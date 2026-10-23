//! The V8 runtime a module carries: V8's version, and where its heap
//! objects hold each field the runtime model reads, with the numbers its
//! frames, tags, and types are told apart by, validated at load. One row,
//! for a module that exports V8's `v8dbg_` constants (see
//! `crate::debug_info::v8`).
//!
//! A layout is the offsets of [`Field`]s and the values of [`Constant`]s,
//! each indexed by its enum, so that no per-version number or name reaches
//! the runtime model.

use zerocopy::little_endian::{I64, U16, U32};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

use super::strings::{StrId, StringsBuilder};
use super::{Builder, Image, NONE, Record, TableKind};

mod program;

#[rustfmt::skip]
#[path = "v8/slices.gen.rs"]
mod slices;

#[cfg(test)]
#[path = "v8/scope_info_tests.rs"]
mod scope_info_tests;

/// Defines an enum of a layout's entries, each with the name V8 gives it.
macro_rules! entries {
    ($(#[$meta:meta])* $kind:ident { $($(#[$doc:meta])* $variant:ident = $name:literal,)* }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        #[repr(u8)]
        pub enum $kind {
            $($(#[$doc])* $variant,)*
        }

        impl $kind {
            /// Every entry, in order.
            pub const ALL: &[Self] = &[$(Self::$variant,)*];
            pub const COUNT: usize = Self::ALL.len();

            /// The entry's name, as V8 spells it.
            #[must_use]
            pub const fn name(self) -> &'static str {
                match self {
                    $(Self::$variant => $name,)*
                }
            }

            /// The entry `name` names.
            #[must_use]
            pub fn named(name: &str) -> Option<Self> {
                Self::ALL.iter().copied().find(|entry| entry.name() == name)
            }

            const fn index(self) -> usize {
                self as usize
            }
        }
    };
}

entries! {
    /// A field of one of V8's heap objects, by its offset in bytes from
    /// the object's untagged address, named `Class::Field` as Torque's
    /// `GetFieldAddress` readers spell it. Where Torque renamed a field,
    /// the entry has the newest name.
    #[expect(clippy::enum_variant_names, reason = "V8 names a map's flags BitField")]
    Field {
        HeapObjectMap = "HeapObject::Map",

        StringLength = "String::Length",
        ConsStringFirst = "ConsString::First",
        ConsStringSecond = "ConsString::Second",
        SlicedStringParent = "SlicedString::Parent",
        SlicedStringOffset = "SlicedString::Offset",
        ThinStringActual = "ThinString::Actual",
        SymbolDescription = "Symbol::Description",
        ExternalStringResource = "ExternalString::Resource",
        ExternalStringResourceData = "ExternalString::ResourceData",
        SeqOneByteStringChars = "SeqOneByteString::Chars",
        SeqTwoByteStringChars = "SeqTwoByteString::Chars",

        ContextLength = "Context::Length",
        ContextElements = "Context::Elements",
        FixedArrayBaseLength = "FixedArrayBase::Length",
        FixedArrayObjects = "FixedArray::Objects",
        TrustedFixedArrayLength = "TrustedFixedArray::Length",
        TrustedFixedArrayObjects = "TrustedFixedArray::Objects",
        ProtectedFixedArrayLength = "ProtectedFixedArray::Length",
        ProtectedFixedArrayObjects = "ProtectedFixedArray::Objects",

        /// A byte array's bytes: `ByteArray::Bytes` in 12.4.
        ByteArrayValues = "ByteArray::Values",
        TrustedByteArrayLength = "TrustedByteArray::Length",

        /// `TrustedByteArray::Bytes` in 12.4.
        TrustedByteArrayValues = "TrustedByteArray::Values",

        MapInstanceSizeInWords = "Map::InstanceSizeInWords",
        MapInobjectPropertiesStartOrConstructorFunctionIndex =
            "Map::InobjectPropertiesStartOrConstructorFunctionIndex",
        MapInstanceType = "Map::InstanceType",
        MapBitField = "Map::BitField",
        MapBitField2 = "Map::BitField2",
        MapBitField3 = "Map::BitField3",
        MapPrototype = "Map::Prototype",
        MapConstructorOrBackPointerOrNativeContext = "Map::ConstructorOrBackPointerOrNativeContext",
        MapInstanceDescriptors = "Map::InstanceDescriptors",

        /// A function's code, through 12.4; from 13.6 a function has its
        /// [`Field::JSFunctionDispatchHandle`] instead.
        JSFunctionCode = "JSFunction::Code",
        JSFunctionDispatchHandle = "JSFunction::DispatchHandle",
        JSFunctionSharedFunctionInfo = "JSFunction::SharedFunctionInfo",
        JSFunctionContext = "JSFunction::Context",
        JSFunctionFeedbackCell = "JSFunction::FeedbackCell",
        FeedbackCellValue = "FeedbackCell::Value",

        /// A function's bytecode or the like, through 12.4; from 13.6 it
        /// is split into the trusted and the untrusted function data.
        SharedFunctionInfoFunctionData = "SharedFunctionInfo::FunctionData",
        SharedFunctionInfoTrustedFunctionData = "SharedFunctionInfo::TrustedFunctionData",
        SharedFunctionInfoUntrustedFunctionData = "SharedFunctionInfo::UntrustedFunctionData",
        SharedFunctionInfoNameOrScopeInfo = "SharedFunctionInfo::NameOrScopeInfo",
        SharedFunctionInfoOuterScopeInfoOrFeedbackMetadata =
            "SharedFunctionInfo::OuterScopeInfoOrFeedbackMetadata",
        SharedFunctionInfoScript = "SharedFunctionInfo::Script",
        SharedFunctionInfoLength = "SharedFunctionInfo::Length",
        SharedFunctionInfoFormalParameterCount = "SharedFunctionInfo::FormalParameterCount",
        SharedFunctionInfoFunctionTokenOffset = "SharedFunctionInfo::FunctionTokenOffset",
        SharedFunctionInfoFlags = "SharedFunctionInfo::Flags",
        SharedFunctionInfoFlags2 = "SharedFunctionInfo::Flags2",
        SharedFunctionInfoFunctionLiteralId = "SharedFunctionInfo::FunctionLiteralId",
        SharedFunctionInfoUniqueId = "SharedFunctionInfo::UniqueId",

        /// From 13.6, the shared function info a trusted object's wrapper
        /// points to.
        SharedFunctionInfoWrapperSharedInfo = "SharedFunctionInfoWrapper::SharedInfo",

        UncompiledDataInferredName = "UncompiledData::InferredName",
        UncompiledDataStartPosition = "UncompiledData::StartPosition",
        UncompiledDataEndPosition = "UncompiledData::EndPosition",
        InterpreterDataBytecodeArray = "InterpreterData::BytecodeArray",
        BytecodeWrapperBytecode = "BytecodeWrapper::Bytecode",

        BytecodeArrayLength = "BytecodeArray::Length",
        BytecodeArrayWrapper = "BytecodeArray::Wrapper",
        BytecodeArraySourcePositionTable = "BytecodeArray::SourcePositionTable",
        BytecodeArrayHandlerTable = "BytecodeArray::HandlerTable",
        BytecodeArrayConstantPool = "BytecodeArray::ConstantPool",
        BytecodeArrayFrameSize = "BytecodeArray::FrameSize",
        BytecodeArrayParameterSize = "BytecodeArray::ParameterSize",
        BytecodeArrayBytes = "BytecodeArray::Bytes",

        ScopeInfoFlags = "ScopeInfo::Flags",
        ScopeInfoParameterCount = "ScopeInfo::ParameterCount",
        ScopeInfoContextLocalCount = "ScopeInfo::ContextLocalCount",

        /// From 13.6; 12.4 keeps a scope's positions among its slices.
        ScopeInfoPositionInfo = "ScopeInfo::PositionInfo",
        ScopeInfoModuleVariableCount = "ScopeInfo::ModuleVariableCount",

        ScriptSource = "Script::Source",
        ScriptName = "Script::Name",
        ScriptLineOffset = "Script::LineOffset",
        ScriptColumnOffset = "Script::ColumnOffset",
        ScriptScriptType = "Script::ScriptType",
        ScriptLineEnds = "Script::LineEnds",
        ScriptId = "Script::Id",
        ScriptEvalFromSharedOrWrappedArguments = "Script::EvalFromSharedOrWrappedArguments",
        ScriptEvalFromPosition = "Script::EvalFromPosition",

        /// The script's functions' shared infos: `Script::SharedFunctionInfos`
        /// in 12.4.
        ScriptInfos = "Script::Infos",
        ScriptFlags = "Script::Flags",
        ScriptSourceUrl = "Script::SourceUrl",
        ScriptSourceMappingUrl = "Script::SourceMappingUrl",

        /// Where a code object's instructions begin and how long they are,
        /// and its flags, which hold its kind. Torque has no readers of
        /// `Code`, so these come from the binary's own `v8dbg_class_Code__`
        /// constants.
        CodeInstructionStart = "Code::InstructionStart",
        CodeInstructionSize = "Code::InstructionSize",
        CodeFlags = "Code::Flags",
    }
}

entries! {
    /// A number of V8's that no object holds: a constant the binary
    /// exports, by its name without `v8dbg_`; an instance type, by its
    /// name in Torque's `instance-types.h`; or where a bit field of a flags
    /// word begins and which bits it covers, as `Class::FieldBits::kShift`
    /// and `::kMask`, from Torque's `bit-fields.h`.
    Constant {
        /// Where a JavaScript frame keeps each of these, from its frame
        /// pointer; negative below it.
        OffFpFunction = "off_fp_function",
        OffFpContext = "off_fp_context",
        OffFpContextOrFrameType = "off_fp_context_or_frame_type",
        OffFpBytecodeArray = "off_fp_bytecode_array",
        OffFpBytecodeOffset = "off_fp_bytecode_offset",
        OffFpArgs = "off_fp_args",
        OffFpConstantPool = "off_fp_constant_pool",

        /// The number of each type of frame, as a typed frame's marker
        /// holds it; a type a version does not have is absent.
        FrameApiAccessorExit = "frametype_ApiAccessorExitFrame",
        FrameApiCallbackExit = "frametype_ApiCallbackExitFrame",
        FrameApiConstructExit = "frametype_ApiConstructExitFrame",
        FrameApiIndexedAccessorExit = "frametype_ApiIndexedAccessorExitFrame",
        FrameApiNamedAccessorExit = "frametype_ApiNamedAccessorExitFrame",
        FrameBaseline = "frametype_BaselineFrame",
        FrameBuiltinContinuation = "frametype_BuiltinContinuationFrame",
        FrameBuiltinExit = "frametype_BuiltinExitFrame",
        FrameBuiltin = "frametype_BuiltinFrame",
        FrameConstructEntry = "frametype_ConstructEntryFrame",
        FrameConstruct = "frametype_ConstructFrame",
        FrameCWasmEntry = "frametype_CWasmEntryFrame",
        FrameEntry = "frametype_EntryFrame",
        FrameExit = "frametype_ExitFrame",
        FrameFastConstruct = "frametype_FastConstructFrame",
        FrameInternal = "frametype_InternalFrame",
        FrameInterpreted = "frametype_InterpretedFrame",
        FrameIrregexp = "frametype_IrregexpFrame",
        FrameJavaScriptBuiltinContinuation = "frametype_JavaScriptBuiltinContinuationFrame",
        FrameJavaScriptBuiltinContinuationWithCatch =
            "frametype_JavaScriptBuiltinContinuationWithCatchFrame",
        FrameJsToWasm = "frametype_JsToWasmFrame",
        FrameMaglev = "frametype_MaglevFrame",
        FrameNative = "frametype_NativeFrame",
        FrameStackSwitch = "frametype_StackSwitchFrame",
        FrameStub = "frametype_StubFrame",

        /// Optimized code's frame: `TurbofanFrame` in 12.4, then
        /// `TurbofanJSFrame`.
        FrameTurbofan = "frametype_TurbofanFrame",
        FrameTurbofanJS = "frametype_TurbofanJSFrame",
        FrameTurbofanStubWithContext = "frametype_TurbofanStubWithContextFrame",
        FrameWasmDebugBreak = "frametype_WasmDebugBreakFrame",
        FrameWasmExit = "frametype_WasmExitFrame",
        FrameWasm = "frametype_WasmFrame",
        FrameWasmJspi = "frametype_WasmJspiFrame",
        FrameWasmLiftoffSetup = "frametype_WasmLiftoffSetupFrame",
        FrameWasmSegmentStart = "frametype_WasmSegmentStartFrame",
        FrameWasmStackEntry = "frametype_WasmStackEntryFrame",
        FrameWasmStackExit = "frametype_WasmStackExitFrame",
        FrameWasmToJs = "frametype_WasmToJsFrame",
        FrameWasmToJsFunction = "frametype_WasmToJsFunctionFrame",

        HeapObjectTag = "HeapObjectTag",
        HeapObjectTagMask = "HeapObjectTagMask",
        SmiTag = "SmiTag",
        SmiTagMask = "SmiTagMask",
        SmiShiftSize = "SmiShiftSize",
        SmiValueShift = "SmiValueShift",
        TaggedSize = "TaggedSize",
        SystemPointerSize = "SystemPointerSize",

        /// How a string's instance type tells its encoding and
        /// representation.
        IsNotStringMask = "IsNotStringMask",
        StringEncodingMask = "StringEncodingMask",
        StringRepresentationMask = "StringRepresentationMask",
        SeqStringTag = "SeqStringTag",
        ConsStringTag = "ConsStringTag",
        SlicedStringTag = "SlicedStringTag",
        ThinStringTag = "ThinStringTag",
        ExternalStringTag = "ExternalStringTag",
        OneByteStringTag = "OneByteStringTag",
        TwoByteStringTag = "TwoByteStringTag",
        FirstNonstringType = "FirstNonstringType",

        FirstJSFunctionType = "FirstJSFunctionType",
        LastJSFunctionType = "LastJSFunctionType",
        FirstContextType = "FirstContextType",
        LastContextType = "LastContextType",

        /// The kinds of an oddball, as its `kind` holds them.
        OddballUndefined = "OddballUndefined",
        OddballNull = "OddballNull",
        OddballTrue = "OddballTrue",
        OddballFalse = "OddballFalse",

        /// Where a code object's [`Field::CodeFlags`] keep its kind, and
        /// the kinds a JavaScript frame's code may be.
        CodeKindFieldMask = "CodeKindFieldMask",
        CodeKindFieldShift = "CodeKindFieldShift",
        CodeKindInterpretedFunction = "CodeKindInterpretedFunction",
        CodeKindBaseline = "CodeKindBaseline",
        CodeKindBytecodeHandler = "CodeKindBytecodeHandler",

        /// The slots of optimized code's deoptimization data; the shared
        /// info's is `SharedFunctionInfoIndex` in 12.4, then
        /// `WrappedSharedFunctionInfoIndex`.
        DeoptimizationDataInlinedFunctionCountIndex = "DeoptimizationDataInlinedFunctionCountIndex",
        DeoptimizationDataInliningPositionsIndex = "DeoptimizationDataInliningPositionsIndex",
        DeoptimizationDataLiteralArrayIndex = "DeoptimizationDataLiteralArrayIndex",
        DeoptimizationDataOptimizationIdIndex = "DeoptimizationDataOptimizationIdIndex",
        DeoptimizationDataSharedFunctionInfoIndex = "DeoptimizationDataSharedFunctionInfoIndex",
        DeoptimizationDataWrappedSharedFunctionInfoIndex =
            "DeoptimizationDataWrappedSharedFunctionInfoIndex",

        /// A context's slots: its previous context and its scope info,
        /// and how many slots every context has.
        ContextIdxPrev = "context_idx_prev",
        ContextIdxScopeInfo = "context_idx_scope_info",
        ContextMinSlots = "context_min_slots",
        ScopeInfoIdxFirstVars = "scopeinfo_idx_first_vars",
        ScopeInfoIdxNcontextlocals = "scopeinfo_idx_ncontextlocals",
        ScopeInfoIdxNparams = "scopeinfo_idx_nparams",

        ScriptType = "SCRIPT_TYPE",
        SharedFunctionInfoType = "SHARED_FUNCTION_INFO_TYPE",

        /// From 13.6.
        SharedFunctionInfoWrapperType = "SHARED_FUNCTION_INFO_WRAPPER_TYPE",
        BytecodeArrayType = "BYTECODE_ARRAY_TYPE",
        BytecodeWrapperType = "BYTECODE_WRAPPER_TYPE",
        ScopeInfoType = "SCOPE_INFO_TYPE",
        CodeType = "CODE_TYPE",
        InterpreterDataType = "INTERPRETER_DATA_TYPE",
        UncompiledDataWithPreparseDataType = "UNCOMPILED_DATA_WITH_PREPARSE_DATA_TYPE",
        UncompiledDataWithoutPreparseDataType = "UNCOMPILED_DATA_WITHOUT_PREPARSE_DATA_TYPE",
        UncompiledDataWithPreparseDataAndJobType =
            "UNCOMPILED_DATA_WITH_PREPARSE_DATA_AND_JOB_TYPE",
        UncompiledDataWithoutPreparseDataWithJobType =
            "UNCOMPILED_DATA_WITHOUT_PREPARSE_DATA_WITH_JOB_TYPE",
        FixedArrayType = "FIXED_ARRAY_TYPE",
        TrustedFixedArrayType = "TRUSTED_FIXED_ARRAY_TYPE",
        ProtectedFixedArrayType = "PROTECTED_FIXED_ARRAY_TYPE",
        ByteArrayType = "BYTE_ARRAY_TYPE",
        TrustedByteArrayType = "TRUSTED_BYTE_ARRAY_TYPE",
        OddballType = "ODDBALL_TYPE",
        HeapNumberType = "HEAP_NUMBER_TYPE",
        SymbolType = "SYMBOL_TYPE",
        NativeContextType = "NATIVE_CONTEXT_TYPE",
        FeedbackCellType = "FEEDBACK_CELL_TYPE",

        /// The bit fields of [`Field::SharedFunctionInfoFlags`].
        SfiFunctionKindShift = "SharedFunctionInfo::FunctionKindBits::kShift",
        SfiFunctionKindMask = "SharedFunctionInfo::FunctionKindBits::kMask",
        SfiIsNativeShift = "SharedFunctionInfo::IsNativeBit::kShift",
        SfiIsNativeMask = "SharedFunctionInfo::IsNativeBit::kMask",
        SfiIsStrictShift = "SharedFunctionInfo::IsStrictBit::kShift",
        SfiIsStrictMask = "SharedFunctionInfo::IsStrictBit::kMask",
        SfiFunctionSyntaxKindShift = "SharedFunctionInfo::FunctionSyntaxKindBits::kShift",
        SfiFunctionSyntaxKindMask = "SharedFunctionInfo::FunctionSyntaxKindBits::kMask",
        SfiIsClassConstructorShift = "SharedFunctionInfo::IsClassConstructorBit::kShift",
        SfiIsClassConstructorMask = "SharedFunctionInfo::IsClassConstructorBit::kMask",
        SfiHasDuplicateParametersShift = "SharedFunctionInfo::HasDuplicateParametersBit::kShift",
        SfiHasDuplicateParametersMask = "SharedFunctionInfo::HasDuplicateParametersBit::kMask",
        SfiAllowLazyCompilationShift = "SharedFunctionInfo::AllowLazyCompilationBit::kShift",
        SfiAllowLazyCompilationMask = "SharedFunctionInfo::AllowLazyCompilationBit::kMask",
        SfiIsAsmWasmBrokenShift = "SharedFunctionInfo::IsAsmWasmBrokenBit::kShift",
        SfiIsAsmWasmBrokenMask = "SharedFunctionInfo::IsAsmWasmBrokenBit::kMask",
        SfiFunctionMapIndexShift = "SharedFunctionInfo::FunctionMapIndexBits::kShift",
        SfiFunctionMapIndexMask = "SharedFunctionInfo::FunctionMapIndexBits::kMask",
        SfiDisabledOptimizationReasonShift =
            "SharedFunctionInfo::DisabledOptimizationReasonBits::kShift",
        SfiDisabledOptimizationReasonMask =
            "SharedFunctionInfo::DisabledOptimizationReasonBits::kMask",
        SfiRequiresInstanceMembersInitializerShift =
            "SharedFunctionInfo::RequiresInstanceMembersInitializerBit::kShift",
        SfiRequiresInstanceMembersInitializerMask =
            "SharedFunctionInfo::RequiresInstanceMembersInitializerBit::kMask",
        SfiConstructAsBuiltinShift = "SharedFunctionInfo::ConstructAsBuiltinBit::kShift",
        SfiConstructAsBuiltinMask = "SharedFunctionInfo::ConstructAsBuiltinBit::kMask",
        SfiNameShouldPrintAsAnonymousShift =
            "SharedFunctionInfo::NameShouldPrintAsAnonymousBit::kShift",
        SfiNameShouldPrintAsAnonymousMask =
            "SharedFunctionInfo::NameShouldPrintAsAnonymousBit::kMask",
        SfiHasReportedBinaryCoverageShift =
            "SharedFunctionInfo::HasReportedBinaryCoverageBit::kShift",
        SfiHasReportedBinaryCoverageMask =
            "SharedFunctionInfo::HasReportedBinaryCoverageBit::kMask",
        SfiIsTopLevelShift = "SharedFunctionInfo::IsTopLevelBit::kShift",
        SfiIsTopLevelMask = "SharedFunctionInfo::IsTopLevelBit::kMask",
        SfiPropertiesAreFinalShift = "SharedFunctionInfo::PropertiesAreFinalBit::kShift",
        SfiPropertiesAreFinalMask = "SharedFunctionInfo::PropertiesAreFinalBit::kMask",
        SfiPrivateNameLookupSkipsOuterClassShift =
            "SharedFunctionInfo::PrivateNameLookupSkipsOuterClassBit::kShift",
        SfiPrivateNameLookupSkipsOuterClassMask =
            "SharedFunctionInfo::PrivateNameLookupSkipsOuterClassBit::kMask",

        /// From 13.6.
        SfiLiveEditedShift = "SharedFunctionInfo::LiveEditedBit::kShift",
        SfiLiveEditedMask = "SharedFunctionInfo::LiveEditedBit::kMask",

        /// The bit fields of [`Field::ScopeInfoFlags`]; those only some
        /// versions have are absent from the others.
        ScopeTypeShift = "ScopeInfo::ScopeTypeBits::kShift",
        ScopeTypeMask = "ScopeInfo::ScopeTypeBits::kMask",
        ScopeSloppyEvalCanExtendVarsShift = "ScopeInfo::SloppyEvalCanExtendVarsBit::kShift",
        ScopeSloppyEvalCanExtendVarsMask = "ScopeInfo::SloppyEvalCanExtendVarsBit::kMask",
        ScopeLanguageModeShift = "ScopeInfo::LanguageModeBit::kShift",
        ScopeLanguageModeMask = "ScopeInfo::LanguageModeBit::kMask",
        ScopeDeclarationScopeShift = "ScopeInfo::DeclarationScopeBit::kShift",
        ScopeDeclarationScopeMask = "ScopeInfo::DeclarationScopeBit::kMask",
        ScopeReceiverVariableShift = "ScopeInfo::ReceiverVariableBits::kShift",
        ScopeReceiverVariableMask = "ScopeInfo::ReceiverVariableBits::kMask",
        ScopeClassScopeHasPrivateBrandShift = "ScopeInfo::ClassScopeHasPrivateBrandBit::kShift",
        ScopeClassScopeHasPrivateBrandMask = "ScopeInfo::ClassScopeHasPrivateBrandBit::kMask",
        ScopeHasSavedClassVariableShift = "ScopeInfo::HasSavedClassVariableBit::kShift",
        ScopeHasSavedClassVariableMask = "ScopeInfo::HasSavedClassVariableBit::kMask",

        /// Through 13.6.
        ScopeHasNewTargetShift = "ScopeInfo::HasNewTargetBit::kShift",
        ScopeHasNewTargetMask = "ScopeInfo::HasNewTargetBit::kMask",

        /// From 14.6.
        ScopeAllocatesArgumentsShift = "ScopeInfo::AllocatesArgumentsBit::kShift",
        ScopeAllocatesArgumentsMask = "ScopeInfo::AllocatesArgumentsBit::kMask",
        ScopeFunctionVariableShift = "ScopeInfo::FunctionVariableBits::kShift",
        ScopeFunctionVariableMask = "ScopeInfo::FunctionVariableBits::kMask",
        ScopeHasInferredFunctionNameShift = "ScopeInfo::HasInferredFunctionNameBit::kShift",
        ScopeHasInferredFunctionNameMask = "ScopeInfo::HasInferredFunctionNameBit::kMask",
        ScopeIsAsmModuleShift = "ScopeInfo::IsAsmModuleBit::kShift",
        ScopeIsAsmModuleMask = "ScopeInfo::IsAsmModuleBit::kMask",
        ScopeHasSimpleParametersShift = "ScopeInfo::HasSimpleParametersBit::kShift",
        ScopeHasSimpleParametersMask = "ScopeInfo::HasSimpleParametersBit::kMask",
        ScopeFunctionKindShift = "ScopeInfo::FunctionKindBits::kShift",
        ScopeFunctionKindMask = "ScopeInfo::FunctionKindBits::kMask",
        ScopeHasOuterScopeInfoShift = "ScopeInfo::HasOuterScopeInfoBit::kShift",
        ScopeHasOuterScopeInfoMask = "ScopeInfo::HasOuterScopeInfoBit::kMask",
        ScopeIsDebugEvaluateScopeShift = "ScopeInfo::IsDebugEvaluateScopeBit::kShift",
        ScopeIsDebugEvaluateScopeMask = "ScopeInfo::IsDebugEvaluateScopeBit::kMask",
        ScopeForceContextAllocationShift = "ScopeInfo::ForceContextAllocationBit::kShift",
        ScopeForceContextAllocationMask = "ScopeInfo::ForceContextAllocationBit::kMask",
        ScopePrivateNameLookupSkipsOuterClassShift =
            "ScopeInfo::PrivateNameLookupSkipsOuterClassBit::kShift",
        ScopePrivateNameLookupSkipsOuterClassMask =
            "ScopeInfo::PrivateNameLookupSkipsOuterClassBit::kMask",
        ScopeHasContextExtensionSlotShift = "ScopeInfo::HasContextExtensionSlotBit::kShift",
        ScopeHasContextExtensionSlotMask = "ScopeInfo::HasContextExtensionSlotBit::kMask",

        /// 12.4 only.
        ScopeIsReplModeScopeShift = "ScopeInfo::IsReplModeScopeBit::kShift",
        ScopeIsReplModeScopeMask = "ScopeInfo::IsReplModeScopeBit::kMask",
        ScopeHasLocalsBlockListShift = "ScopeInfo::HasLocalsBlockListBit::kShift",
        ScopeHasLocalsBlockListMask = "ScopeInfo::HasLocalsBlockListBit::kMask",

        /// From 13.6.
        ScopeSomeContextHasExtensionShift = "ScopeInfo::SomeContextHasExtensionBit::kShift",
        ScopeSomeContextHasExtensionMask = "ScopeInfo::SomeContextHasExtensionBit::kMask",
        ScopeIsHiddenShift = "ScopeInfo::IsHiddenBit::kShift",
        ScopeIsHiddenMask = "ScopeInfo::IsHiddenBit::kMask",
        ScopeIsEmptyShift = "ScopeInfo::IsEmptyBit::kShift",
        ScopeIsEmptyMask = "ScopeInfo::IsEmptyBit::kMask",

        /// From 13.6.
        ScopeIsWrappedFunctionShift = "ScopeInfo::IsWrappedFunctionBit::kShift",
        ScopeIsWrappedFunctionMask = "ScopeInfo::IsWrappedFunctionBit::kMask",

        /// From 14.6.
        ScopeHasContextCellsShift = "ScopeInfo::HasContextCellsBit::kShift",
        ScopeHasContextCellsMask = "ScopeInfo::HasContextCellsBit::kMask",

        /// The bit fields of [`Field::ScriptFlags`].
        ScriptCompilationTypeShift = "Script::CompilationTypeBit::kShift",
        ScriptCompilationTypeMask = "Script::CompilationTypeBit::kMask",
        ScriptCompilationStateShift = "Script::CompilationStateBit::kShift",
        ScriptCompilationStateMask = "Script::CompilationStateBit::kMask",
        ScriptIsReplModeShift = "Script::IsReplModeBit::kShift",
        ScriptIsReplModeMask = "Script::IsReplModeBit::kMask",
        ScriptOriginOptionsShift = "Script::OriginOptionsBits::kShift",
        ScriptOriginOptionsMask = "Script::OriginOptionsBits::kMask",
        ScriptBreakOnEntryShift = "Script::BreakOnEntryBit::kShift",
        ScriptBreakOnEntryMask = "Script::BreakOnEntryBit::kMask",
        ScriptProduceCompileHintsShift = "Script::ProduceCompileHintsBit::kShift",
        ScriptProduceCompileHintsMask = "Script::ProduceCompileHintsBit::kMask",
        ScriptDeserializedShift = "Script::DeserializedBit::kShift",
        ScriptDeserializedMask = "Script::DeserializedBit::kMask",
    }
}

impl Constant {
    /// Whether the constant comes from Torque's output, an instance type
    /// or a bit field, rather than from the binary's `v8dbg_` constants.
    #[must_use]
    pub fn is_torque(self) -> bool {
        let name = self.name();
        name.ends_with("_TYPE") || name.contains("::")
    }
}

/// An offset no field has: the version does not have the field.
const ABSENT: u32 = u32::MAX;

/// A value no constant has: the version does not have the constant.
const ABSENT_CONSTANT: i64 = i64::MIN;

/// The module names its `v8dbg_` constants only in its dynamic symbol
/// table, as a shared libnode stripped of its local symbols does, so no
/// symbol names its local variables.
pub const DYNAMIC_SYMBOLS: u16 = 1;

/// The V8 runtime a module carries.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct V8RuntimeRecord {
    /// `v8::internal::Version`'s `major_`, `minor_`, `build_`, and
    /// `patch_`.
    pub major: U32,
    pub minor: U32,
    pub build: U32,
    pub patch: U32,
    pub flags: U16,
    pub reserved: U16,

    /// Why the module's runtime cannot be read, a string, or [`NONE`]
    /// when its layout is complete.
    pub problem: U32,
    pub offsets: [U32; Field::COUNT],
    pub constants: [I64; Constant::COUNT],
}

impl Record for V8RuntimeRecord {
    const KIND: TableKind = TableKind::V8Runtime;
}

/// A V8 version, as `v8::internal::Version` holds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct V8Version {
    pub major: u32,
    pub minor: u32,
    pub build: u32,
    pub patch: u32,
}

impl std::fmt::Display for V8Version {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{}.{}.{}.{}",
            self.major, self.minor, self.build, self.patch
        )
    }
}

/// Where each field is, and each constant's value, for one build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct V8Layout {
    offsets: [u32; Field::COUNT],
    constants: [i64; Constant::COUNT],
}

impl Default for V8Layout {
    fn default() -> Self {
        Self {
            offsets: [ABSENT; Field::COUNT],
            constants: [ABSENT_CONSTANT; Constant::COUNT],
        }
    }
}

impl V8Layout {
    /// The field's offset; `None` when the version does not have it.
    #[must_use]
    pub const fn field(&self, field: Field) -> Option<u64> {
        match self.offsets[field.index()] {
            ABSENT => None,
            offset => Some(offset as u64),
        }
    }

    /// The constant's value; `None` when the version does not have it.
    #[must_use]
    pub const fn constant(&self, constant: Constant) -> Option<i64> {
        match self.constants[constant.index()] {
            ABSENT_CONSTANT => None,
            value => Some(value),
        }
    }

    /// Sets a field, or leaves it absent when its offset does not fit.
    pub fn set_field(&mut self, field: Field, offset: u64) {
        self.offsets[field.index()] = u32::try_from(offset)
            .ok()
            .filter(|offset| *offset != ABSENT)
            .unwrap_or(ABSENT);
    }

    /// Sets a constant; `i64::MIN` reads back as absent.
    pub const fn set_constant(&mut self, constant: Constant, value: i64) {
        self.constants[constant.index()] = value;
    }

    #[must_use]
    pub const fn has(&self, field: Field) -> bool {
        self.field(field).is_some()
    }

    #[must_use]
    pub const fn has_constant(&self, constant: Constant) -> bool {
        self.constant(constant).is_some()
    }
}

/// What a module's loader found of its V8 runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct V8Runtime {
    pub version: V8Version,
    pub flags: u16,

    /// The layout, or why the runtime cannot be read, as for a version
    /// microscope has no layout of.
    pub layout: Result<V8Layout, std::sync::Arc<str>>,
}

/// Why a runtime could not be encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the V8 runtime does not fit an image")]
pub struct TooLarge;

/// Adds the module's V8 runtime, if it has one, to `builder`.
pub fn add_to(
    builder: &mut Builder<'_>,
    strings: &mut StringsBuilder,
    runtime: Option<&V8Runtime>,
) -> Result<(), TooLarge> {
    let records = runtime
        .map(|runtime| {
            let (layout, problem) = match &runtime.layout {
                Ok(layout) => (layout.clone(), NONE),
                Err(problem) => (
                    V8Layout::default(),
                    strings.push(problem).ok_or(TooLarge)?.0,
                ),
            };

            Ok(V8RuntimeRecord {
                major: runtime.version.major.into(),
                minor: runtime.version.minor.into(),
                build: runtime.version.build.into(),
                patch: runtime.version.patch.into(),
                flags: runtime.flags.into(),
                reserved: 0.into(),
                problem: problem.into(),
                offsets: layout.offsets.map(U32::new),
                constants: layout.constants.map(I64::new),
            })
        })
        .transpose()?;

    builder.owned_table(records.into_iter().collect());
    Ok(())
}

/// The V8 runtime of a validated image, if it has one.
#[must_use]
pub fn runtime(image: &Image) -> Option<V8Runtime> {
    let record: &V8RuntimeRecord = image.table().first()?;
    let problem = record.problem.get();
    Some(V8Runtime {
        version: V8Version {
            major: record.major.get(),
            minor: record.minor.get(),
            build: record.build.get(),
            patch: record.patch.get(),
        },
        flags: record.flags.get(),
        layout: if problem == NONE {
            Ok(V8Layout {
                offsets: record.offsets.map(U32::get),
                constants: record.constants.map(I64::get),
            })
        } else {
            Err(image.strings().get(StrId(problem)).into())
        },
    })
}

/// Checks that an image has at most one runtime, which sets only the
/// flags this format defines, and whose problem names a string.
pub(super) fn validate(image: &Image) -> Result<(), String> {
    let records: &[V8RuntimeRecord] = image.table();
    if records.len() > 1 {
        return Err(format!("{} V8 runtimes in one image", records.len()));
    }

    let Some(record) = records.first() else {
        return Ok(());
    };

    if record.flags.get() & !DYNAMIC_SYMBOLS != 0 || record.reserved.get() != 0 {
        return Err("the V8 runtime sets flags it does not define".to_owned());
    }

    let problem = record.problem.get();
    if problem != NONE && !image.strings().contains(StrId(problem)) {
        return Err(format!("the V8 runtime's problem {problem} is no string"));
    }

    Ok(())
}

/// A part of a `ScopeInfo` whose place and length depend on the scope's
/// flags and counts, by the name Torque gives its field. A version that
/// does not have the part gives no [`Slice`] of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ScopeInfoSlice {
    ModuleVariableCount,
    ContextLocalNames,
    ContextLocalNamesHashtable,
    ContextLocalInfos,
    SavedClassVariableInfo,
    FunctionVariableInfo,
    InferredFunctionName,

    /// 12.4 only: from 13.6 a scope's positions are a fixed field.
    PositionInfo,
    OuterScopeInfo,

    /// 12.4 only.
    LocalsBlockList,
    ModuleInfo,
    ModuleVariables,

    /// From 13.6.
    DependentCode,

    /// From 14.6.
    UnusedParameterBits,
}

impl ScopeInfoSlice {
    /// Every slice, in the order a `ScopeInfo` holds those its version has.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the translator's tests name slices by it")
    )]
    pub const ALL: &[Self] = &[
        Self::ModuleVariableCount,
        Self::ContextLocalNames,
        Self::ContextLocalNamesHashtable,
        Self::ContextLocalInfos,
        Self::SavedClassVariableInfo,
        Self::FunctionVariableInfo,
        Self::InferredFunctionName,
        Self::PositionInfo,
        Self::OuterScopeInfo,
        Self::LocalsBlockList,
        Self::ModuleInfo,
        Self::ModuleVariables,
        Self::DependentCode,
        Self::UnusedParameterBits,
    ];
}

/// Where one object's slice is: its offset in bytes from the object's
/// untagged start, and how many items it holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slice {
    pub offset: u64,
    pub length: u64,
}

/// Where `slice` is in one `ScopeInfo` of V8 `version`, which has 8-byte
/// tagged words and 64-bit Smis, as the layouts require: by V8's own
/// `debug-macros.cc`, translated for each version. `read` gives the
/// object's little-endian word at a byte offset from its untagged start,
/// a multiple of 8, or `None` when it cannot be read.
///
/// `Ok(None)` when the version has no such slice. An error when the
/// version has no translation, a word cannot be read, or one is not what
/// V8 would have written there, such as a count that is no Smi.
pub fn scope_info_slice(
    version: V8Version,
    slice: ScopeInfoSlice,
    read: &mut dyn FnMut(u64) -> Option<u64>,
) -> Result<Option<Slice>, std::sync::Arc<str>> {
    let translated = slices::TRANSLATED
        .iter()
        .find(|translated| (translated.major, translated.minor) == (version.major, version.minor))
        .ok_or_else(|| -> std::sync::Arc<str> {
            format!(
                "V8 {}.{} has no ScopeInfo slices in this build of microscope",
                version.major, version.minor
            )
            .into()
        })?;
    let Some(&(_, function)) = translated.scope_info.iter().find(|(own, _)| *own == slice) else {
        return Ok(None);
    };

    let fail = |error: String| -> std::sync::Arc<str> {
        format!("V8 {version}'s ScopeInfo {slice:?}: {error}").into()
    };
    let values =
        program::run(translated.functions, function, &[program::OBJECT], read).map_err(fail)?;
    let &[object, offset, length] = values.as_slice() else {
        return Err(fail(format!("{} values for a slice", values.len())));
    };

    if object != program::OBJECT {
        return Err(fail("a slice of another object".to_owned()));
    }

    let unsigned = |value: i64| u64::try_from(value).map_err(|_| fail(format!("{value}")));
    Ok(Some(Slice {
        offset: unsigned(offset)?,
        length: unsigned(length)?,
    }))
}
