//! The V8 runtime a module carries: V8's version, and where its heap
//! objects hold each field the runtime model reads, with the numbers its
//! frames, tags, and types are told apart by, validated at load. One row,
//! for a module that exports V8's `v8dbg_` constants (see
//! `crate::debug_info::v8`).
//!
//! A layout is the offsets of [`Field`]s and the values of [`Constant`]s,
//! each indexed by its enum, so that no per-version number or name reaches
//! the runtime model.

use zerocopy::little_endian::{I64, U16, U32, U64};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

use super::strings::{StrId, StringsBuilder};
use super::{Builder, Image, NONE, Record, TableKind};
use crate::ImageAddress;

mod program;
mod translation;

pub use translation::{TranslationOpcode, Translations};

#[rustfmt::skip]
#[path = "v8/slices.gen.rs"]
mod slices;

#[rustfmt::skip]
#[path = "v8/bytecodes.gen.rs"]
mod bytecodes;

/// V8 `version`'s `Bytecodes::kBytecodeSizes`, generated from its headers:
/// three rows, at the operand scales 1, 2, and 4, of one size per
/// bytecode; or why microscope has none.
pub fn bytecode_sizes(version: V8Version) -> Result<&'static [u8], std::sync::Arc<str>> {
    bytecodes::BYTECODE_SIZES
        .iter()
        .find(|(major, minor, _)| (*major, *minor) == (version.major, version.minor))
        .map(|(_, _, sizes)| *sizes)
        .ok_or_else(|| {
            format!(
                "V8 {}.{} has no bytecode sizes in this build of microscope",
                version.major, version.minor
            )
            .into()
        })
}

#[cfg(test)]
#[path = "v8/scope_info_tests.rs"]
mod scope_info_tests;

/// Defines an enum of a layout's entries, each with the name V8 gives it.
macro_rules! entries {
    ($(#[$meta:meta])* $kind:ident { $($(#[$doc:meta])* $variant:ident = $name:literal,)* }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        #[repr(u16)]
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
        MapUsedOrUnusedInstanceSizeInWords = "Map::UsedOrUnusedInstanceSizeInWords",

        /// An object's out-of-object properties: a `PropertyArray`, a
        /// dictionary when its map is a dictionary map, or its hash.
        JSReceiverPropertiesOrHash = "JSReceiver::PropertiesOrHash",
        JSObjectElements = "JSObject::Elements",
        NameRawHashField = "Name::RawHashField",

        /// A map's own descriptors, each `prop_desc_size` words from
        /// `Descriptors`: its key, its details, and its value.
        DescriptorArrayNumberOfAllDescriptors = "DescriptorArray::NumberOfAllDescriptors",
        DescriptorArrayNumberOfDescriptors = "DescriptorArray::NumberOfDescriptors",
        DescriptorArrayDescriptors = "DescriptorArray::Descriptors",
        PropertyArrayLengthAndHash = "PropertyArray::LengthAndHash",
        PropertyArrayObjects = "PropertyArray::Objects",
        AccessorInfoName = "AccessorInfo::Name",

        /// The name an API constructor's template gives its instances, a
        /// string or undefined.
        FunctionTemplateInfoClassName = "FunctionTemplateInfo::ClassName",
        SwissNameDictionaryHash = "SwissNameDictionary::Hash",
        SwissNameDictionaryCapacity = "SwissNameDictionary::Capacity",
        SwissNameDictionaryMetaTable = "SwissNameDictionary::MetaTable",
        SwissNameDictionaryDataTable = "SwissNameDictionary::DataTable",
        WeakFixedArrayLength = "WeakFixedArray::Length",
        WeakFixedArrayObjects = "WeakFixedArray::Objects",

        /// From 13.6.
        TrustedWeakFixedArrayLength = "TrustedWeakFixedArray::Length",
        TrustedWeakFixedArrayObjects = "TrustedWeakFixedArray::Objects",

        /// A function's code, through 12.4; from 13.6 a function has its
        /// [`Field::JSFunctionDispatchHandle`] instead.
        JSFunctionCode = "JSFunction::Code",
        JSFunctionDispatchHandle = "JSFunction::DispatchHandle",
        JSFunctionSharedFunctionInfo = "JSFunction::SharedFunctionInfo",
        JSFunctionContext = "JSFunction::Context",
        JSFunctionFeedbackCell = "JSFunction::FeedbackCell",
        FeedbackCellValue = "FeedbackCell::Value",

        /// From 13.6.
        FeedbackCellDispatchHandle = "FeedbackCell::DispatchHandle",

        /// A function's feedback: `Length` slots from `RawFeedbackSlots`,
        /// among them, as weak references to `CodeWrapper`s, the code its
        /// loops entered on stack replacement (OSR), which `OsrState`'s
        /// bits say may be cached.
        FeedbackVectorLength = "FeedbackVector::Length",
        FeedbackVectorOsrState = "FeedbackVector::OsrState",
        FeedbackVectorFlags = "FeedbackVector::Flags",

        /// 12.4 only.
        FeedbackVectorMaybeOptimizedCode = "FeedbackVector::MaybeOptimizedCode",
        FeedbackVectorRawFeedbackSlots = "FeedbackVector::RawFeedbackSlots",

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

        /// A code object's fields, from `CODE_DATA_FIELDS` in `code.h`,
        /// which Torque has no readers of, as the release configuration
        /// lays them out. Optimized code's deoptimization data, or a
        /// baseline code's bytecode or interpreter data.
        CodeDeoptimizationDataOrInterpreterData = "Code::DeoptimizationDataOrInterpreterData",

        /// The source position table, or a baseline code's bytecode offset
        /// table; `Smi::zero()` for an embedded builtin's.
        CodePositionTable = "Code::PositionTable",
        CodeWrapper = "Code::Wrapper",
        CodeInstructionStream = "Code::InstructionStream",

        /// Where a code object's instructions begin and how long they are,
        /// and its flags, which hold its kind.
        CodeInstructionStart = "Code::InstructionStart",

        /// From 13.6.
        CodeDispatchHandle = "Code::DispatchHandle",
        CodeFlags = "Code::Flags",
        CodeInstructionSize = "Code::InstructionSize",
        CodeMetadataSize = "Code::MetadataSize",
        CodeInlinedBytecodeSize = "Code::InlinedBytecodeSize",
        CodeOsrOffset = "Code::OsrOffset",
        CodeHandlerTableOffset = "Code::HandlerTableOffset",

        /// From 13.6.
        CodeParameterCount = "Code::ParameterCount",

        /// The builtin the code is, a 16-bit number; -1 for none.
        CodeBuiltinId = "Code::BuiltinId",

        /// The code a wrapper points to, as an OSR slot's weak reference
        /// does: from `CodeWrapper`'s `FIELD_LIST` in `code.h`.
        CodeWrapperCode = "CodeWrapper::Code",

        /// A heap number's double.
        HeapNumberValue = "HeapNumber::Value",

        /// An oddball's kind, one of the `Oddball::k` constants.
        OddballKind = "Oddball::Kind",

        /// A symbol's flags, its `Symbol::` bit fields.
        SymbolFlags = "Symbol::Flags",

        /// A `BigInt`'s 32-bit bit field, its sign and its length in digits
        /// (`BigIntBase::SignBits` and `LengthBits`), and its 64-bit
        /// digits, least significant first; from `bigint.h`, which Torque
        /// has no readers of.
        BigIntBaseBitfield = "BigIntBase::Bitfield",
        BigIntBaseDigits = "BigIntBase::Digits",

        JSArrayLength = "JSArray::Length",

        /// A double array's doubles: `FixedDoubleArray::Floats` in 12.4.
        FixedDoubleArrayValues = "FixedDoubleArray::Values",

        JSDateValue = "JSDate::Value",
        JSRegExpSource = "JSRegExp::Source",
        JSRegExpFlags = "JSRegExp::Flags",
        JSPromiseReactionsOrResult = "JSPromise::ReactionsOrResult",
        JSPromiseFlags = "JSPromise::Flags",
        JSTypedArrayExternalPointer = "JSTypedArray::ExternalPointer",
        JSTypedArrayBasePointer = "JSTypedArray::BasePointer",
        JSArrayBufferViewBuffer = "JSArrayBufferView::Buffer",
        JSArrayBufferViewBitField = "JSArrayBufferView::BitField",
        JSArrayBufferViewRawByteLength = "JSArrayBufferView::RawByteLength",
        JSArrayBufferViewRawByteOffset = "JSArrayBufferView::RawByteOffset",
        JSArrayBufferBackingStore = "JSArrayBuffer::BackingStore",
        JSArrayBufferBitField = "JSArrayBuffer::BitField",
        JSArrayBufferRawByteLength = "JSArrayBuffer::RawByteLength",
        JSArrayBufferRawMaxByteLength = "JSArrayBuffer::RawMaxByteLength",
        JSPrimitiveWrapperValue = "JSPrimitiveWrapper::Value",
        JSProxyTarget = "JSProxy::Target",
        JSProxyHandler = "JSProxy::Handler",
        JSBoundFunctionBoundTargetFunction = "JSBoundFunction::BoundTargetFunction",
        JSBoundFunctionBoundThis = "JSBoundFunction::BoundThis",
        JSBoundFunctionBoundArguments = "JSBoundFunction::BoundArguments",
        JSCollectionTable = "JSCollection::Table",
        JSWeakCollectionTable = "JSWeakCollection::Table",
        JSFunctionPrototypeOrInitialMap = "JSFunction::PrototypeOrInitialMap",
        JSGlobalObjectGlobalProxy = "JSGlobalObject::GlobalProxy",

        /// Through 13.6: a `FixedArray` of `CallSiteInfo`s, or the
        /// formatted stack once `error.stack` was read.
        ErrorStackDataCallSiteInfosOrFormattedStack =
            "ErrorStackData::CallSiteInfosOrFormattedStack",

        /// In 12.4, a nonnegative Smi caps the application's stack;
        /// a negative Smi caps only the inspector's stack. A `FixedArray`
        /// means V8 has already applied the application's limit.
        ErrorStackDataLimitOrStackFrameInfos = "ErrorStackData::LimitOrStackFrameInfos",

        /// From 14.6, in its place (a change Node makes to V8): the
        /// formatted stack, any value but a `FixedArray`, or a `FixedArray`
        /// of the raw data `CallSiteInfo`s are made from
        /// (`debug-objects.tq`). Frame `i` is the
        /// [`Constant::CallSiteInfoRawCount`] elements from
        /// `i * CallSiteInfoRawCount`, and the array ends with the last
        /// frame. By `CallSiteInfo::Fields` index, a frame holds its code (a
        /// `CodeWrapper`, a `BytecodeWrapper`, or undefined), receiver,
        /// function, offset (a Smi), parameters, and last its `CallSiteInfo`
        /// flags (a Smi). With `IsDeferredBaselineFrameBit` set, the code is
        /// baseline code's wrapper and the offset is a pc offset into its
        /// instructions rather than a bytecode offset.
        ErrorStackDataRawDataForCallSiteInfosOrFormattedStack =
            "ErrorStackData::RawDataForCallSiteInfosOrFormattedStack",

        CallSiteInfoReceiverOrInstance = "CallSiteInfo::ReceiverOrInstance",
        CallSiteInfoFunction = "CallSiteInfo::Function",
        CallSiteInfoCodeObject = "CallSiteInfo::CodeObject",
        CallSiteInfoCodeOffsetOrSourcePosition = "CallSiteInfo::CodeOffsetOrSourcePosition",
        CallSiteInfoFlags = "CallSiteInfo::Flags",
        CallSiteInfoParameters = "CallSiteInfo::Parameters",
        ScriptContextTableLength = "ScriptContextTable::Length",
        ScriptContextTableCapacity = "ScriptContextTable::Capacity",
        ScriptContextTableNamesToContextIndex = "ScriptContextTable::NamesToContextIndex",
        ScriptContextTableObjects = "ScriptContextTable::Objects",
        PropertyCellName = "PropertyCell::Name",
        PropertyCellValue = "PropertyCell::Value",
        PropertyCellPropertyDetailsRaw = "PropertyCell::PropertyDetailsRaw",

        /// An accessor pair's getter and setter; 14.6's from `struct.h`,
        /// which Torque has no readers of.
        AccessorPairGetter = "AccessorPair::Getter",
        AccessorPairSetter = "AccessorPair::Setter",

        JSGeneratorObjectFunction = "JSGeneratorObject::Function",
        JSGeneratorObjectContext = "JSGeneratorObject::Context",
        JSGeneratorObjectReceiver = "JSGeneratorObject::Receiver",
        JSGeneratorObjectContinuation = "JSGeneratorObject::Continuation",
        JSGeneratorObjectParametersAndRegisters = "JSGeneratorObject::ParametersAndRegisters",
        JSAsyncFunctionObjectPromise = "JSAsyncFunctionObject::Promise",
        JSExternalObjectValue = "JSExternalObject::Value",

        /// The data an API function's template passes its callback: for a
        /// Node-API function, a `JSExternalObject` holding its bundle.
        FunctionTemplateInfoCallbackData = "FunctionTemplateInfo::CallbackData",
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
        UncachedExternalStringMask = "internal::kUncachedExternalStringMask",
        UncachedExternalStringTag = "internal::kUncachedExternalStringTag",
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

        /// A descriptor's words, from its descriptor array's
        /// [`Field::DescriptorArrayDescriptors`]: its key, its details,
        /// and its value, and how many words each descriptor has.
        PropDescKey = "prop_desc_key",
        PropDescDetails = "prop_desc_details",
        PropDescValue = "prop_desc_value",
        PropDescSize = "prop_desc_size",

        /// The bits of a property's details, a Smi: its kind, where its
        /// value is, the field it is in, its representation, and its
        /// attributes.
        PropKindData = "prop_kind_Data",
        PropKindAccessor = "prop_kind_Accessor",
        PropKindMask = "prop_kind_mask",
        PropLocationField = "prop_location_Field",
        PropLocationDescriptor = "prop_location_Descriptor",
        PropLocationMask = "prop_location_mask",
        PropLocationShift = "prop_location_shift",
        PropIndexMask = "prop_index_mask",
        PropIndexShift = "prop_index_shift",
        PropRepresentationSmi = "prop_representation_smi",
        PropRepresentationDouble = "prop_representation_double",
        PropRepresentationHeapObject = "prop_representation_heapobject",
        PropRepresentationTagged = "prop_representation_tagged",
        PropRepresentationMask = "prop_representation_mask",
        PropRepresentationShift = "prop_representation_shift",
        PropAttributesNone = "prop_attributes_NONE",
        PropAttributesReadOnly = "prop_attributes_READ_ONLY",
        PropAttributesDontEnum = "prop_attributes_DONT_ENUM",
        PropAttributesDontDelete = "prop_attributes_DONT_DELETE",
        PropAttributesMask = "prop_attributes_mask",
        PropAttributesShift = "prop_attributes_shift",

        /// Where [`Field::MapBitField3`] says a map is a dictionary map and
        /// how many of its descriptors are its own.
        BitField3IsDictionaryMapShift = "bit_field3_is_dictionary_map_shift",
        BitField3NumberOfOwnDescriptorsMask = "bit_field3_number_of_own_descriptors_mask",
        BitField3NumberOfOwnDescriptorsShift = "bit_field3_number_of_own_descriptors_shift",

        /// A dictionary's slots, in words from its first element.
        NameDictionaryPrefixStartIndex = "namedictionary_prefix_start_index",
        NameDictionaryShapeEntrySize = "namedictionaryshape_entry_size",
        NameDictionaryShapePrefixSize = "namedictionaryshape_prefix_size",
        GlobalDictionaryShapeEntrySize = "globaldictionaryshape_entry_size",
        NumberDictionaryShapeEntrySize = "numberdictionaryshape_entry_size",
        NumberDictionaryShapePrefixSize = "numberdictionaryshape_prefix_size",

        /// The elements kinds of an object's map's [`Field::MapBitField2`].
        ElementsFastElements = "elements_fast_elements",
        ElementsFastHoleyElements = "elements_fast_holey_elements",
        ElementsDictionaryElements = "elements_dictionary_elements",

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
        FeedbackVectorType = "FEEDBACK_VECTOR_TYPE",
        CodeWrapperType = "CODE_WRAPPER_TYPE",
        WeakFixedArrayType = "WEAK_FIXED_ARRAY_TYPE",

        /// From 13.6.
        TrustedWeakFixedArrayType = "TRUSTED_WEAK_FIXED_ARRAY_TYPE",
        ProtectedWeakFixedArrayType = "PROTECTED_WEAK_FIXED_ARRAY_TYPE",
        DescriptorArrayType = "DESCRIPTOR_ARRAY_TYPE",
        StrongDescriptorArrayType = "STRONG_DESCRIPTOR_ARRAY_TYPE",
        PropertyArrayType = "PROPERTY_ARRAY_TYPE",
        NameDictionaryType = "NAME_DICTIONARY_TYPE",
        GlobalDictionaryType = "GLOBAL_DICTIONARY_TYPE",
        SwissNameDictionaryType = "SWISS_NAME_DICTIONARY_TYPE",
        AccessorInfoType = "ACCESSOR_INFO_TYPE",
        AccessorPairType = "ACCESSOR_PAIR_TYPE",
        JSObjectType = "JS_OBJECT_TYPE",
        JSObjectPrototypeType = "JS_OBJECT_PROTOTYPE_TYPE",

        /// The kinds of code, as [`Constant::CodeKindFieldMask`] of its
        /// flags holds them: `CODE_KIND_LIST`'s order in `code-kind.h`.
        /// Optimized code is `TURBOFAN` in 12.4, then `TURBOFAN_JS`.
        CodeKindBuiltin = "CodeKind::BUILTIN",
        CodeKindMaglev = "CodeKind::MAGLEV",
        CodeKindTurbofanJS = "CodeKind::TURBOFAN_JS",

        /// The slots of optimized code's deoptimization data, from
        /// `deoptimization-data.h`, beside those the binary exports. From
        /// 13.6 the inlined functions' shared infos are among the
        /// protected literals.
        DeoptimizationDataFrameTranslationIndex = "DeoptimizationData::kFrameTranslationIndex",
        DeoptimizationDataProtectedLiteralArrayIndex =
            "DeoptimizationData::kProtectedLiteralArrayIndex",
        DeoptimizationDataOsrBytecodeOffsetIndex = "DeoptimizationData::kOsrBytecodeOffsetIndex",
        DeoptimizationDataOsrPcOffsetIndex = "DeoptimizationData::kOsrPcOffsetIndex",
        DeoptimizationDataDeoptExitStartIndex = "DeoptimizationData::kDeoptExitStartIndex",
        DeoptimizationDataFirstDeoptEntryIndex = "DeoptimizationData::kFirstDeoptEntryIndex",

        /// A deoptimization entry's slots from
        /// [`Constant::DeoptimizationDataFirstDeoptEntryIndex`], in a
        /// release build: entry `i`'s translation index, a Smi, is slot
        /// `kFirstDeoptEntryIndex + i * kDeoptEntrySize +
        /// kTranslationIndexOffset`.
        DeoptimizationDataDeoptEntrySize = "DeoptimizationData::kDeoptEntrySize",
        DeoptimizationDataTranslationIndexOffset = "DeoptimizationData::kTranslationIndexOffset",

        /// Optimized code's safepoint table, at its metadata's start, just
        /// past its instructions, and `Code::HandlerTableOffset` bytes long
        /// (`safepoint-table.h`): a header, then `length` entries, each its
        /// return pc's offset in `PcSize` bytes; with deoptimization data,
        /// its deoptimization index plus one in `DeoptIndexSize` bytes and
        /// its trampoline's pc plus one in `PcSize`; then its registers'
        /// bits in `RegisterIndexesSize`. All little-endian, the sizes in
        /// the header's entry configuration.
        SafepointTableLengthOffset = "SafepointTable::kLengthOffset",
        SafepointTableEntryConfigurationOffset = "SafepointTable::kEntryConfigurationOffset",
        SafepointTableHeaderSize = "SafepointTable::kHeaderSize",
        SafepointHasDeoptDataShift = "SafepointTable::HasDeoptDataField::kShift",
        SafepointHasDeoptDataMask = "SafepointTable::HasDeoptDataField::kMask",
        SafepointRegisterIndexesSizeShift = "SafepointTable::RegisterIndexesSizeField::kShift",
        SafepointRegisterIndexesSizeMask = "SafepointTable::RegisterIndexesSizeField::kMask",
        SafepointPcSizeShift = "SafepointTable::PcSizeField::kShift",
        SafepointPcSizeMask = "SafepointTable::PcSizeField::kMask",
        SafepointDeoptIndexSizeShift = "SafepointTable::DeoptIndexSizeField::kShift",
        SafepointDeoptIndexSizeMask = "SafepointTable::DeoptIndexSizeField::kMask",

        /// Maglev code's safepoint table (`maglev-safepoint-table.h`), as
        /// Turbofan's but for its header, and the bytes of its extra spill
        /// slots' count between an entry's deoptimization data and its
        /// registers.
        MaglevSafepointTableLengthOffset = "MaglevSafepointTable::kLengthOffset",
        MaglevSafepointTableEntryConfigurationOffset =
            "MaglevSafepointTable::kEntryConfigurationOffset",
        MaglevSafepointTableHeaderSize = "MaglevSafepointTable::kHeaderSize",
        MaglevSafepointTableNumPushedRegistersSize =
            "MaglevSafepointTable::num_pushed_registers_size",
        MaglevSafepointHasDeoptDataShift = "MaglevSafepointTable::HasDeoptDataField::kShift",
        MaglevSafepointHasDeoptDataMask = "MaglevSafepointTable::HasDeoptDataField::kMask",
        MaglevSafepointRegisterIndexesSizeShift =
            "MaglevSafepointTable::RegisterIndexesSizeField::kShift",
        MaglevSafepointRegisterIndexesSizeMask =
            "MaglevSafepointTable::RegisterIndexesSizeField::kMask",
        MaglevSafepointPcSizeShift = "MaglevSafepointTable::PcSizeField::kShift",
        MaglevSafepointPcSizeMask = "MaglevSafepointTable::PcSizeField::kMask",
        MaglevSafepointDeoptIndexSizeShift = "MaglevSafepointTable::DeoptIndexSizeField::kShift",
        MaglevSafepointDeoptIndexSizeMask = "MaglevSafepointTable::DeoptIndexSizeField::kMask",

        /// The bytecode offset of a function's entry, before its first
        /// bytecode (`globals.h`), and what a builtin continuation's
        /// translated bytecode offset adds to its builtin's id
        /// (`BytecodeOffset` in `utils.h`).
        FunctionEntryBytecodeOffset = "internal::kFunctionEntryBytecodeOffset",
        FirstBuiltinContinuationId = "BytecodeOffset::kFirstBuiltinContinuationId",

        /// An inlining position, as the `InliningPositions` byte array
        /// holds them: a `SourcePosition` and the index of the inlined
        /// function, in `source-position.h`, laid out as C does.
        InliningPositionSize = "sizeof(InliningPosition)",
        InliningPositionPosition = "offsetof(InliningPosition, position)",
        InliningPositionInlinedFunctionId = "offsetof(InliningPosition, inlined_function_id)",

        /// The bit fields of a `SourcePosition`'s 64-bit value, and the
        /// inlining id of a position in no inlined function. The
        /// `InliningIdField` holds the inlining id plus one, so that 0 is
        /// `kNotInlined`; a script offset is valid only where
        /// `IsExternalField` is 0.
        SourcePositionIsExternalShift = "SourcePosition::IsExternalField::kShift",
        SourcePositionIsExternalMask = "SourcePosition::IsExternalField::kMask",
        SourcePositionScriptOffsetShift = "SourcePosition::ScriptOffsetField::kShift",
        SourcePositionScriptOffsetMask = "SourcePosition::ScriptOffsetField::kMask",
        SourcePositionInliningIdShift = "SourcePosition::InliningIdField::kShift",
        SourcePositionInliningIdMask = "SourcePosition::InliningIdField::kMask",
        SourcePositionExternalLineShift = "SourcePosition::ExternalLineField::kShift",
        SourcePositionExternalLineMask = "SourcePosition::ExternalLineField::kMask",
        SourcePositionExternalFileIdShift = "SourcePosition::ExternalFileIdField::kShift",
        SourcePositionExternalFileIdMask = "SourcePosition::ExternalFileIdField::kMask",
        SourcePositionNotInlined = "SourcePosition::kNotInlined",

        /// A hash table's slots before its prefix, in words from its first
        /// element (`hash-table.h`), and a name dictionary's own (its
        /// prefix and its entries', `dictionary.h`).
        HashTableNumberOfElementsIndex = "HashTableBase::kNumberOfElementsIndex",
        HashTableNumberOfDeletedElementsIndex = "HashTableBase::kNumberOfDeletedElementsIndex",
        HashTableCapacityIndex = "HashTableBase::kCapacityIndex",
        HashTablePrefixStartIndex = "HashTableBase::kPrefixStartIndex",
        HashTableEntryKeyIndex = "HashTable::kEntryKeyIndex",
        NameDictionaryNextEnumerationIndexIndex = "BaseNameDictionary::kNextEnumerationIndexIndex",
        NameDictionaryObjectHashIndex = "BaseNameDictionary::kObjectHashIndex",
        NameDictionaryFlagsIndex = "NameDictionary::kFlagsIndex",
        NameDictionaryEntryValueIndex = "NameDictionary::kEntryValueIndex",
        NameDictionaryEntryDetailsIndex = "NameDictionary::kEntryDetailsIndex",

        /// The table a scope of many context locals keeps their names in,
        /// each name's value the Smi index of its local (`hash-table.h`).
        NameToIndexHashTableType = "NAME_TO_INDEX_HASH_TABLE_TYPE",
        NameToIndexPrefixSize = "NameToIndexShape::kPrefixSize",
        NameToIndexEntrySize = "NameToIndexShape::kEntrySize",
        NameToIndexEntryValueIndex = "NameToIndexShape::kEntryValueIndex",

        /// A name's raw hash field: the bits of its type, the type of a
        /// field that holds a hash, and where the hash begins above them
        /// (`name.h`).
        NameHashFieldTypeMask = "Name::HashFieldTypeBits::kMask",
        NameHashFieldTypeHash = "Name::HashFieldType::kHash",
        NameHashShift = "Name::HashBits::kShift",

        /// The bits of a `PropertyArray`'s `LengthAndHash` its length takes,
        /// the lowest.
        PropertyArrayLengthFieldSize = "PropertyArray::kLengthFieldSize",

        /// Where a function's map keeps its `name` descriptor while V8
        /// names the function from its `SharedFunctionInfo`, and the fewest
        /// own descriptors such a map has (`js-function.h`).
        JSFunctionNameDescriptorIndex = "JSFunction::kNameDescriptorIndex",
        JSFunctionMinDescriptorsForFastBindAndWrap = "JSFunction::kMinDescriptorsForFastBindAndWrap",

        /// How a weak reference is told from a strong one, and the value
        /// of a cleared one's low 32 bits (`v8-internal.h`, `globals.h`).
        WeakHeapObjectTag = "internal::kWeakHeapObjectTag",
        WeakHeapObjectMask = "internal::kWeakHeapObjectMask",
        ClearedWeakHeapObjectLower32 = "internal::kClearedWeakHeapObjectLower32",

        /// From 13.6, the JS dispatch table's entries (`globals.h`,
        /// `js-dispatch-table.h`): a handle shifted right by
        /// `kJSDispatchHandleShift` is its entry's index; an entry is
        /// `kJSDispatchTableEntrySize` bytes, whose word at
        /// `kCodeObjectOffset`, shifted right by `kObjectPointerShift`
        /// with `kHeapObjectTag` set, is its code, the bit it shifts into
        /// the tag's place being the entry's marking bit; the word's low
        /// bits under `kParameterCountMask` are the parameter count.
        JSDispatchTableEntrySize = "internal::kJSDispatchTableEntrySize",
        JSDispatchHandleShift = "internal::kJSDispatchHandleShift",
        JSDispatchEntryEntrypointOffset = "JSDispatchEntry::kEntrypointOffset",
        JSDispatchEntryCodeObjectOffset = "JSDispatchEntry::kCodeObjectOffset",
        JSDispatchEntryObjectPointerShift = "JSDispatchEntry::kObjectPointerShift",
        JSDispatchEntryParameterCountMask = "JSDispatchEntry::kParameterCountMask",

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

        /// The compilation type of an `eval`'s script
        /// (`Script::CompilationType`, `script.h`).
        ScriptCompilationTypeEval = "Script::CompilationType::kEval",
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

        /// The bits of [`Field::FeedbackVectorOsrState`]: whether a loop's
        /// Maglev or Turbofan OSR code may be cached in a slot.
        FeedbackOsrUrgencyShift = "FeedbackVector::OsrUrgencyBits::kShift",
        FeedbackOsrUrgencyMask = "FeedbackVector::OsrUrgencyBits::kMask",
        FeedbackMaybeHasMaglevOsrCodeShift = "FeedbackVector::MaybeHasMaglevOsrCodeBit::kShift",
        FeedbackMaybeHasMaglevOsrCodeMask = "FeedbackVector::MaybeHasMaglevOsrCodeBit::kMask",
        FeedbackMaybeHasTurbofanOsrCodeShift =
            "FeedbackVector::MaybeHasTurbofanOsrCodeBit::kShift",
        FeedbackMaybeHasTurbofanOsrCodeMask = "FeedbackVector::MaybeHasTurbofanOsrCodeBit::kMask",

        /// The bits of [`Field::FeedbackVectorFlags`] in 12.4: whether
        /// [`Field::FeedbackVectorMaybeOptimizedCode`] may hold code.
        FeedbackMaybeHasMaglevCodeShift = "FeedbackVector::MaybeHasMaglevCodeBit::kShift",
        FeedbackMaybeHasMaglevCodeMask = "FeedbackVector::MaybeHasMaglevCodeBit::kMask",
        FeedbackMaybeHasTurbofanCodeShift = "FeedbackVector::MaybeHasTurbofanCodeBit::kShift",
        FeedbackMaybeHasTurbofanCodeMask = "FeedbackVector::MaybeHasTurbofanCodeBit::kMask",

        /// From 13.6.
        FeedbackTieringInProgressShift = "FeedbackVector::TieringInProgressBit::kShift",
        FeedbackTieringInProgressMask = "FeedbackVector::TieringInProgressBit::kMask",
        FeedbackOsrTieringInProgressShift = "FeedbackVector::OsrTieringInProgressBit::kShift",
        FeedbackOsrTieringInProgressMask = "FeedbackVector::OsrTieringInProgressBit::kMask",

        /// The instance types of JavaScript's values.
        HoleType = "HOLE_TYPE",
        BigIntBaseType = "BIG_INT_BASE_TYPE",
        JSArrayType = "JS_ARRAY_TYPE",
        FixedDoubleArrayType = "FIXED_DOUBLE_ARRAY_TYPE",
        NumberDictionaryType = "NUMBER_DICTIONARY_TYPE",
        JSDateType = "JS_DATE_TYPE",
        JSRegExpType = "JS_REG_EXP_TYPE",
        JSPromiseType = "JS_PROMISE_TYPE",
        JSTypedArrayType = "JS_TYPED_ARRAY_TYPE",
        JSArrayBufferType = "JS_ARRAY_BUFFER_TYPE",
        JSDataViewType = "JS_DATA_VIEW_TYPE",
        JSRabGsabDataViewType = "JS_RAB_GSAB_DATA_VIEW_TYPE",
        JSPrimitiveWrapperType = "JS_PRIMITIVE_WRAPPER_TYPE",
        JSProxyType = "JS_PROXY_TYPE",
        JSBoundFunctionType = "JS_BOUND_FUNCTION_TYPE",
        JSFunctionType = "JS_FUNCTION_TYPE",
        JSClassConstructorType = "JS_CLASS_CONSTRUCTOR_TYPE",
        JSMapType = "JS_MAP_TYPE",
        JSSetType = "JS_SET_TYPE",
        JSWeakMapType = "JS_WEAK_MAP_TYPE",
        JSWeakSetType = "JS_WEAK_SET_TYPE",
        JSErrorType = "JS_ERROR_TYPE",
        OrderedHashMapType = "ORDERED_HASH_MAP_TYPE",
        OrderedHashSetType = "ORDERED_HASH_SET_TYPE",
        ErrorStackDataType = "ERROR_STACK_DATA_TYPE",
        CallSiteInfoType = "CALL_SITE_INFO_TYPE",

        /// A `WeakMap`'s table, as Node's `overrideStackTrace` keeps one.
        EphemeronHashTableType = "EPHEMERON_HASH_TABLE_TYPE",
        JSGlobalObjectType = "JS_GLOBAL_OBJECT_TYPE",
        JSGlobalProxyType = "JS_GLOBAL_PROXY_TYPE",
        JSApiObjectType = "JS_API_OBJECT_TYPE",
        JSSpecialApiObjectType = "JS_SPECIAL_API_OBJECT_TYPE",
        JSArgumentsObjectType = "JS_ARGUMENTS_OBJECT_TYPE",
        JSContextExtensionObjectType = "JS_CONTEXT_EXTENSION_OBJECT_TYPE",
        JSAsyncFunctionObjectType = "JS_ASYNC_FUNCTION_OBJECT_TYPE",
        JSGeneratorObjectType = "JS_GENERATOR_OBJECT_TYPE",
        JSAsyncGeneratorObjectType = "JS_ASYNC_GENERATOR_OBJECT_TYPE",
        JSExternalObjectType = "JS_EXTERNAL_OBJECT_TYPE",
        PropertyCellType = "PROPERTY_CELL_TYPE",
        ScriptContextTableType = "SCRIPT_CONTEXT_TABLE_TYPE",
        FunctionContextType = "FUNCTION_CONTEXT_TYPE",
        BlockContextType = "BLOCK_CONTEXT_TYPE",
        ScriptContextType = "SCRIPT_CONTEXT_TYPE",
        ModuleContextType = "MODULE_CONTEXT_TYPE",
        CatchContextType = "CATCH_CONTEXT_TYPE",
        WithContextType = "WITH_CONTEXT_TYPE",
        EvalContextType = "EVAL_CONTEXT_TYPE",
        AwaitContextType = "AWAIT_CONTEXT_TYPE",

        /// The instance types `JSReceiver::class_name` and
        /// `GetConstructorHelper` tell apart.
        FunctionTemplateInfoType = "FUNCTION_TEMPLATE_INFO_TYPE",
        Tuple2Type = "TUPLE2_TYPE",
        WasmExportedFunctionDataType = "WASM_EXPORTED_FUNCTION_DATA_TYPE",
        JSArrayIteratorType = "JS_ARRAY_ITERATOR_TYPE",
        JSSharedStructType = "JS_SHARED_STRUCT_TYPE",
        JSSharedArrayType = "JS_SHARED_ARRAY_TYPE",
        JSAtomicsMutexType = "JS_ATOMICS_MUTEX_TYPE",
        JSAtomicsConditionType = "JS_ATOMICS_CONDITION_TYPE",

        /// The first and last instance types of a range, inclusive.
        FirstJSReceiverType = "FIRST_JS_RECEIVER_TYPE",
        LastJSReceiverType = "LAST_JS_RECEIVER_TYPE",
        FirstJSFunctionOrBoundFunctionOrWrappedFunctionType =
            "FIRST_JS_FUNCTION_OR_BOUND_FUNCTION_OR_WRAPPED_FUNCTION_TYPE",
        LastJSFunctionOrBoundFunctionOrWrappedFunctionType =
            "LAST_JS_FUNCTION_OR_BOUND_FUNCTION_OR_WRAPPED_FUNCTION_TYPE",
        FirstJSGeneratorObjectType = "FIRST_JS_GENERATOR_OBJECT_TYPE",
        LastJSGeneratorObjectType = "LAST_JS_GENERATOR_OBJECT_TYPE",
        FirstJSMapIteratorType = "FIRST_JS_MAP_ITERATOR_TYPE",
        LastJSMapIteratorType = "LAST_JS_MAP_ITERATOR_TYPE",
        FirstJSSetIteratorType = "FIRST_JS_SET_ITERATOR_TYPE",
        LastJSSetIteratorType = "LAST_JS_SET_ITERATOR_TYPE",
        FirstJSObjectType = "FIRST_JS_OBJECT_TYPE",
        LastJSObjectType = "LAST_JS_OBJECT_TYPE",
        FirstJSArrayBufferViewType = "FIRST_JS_ARRAY_BUFFER_VIEW_TYPE",
        LastJSArrayBufferViewType = "LAST_JS_ARRAY_BUFFER_VIEW_TYPE",

        /// The bits of [`Field::MapBitField`] and [`Field::MapBitField2`]:
        /// an object's elements kind is one of the `ElementsKind::` ones.
        MapIsCallableShift = "Map::IsCallableBit::kShift",
        MapIsCallableMask = "Map::IsCallableBit::kMask",
        MapIsConstructorShift = "Map::IsConstructorBit::kShift",
        MapIsConstructorMask = "Map::IsConstructorBit::kMask",
        MapIsUndetectableShift = "Map::IsUndetectableBit::kShift",
        MapIsUndetectableMask = "Map::IsUndetectableBit::kMask",
        MapHasNamedInterceptorShift = "Map::HasNamedInterceptorBit::kShift",
        MapHasNamedInterceptorMask = "Map::HasNamedInterceptorBit::kMask",
        MapHasIndexedInterceptorShift = "Map::HasIndexedInterceptorBit::kShift",
        MapHasIndexedInterceptorMask = "Map::HasIndexedInterceptorBit::kMask",
        MapIsAccessCheckNeededShift = "Map::IsAccessCheckNeededBit::kShift",
        MapIsAccessCheckNeededMask = "Map::IsAccessCheckNeededBit::kMask",
        MapElementsKindShift = "Map::ElementsKindBits::kShift",
        MapElementsKindMask = "Map::ElementsKindBits::kMask",
        MapNewTargetIsBaseShift = "Map::NewTargetIsBaseBit::kShift",
        MapNewTargetIsBaseMask = "Map::NewTargetIsBaseBit::kMask",

        /// The bit of [`Field::MapBitField3`] set on a prototype's map.
        MapIsPrototypeMapShift = "Map::IsPrototypeMapBit::kShift",
        MapIsPrototypeMapMask = "Map::IsPrototypeMapBit::kMask",

        /// The bits of [`Field::SymbolFlags`]. Through 13.6 a private
        /// symbol has `IsPrivateBit`, and a private name or brand its own
        /// bit too.
        SymbolIsPrivateShift = "Symbol::IsPrivateBit::kShift",
        SymbolIsPrivateMask = "Symbol::IsPrivateBit::kMask",
        SymbolIsPrivateNameShift = "Symbol::IsPrivateNameBit::kShift",
        SymbolIsPrivateNameMask = "Symbol::IsPrivateNameBit::kMask",
        SymbolIsPrivateBrandShift = "Symbol::IsPrivateBrandBit::kShift",
        SymbolIsPrivateBrandMask = "Symbol::IsPrivateBrandBit::kMask",

        /// From 14.6, the three are one `PrivateSymbolKind` (`name.h`):
        /// public; private and internal to V8; a private name, `#field`;
        /// or a class's private brand.
        SymbolPrivateSymbolKindShift = "Symbol::PrivateSymbolKindBits::kShift",
        SymbolPrivateSymbolKindMask = "Symbol::PrivateSymbolKindBits::kMask",
        PrivateSymbolKindPublic = "PrivateSymbolKind::kPublic",
        PrivateSymbolKindInternal = "PrivateSymbolKind::kInternal",
        PrivateSymbolKindFieldName = "PrivateSymbolKind::kFieldName",
        PrivateSymbolKindBrand = "PrivateSymbolKind::kBrand",
        SymbolIsWellKnownSymbolShift = "Symbol::IsWellKnownSymbolBit::kShift",
        SymbolIsWellKnownSymbolMask = "Symbol::IsWellKnownSymbolBit::kMask",
        SymbolIsInPublicSymbolTableShift = "Symbol::IsInPublicSymbolTableBit::kShift",
        SymbolIsInPublicSymbolTableMask = "Symbol::IsInPublicSymbolTableBit::kMask",
        SymbolIsInterestingSymbolShift = "Symbol::IsInterestingSymbolBit::kShift",
        SymbolIsInterestingSymbolMask = "Symbol::IsInterestingSymbolBit::kMask",

        /// The bits of [`Field::BigIntBaseBitfield`] (`bigint.h`): the
        /// sign, set for a negative `BigInt`, and the number of digits.
        BigIntSignShift = "BigIntBase::SignBits::kShift",
        BigIntSignMask = "BigIntBase::SignBits::kMask",
        BigIntLengthShift = "BigIntBase::LengthBits::kShift",
        BigIntLengthMask = "BigIntBase::LengthBits::kMask",

        /// The bits of [`Field::JSPromiseFlags`]: its status is one of the
        /// `Promise::` states.
        JSPromiseStatusShift = "JSPromise::StatusBits::kShift",
        JSPromiseStatusMask = "JSPromise::StatusBits::kMask",
        JSPromiseHasHandlerShift = "JSPromise::HasHandlerBit::kShift",
        JSPromiseHasHandlerMask = "JSPromise::HasHandlerBit::kMask",

        /// The bits of [`Field::JSArrayBufferBitField`].
        JSArrayBufferWasDetachedShift = "JSArrayBuffer::WasDetachedBit::kShift",
        JSArrayBufferWasDetachedMask = "JSArrayBuffer::WasDetachedBit::kMask",
        JSArrayBufferIsSharedShift = "JSArrayBuffer::IsSharedBit::kShift",
        JSArrayBufferIsSharedMask = "JSArrayBuffer::IsSharedBit::kMask",
        JSArrayBufferIsResizableByJsShift = "JSArrayBuffer::IsResizableByJsBit::kShift",
        JSArrayBufferIsResizableByJsMask = "JSArrayBuffer::IsResizableByJsBit::kMask",

        /// The bits of [`Field::JSArrayBufferViewBitField`].
        JSArrayBufferViewIsLengthTrackingShift = "JSArrayBufferView::IsLengthTrackingBit::kShift",
        JSArrayBufferViewIsLengthTrackingMask = "JSArrayBufferView::IsLengthTrackingBit::kMask",
        JSArrayBufferViewIsBackedByRabShift = "JSArrayBufferView::IsBackedByRabBit::kShift",
        JSArrayBufferViewIsBackedByRabMask = "JSArrayBufferView::IsBackedByRabBit::kMask",

        /// The bits of [`Field::JSRegExpFlags`], a Smi.
        JSRegExpGlobalShift = "JSRegExp::GlobalBit::kShift",
        JSRegExpGlobalMask = "JSRegExp::GlobalBit::kMask",
        JSRegExpIgnoreCaseShift = "JSRegExp::IgnoreCaseBit::kShift",
        JSRegExpIgnoreCaseMask = "JSRegExp::IgnoreCaseBit::kMask",
        JSRegExpMultilineShift = "JSRegExp::MultilineBit::kShift",
        JSRegExpMultilineMask = "JSRegExp::MultilineBit::kMask",
        JSRegExpStickyShift = "JSRegExp::StickyBit::kShift",
        JSRegExpStickyMask = "JSRegExp::StickyBit::kMask",
        JSRegExpUnicodeShift = "JSRegExp::UnicodeBit::kShift",
        JSRegExpUnicodeMask = "JSRegExp::UnicodeBit::kMask",
        JSRegExpDotAllShift = "JSRegExp::DotAllBit::kShift",
        JSRegExpDotAllMask = "JSRegExp::DotAllBit::kMask",
        JSRegExpLinearShift = "JSRegExp::LinearBit::kShift",
        JSRegExpLinearMask = "JSRegExp::LinearBit::kMask",
        JSRegExpHasIndicesShift = "JSRegExp::HasIndicesBit::kShift",
        JSRegExpHasIndicesMask = "JSRegExp::HasIndicesBit::kMask",
        JSRegExpUnicodeSetsShift = "JSRegExp::UnicodeSetsBit::kShift",
        JSRegExpUnicodeSetsMask = "JSRegExp::UnicodeSetsBit::kMask",

        /// The bits of [`Field::CallSiteInfoFlags`], a Smi.
        CallSiteInfoIsWasmShift = "CallSiteInfo::IsWasmBit::kShift",
        CallSiteInfoIsWasmMask = "CallSiteInfo::IsWasmBit::kMask",
        CallSiteInfoIsAsmJsWasmShift = "CallSiteInfo::IsAsmJsWasmBit::kShift",
        CallSiteInfoIsAsmJsWasmMask = "CallSiteInfo::IsAsmJsWasmBit::kMask",
        CallSiteInfoIsStrictShift = "CallSiteInfo::IsStrictBit::kShift",
        CallSiteInfoIsStrictMask = "CallSiteInfo::IsStrictBit::kMask",
        CallSiteInfoIsConstructorShift = "CallSiteInfo::IsConstructorBit::kShift",
        CallSiteInfoIsConstructorMask = "CallSiteInfo::IsConstructorBit::kMask",
        CallSiteInfoIsAsmJsAtNumberConversionShift =
            "CallSiteInfo::IsAsmJsAtNumberConversionBit::kShift",
        CallSiteInfoIsAsmJsAtNumberConversionMask =
            "CallSiteInfo::IsAsmJsAtNumberConversionBit::kMask",
        CallSiteInfoIsAsyncShift = "CallSiteInfo::IsAsyncBit::kShift",
        CallSiteInfoIsAsyncMask = "CallSiteInfo::IsAsyncBit::kMask",
        CallSiteInfoIsBuiltinShift = "CallSiteInfo::IsBuiltinBit::kShift",
        CallSiteInfoIsBuiltinMask = "CallSiteInfo::IsBuiltinBit::kMask",
        CallSiteInfoIsSourcePositionComputedShift =
            "CallSiteInfo::IsSourcePositionComputedBit::kShift",
        CallSiteInfoIsSourcePositionComputedMask =
            "CallSiteInfo::IsSourcePositionComputedBit::kMask",

        /// From 14.6, set only in the raw data of
        /// [`Field::ErrorStackDataRawDataForCallSiteInfosOrFormattedStack`].
        CallSiteInfoIsDeferredBaselineFrameShift =
            "CallSiteInfo::IsDeferredBaselineFrameBit::kShift",
        CallSiteInfoIsDeferredBaselineFrameMask =
            "CallSiteInfo::IsDeferredBaselineFrameBit::kMask",

        /// From 14.6, where each of a raw frame's elements is
        /// (`CallSiteInfo::Fields`), and how many it has.
        CallSiteInfoRawCode = "CallSiteInfo::kCode",
        CallSiteInfoRawReceiver = "CallSiteInfo::kReceiver",
        CallSiteInfoRawFunction = "CallSiteInfo::kFunction",
        CallSiteInfoRawOffset = "CallSiteInfo::kOffset",
        CallSiteInfoRawParameters = "CallSiteInfo::kParameters",
        CallSiteInfoRawFlags = "CallSiteInfo::kFlags",
        CallSiteInfoRawCount = "CallSiteInfo::kCount",

        /// The bits of a context local's info, a Smi of the `ScopeInfo`'s
        /// `ContextLocalInfos` slice, beside its name in
        /// `ContextLocalNames`: its mode, one of the `VariableMode::` ones,
        /// and whether it must be initialized before use, one of the
        /// `InitializationFlag::` ones.
        ContextLocalVariableModeShift = "ScopeInfo::VariableModeBits::kShift",
        ContextLocalVariableModeMask = "ScopeInfo::VariableModeBits::kMask",
        ContextLocalInitFlagShift = "ScopeInfo::InitFlagBit::kShift",
        ContextLocalInitFlagMask = "ScopeInfo::InitFlagBit::kMask",
        ContextLocalMaybeAssignedFlagShift = "ScopeInfo::MaybeAssignedFlagBit::kShift",
        ContextLocalMaybeAssignedFlagMask = "ScopeInfo::MaybeAssignedFlagBit::kMask",
        ContextLocalParameterNumberShift = "ScopeInfo::ParameterNumberBits::kShift",
        ContextLocalParameterNumberMask = "ScopeInfo::ParameterNumberBits::kMask",
        ContextLocalIsStaticFlagShift = "ScopeInfo::IsStaticFlagBit::kShift",
        ContextLocalIsStaticFlagMask = "ScopeInfo::IsStaticFlagBit::kMask",

        /// A variable's modes (`globals.h`).
        VariableModeLet = "VariableMode::kLet",
        VariableModeConst = "VariableMode::kConst",
        VariableModeUsing = "VariableMode::kUsing",

        /// From 13.6.
        VariableModeAwaitUsing = "VariableMode::kAwaitUsing",
        VariableModeVar = "VariableMode::kVar",
        VariableModeTemporary = "VariableMode::kTemporary",
        VariableModeDynamic = "VariableMode::kDynamic",
        VariableModeDynamicGlobal = "VariableMode::kDynamicGlobal",
        VariableModeDynamicLocal = "VariableMode::kDynamicLocal",
        VariableModePrivateMethod = "VariableMode::kPrivateMethod",
        VariableModePrivateSetterOnly = "VariableMode::kPrivateSetterOnly",
        VariableModePrivateGetterOnly = "VariableMode::kPrivateGetterOnly",
        VariableModePrivateGetterAndSetter = "VariableMode::kPrivateGetterAndSetter",
        InitializationFlagNeedsInitialization = "InitializationFlag::kNeedsInitialization",
        InitializationFlagCreatedInitialized = "InitializationFlag::kCreatedInitialized",

        /// The elements kinds (`elements-kind.h`), as
        /// [`Constant::MapElementsKindMask`] of a map's second bit field
        /// holds them; the typed arrays' are numbered in each version's
        /// `TYPED_ARRAYS` order.
        ElementsKindPackedSmi = "ElementsKind::PACKED_SMI_ELEMENTS",
        ElementsKindHoleySmi = "ElementsKind::HOLEY_SMI_ELEMENTS",
        ElementsKindPacked = "ElementsKind::PACKED_ELEMENTS",
        ElementsKindHoley = "ElementsKind::HOLEY_ELEMENTS",
        ElementsKindPackedDouble = "ElementsKind::PACKED_DOUBLE_ELEMENTS",
        ElementsKindHoleyDouble = "ElementsKind::HOLEY_DOUBLE_ELEMENTS",
        ElementsKindPackedNonextensible = "ElementsKind::PACKED_NONEXTENSIBLE_ELEMENTS",
        ElementsKindHoleyNonextensible = "ElementsKind::HOLEY_NONEXTENSIBLE_ELEMENTS",
        ElementsKindPackedSealed = "ElementsKind::PACKED_SEALED_ELEMENTS",
        ElementsKindHoleySealed = "ElementsKind::HOLEY_SEALED_ELEMENTS",
        ElementsKindPackedFrozen = "ElementsKind::PACKED_FROZEN_ELEMENTS",
        ElementsKindHoleyFrozen = "ElementsKind::HOLEY_FROZEN_ELEMENTS",
        ElementsKindSharedArray = "ElementsKind::SHARED_ARRAY_ELEMENTS",
        ElementsKindDictionary = "ElementsKind::DICTIONARY_ELEMENTS",
        ElementsKindFastSloppyArguments = "ElementsKind::FAST_SLOPPY_ARGUMENTS_ELEMENTS",
        ElementsKindSlowSloppyArguments = "ElementsKind::SLOW_SLOPPY_ARGUMENTS_ELEMENTS",
        ElementsKindFastStringWrapper = "ElementsKind::FAST_STRING_WRAPPER_ELEMENTS",
        ElementsKindSlowStringWrapper = "ElementsKind::SLOW_STRING_WRAPPER_ELEMENTS",
        ElementsKindUint8 = "ElementsKind::UINT8_ELEMENTS",
        ElementsKindInt8 = "ElementsKind::INT8_ELEMENTS",
        ElementsKindUint16 = "ElementsKind::UINT16_ELEMENTS",
        ElementsKindInt16 = "ElementsKind::INT16_ELEMENTS",
        ElementsKindUint32 = "ElementsKind::UINT32_ELEMENTS",
        ElementsKindInt32 = "ElementsKind::INT32_ELEMENTS",
        ElementsKindFloat32 = "ElementsKind::FLOAT32_ELEMENTS",
        ElementsKindFloat64 = "ElementsKind::FLOAT64_ELEMENTS",
        ElementsKindUint8Clamped = "ElementsKind::UINT8_CLAMPED_ELEMENTS",
        ElementsKindBigUint64 = "ElementsKind::BIGUINT64_ELEMENTS",
        ElementsKindBigInt64 = "ElementsKind::BIGINT64_ELEMENTS",
        ElementsKindFloat16 = "ElementsKind::FLOAT16_ELEMENTS",
        ElementsKindRabGsabUint8 = "ElementsKind::RAB_GSAB_UINT8_ELEMENTS",
        ElementsKindRabGsabInt8 = "ElementsKind::RAB_GSAB_INT8_ELEMENTS",
        ElementsKindRabGsabUint16 = "ElementsKind::RAB_GSAB_UINT16_ELEMENTS",
        ElementsKindRabGsabInt16 = "ElementsKind::RAB_GSAB_INT16_ELEMENTS",
        ElementsKindRabGsabUint32 = "ElementsKind::RAB_GSAB_UINT32_ELEMENTS",
        ElementsKindRabGsabInt32 = "ElementsKind::RAB_GSAB_INT32_ELEMENTS",
        ElementsKindRabGsabFloat32 = "ElementsKind::RAB_GSAB_FLOAT32_ELEMENTS",
        ElementsKindRabGsabFloat64 = "ElementsKind::RAB_GSAB_FLOAT64_ELEMENTS",
        ElementsKindRabGsabUint8Clamped = "ElementsKind::RAB_GSAB_UINT8_CLAMPED_ELEMENTS",
        ElementsKindRabGsabBigUint64 = "ElementsKind::RAB_GSAB_BIGUINT64_ELEMENTS",
        ElementsKindRabGsabBigInt64 = "ElementsKind::RAB_GSAB_BIGINT64_ELEMENTS",
        ElementsKindRabGsabFloat16 = "ElementsKind::RAB_GSAB_FLOAT16_ELEMENTS",

        /// The kinds of function (`function-kind.h`), as
        /// [`Constant::SfiFunctionKindMask`] holds them.
        FunctionKindNormalFunction = "FunctionKind::kNormalFunction",
        FunctionKindModule = "FunctionKind::kModule",

        /// 12.4 only; from 13.6 `kModuleWithTopLevelAwait`.
        FunctionKindAsyncModule = "FunctionKind::kAsyncModule",
        FunctionKindModuleWithTopLevelAwait = "FunctionKind::kModuleWithTopLevelAwait",
        FunctionKindBaseConstructor = "FunctionKind::kBaseConstructor",
        FunctionKindDefaultBaseConstructor = "FunctionKind::kDefaultBaseConstructor",
        FunctionKindDefaultDerivedConstructor = "FunctionKind::kDefaultDerivedConstructor",
        FunctionKindDerivedConstructor = "FunctionKind::kDerivedConstructor",
        FunctionKindGetterFunction = "FunctionKind::kGetterFunction",
        FunctionKindStaticGetterFunction = "FunctionKind::kStaticGetterFunction",
        FunctionKindSetterFunction = "FunctionKind::kSetterFunction",
        FunctionKindStaticSetterFunction = "FunctionKind::kStaticSetterFunction",
        FunctionKindArrowFunction = "FunctionKind::kArrowFunction",
        FunctionKindAsyncArrowFunction = "FunctionKind::kAsyncArrowFunction",
        FunctionKindAsyncFunction = "FunctionKind::kAsyncFunction",
        FunctionKindAsyncConciseMethod = "FunctionKind::kAsyncConciseMethod",
        FunctionKindStaticAsyncConciseMethod = "FunctionKind::kStaticAsyncConciseMethod",
        FunctionKindAsyncConciseGeneratorMethod = "FunctionKind::kAsyncConciseGeneratorMethod",
        FunctionKindStaticAsyncConciseGeneratorMethod =
            "FunctionKind::kStaticAsyncConciseGeneratorMethod",
        FunctionKindAsyncGeneratorFunction = "FunctionKind::kAsyncGeneratorFunction",
        FunctionKindGeneratorFunction = "FunctionKind::kGeneratorFunction",
        FunctionKindConciseGeneratorMethod = "FunctionKind::kConciseGeneratorMethod",
        FunctionKindStaticConciseGeneratorMethod = "FunctionKind::kStaticConciseGeneratorMethod",
        FunctionKindConciseMethod = "FunctionKind::kConciseMethod",
        FunctionKindStaticConciseMethod = "FunctionKind::kStaticConciseMethod",
        FunctionKindClassMembersInitializerFunction =
            "FunctionKind::kClassMembersInitializerFunction",

        /// From 14.6.
        FunctionKindClassMembersInitializerFunctionPrecededByStatic =
            "FunctionKind::kClassMembersInitializerFunctionPrecededByStatic",
        FunctionKindClassStaticInitializerFunction =
            "FunctionKind::kClassStaticInitializerFunction",

        /// From 14.6.
        FunctionKindClassStaticInitializerFunctionPrecededByMember =
            "FunctionKind::kClassStaticInitializerFunctionPrecededByMember",
        FunctionKindInvalid = "FunctionKind::kInvalid",

        /// A promise's states (`v8-promise.h`).
        PromisePending = "Promise::kPending",
        PromiseFulfilled = "Promise::kFulfilled",
        PromiseRejected = "Promise::kRejected",

        /// The kinds of an oddball (`oddball.h`), as [`Field::OddballKind`]
        /// holds them; the hole is a `Hole`, with no kind.
        OddballKindUndefined = "Oddball::kUndefined",
        OddballKindNull = "Oddball::kNull",
        OddballKindTrue = "Oddball::kTrue",
        OddballKindFalse = "Oddball::kFalse",

        /// The bits of the hole in a holey double array (`globals.h`): a
        /// NaN, as a 64-bit pattern.
        HoleNanInt64 = "internal::kHoleNanInt64",

        /// 1 where [`Field::JSDateValue`] holds the time as a raw `float64`,
        /// as from 13.6, and 0 where it holds a tagged number, as
        /// `class-debug-readers.cc` reads it.
        JSDateValueIsFloat64 = "JSDate::value is float64",

        /// An ordered hash table's slots (`ordered-hash-table.h`), in
        /// elements from its `FixedArray::Objects`: its counts, then its
        /// buckets from `HashTableStartIndex()`, each the first entry of its
        /// chain or `kNotFound`, then its entries of `kEntrySize`, each its
        /// key, a map's value at `kValueOffset`, and the next entry of its
        /// chain at `kChainOffset`. An obsolete table's deleted count is
        /// `kClearedTableSentinel` when it was cleared.
        OrderedHashMapEntrySize = "OrderedHashMap::kEntrySize",
        OrderedHashMapChainOffset = "OrderedHashMap::kChainOffset",
        OrderedHashMapValueOffset = "OrderedHashMap::kValueOffset",
        OrderedHashMapNumberOfElementsIndex = "OrderedHashMap::NumberOfElementsIndex()",
        OrderedHashMapNumberOfDeletedElementsIndex =
            "OrderedHashMap::NumberOfDeletedElementsIndex()",
        OrderedHashMapNumberOfBucketsIndex = "OrderedHashMap::NumberOfBucketsIndex()",
        OrderedHashMapHashTableStartIndex = "OrderedHashMap::HashTableStartIndex()",
        OrderedHashMapClearedTableSentinel = "OrderedHashMap::kClearedTableSentinel",
        OrderedHashMapNotFound = "OrderedHashMap::kNotFound",
        OrderedHashSetEntrySize = "OrderedHashSet::kEntrySize",
        OrderedHashSetChainOffset = "OrderedHashSet::kChainOffset",
        OrderedHashSetNumberOfElementsIndex = "OrderedHashSet::NumberOfElementsIndex()",
        OrderedHashSetNumberOfDeletedElementsIndex =
            "OrderedHashSet::NumberOfDeletedElementsIndex()",
        OrderedHashSetNumberOfBucketsIndex = "OrderedHashSet::NumberOfBucketsIndex()",
        OrderedHashSetHashTableStartIndex = "OrderedHashSet::HashTableStartIndex()",
        OrderedHashSetClearedTableSentinel = "OrderedHashSet::kClearedTableSentinel",
        OrderedHashSetNotFound = "OrderedHashSet::kNotFound",

        /// A number dictionary's entry's value and details, from its key
        /// at [`Constant::HashTableEntryKeyIndex`] (`dictionary.h`).
        NumberDictionaryEntryValueIndex = "NumberDictionary::kEntryValueIndex",
        NumberDictionaryEntryDetailsIndex = "NumberDictionary::kEntryDetailsIndex",

        /// A global dictionary's prefix: each of its entries,
        /// [`Constant::GlobalDictionaryShapeEntrySize`] long, is the
        /// `PropertyCell` at its key's index, which holds the property's
        /// name, value, and details.
        GlobalDictionaryShapePrefixSize = "GlobalDictionaryShape::kPrefixSize",

        /// Where a dictionary property's details keep its enumeration
        /// index (`property-details.h`).
        PropertyDetailsDictionaryStorageShift = "PropertyDetails::DictionaryStorageField::kShift",
        PropertyDetailsDictionaryStorageMask = "PropertyDetails::DictionaryStorageField::kMask",

        /// A context's slots, in elements from [`Field::ContextElements`]
        /// (`contexts.h`): its scope info, its previous context, and its
        /// extension, and how many slots come before its locals without and
        /// with an extension. Only a context whose scope info has
        /// `HasContextExtensionSlotBit` has an extension; a native context's
        /// extension is its global object. A context's native context is its
        /// map's [`Field::MapConstructorOrBackPointerOrNativeContext`].
        ContextScopeInfoIndex = "Context::SCOPE_INFO_INDEX",
        ContextPreviousIndex = "Context::PREVIOUS_INDEX",
        ContextExtensionIndex = "Context::EXTENSION_INDEX",
        ContextMinContextSlots = "Context::MIN_CONTEXT_SLOTS",
        ContextMinContextExtendedSlots = "Context::MIN_CONTEXT_EXTENDED_SLOTS",

        /// A native context's script context table and global proxy.
        ContextScriptContextTableIndex = "Context::SCRIPT_CONTEXT_TABLE_INDEX",
        ContextGlobalProxyIndex = "Context::GLOBAL_PROXY_INDEX",

        /// A native context's `%Error%`, whose initial map's first
        /// descriptor is keyed by the private symbol every error keeps its
        /// stack under (`InstallError` in `bootstrapper.cc`).
        ContextErrorFunctionIndex = "Context::ERROR_FUNCTION_INDEX",

        /// What formatting an error's call sites compares with: a native
        /// context's `Promise.all`, `Promise.allSettled`, and `Promise.any`,
        /// which name an async frame by its promise's index; the `CallSite`
        /// constructor, whose prototype's `toString` writes a call site;
        /// and `%DataView%` and the initial `String.prototype`, whose
        /// builtins `Builtins::NameForStackTrace` names.
        ContextPromiseAllIndex = "Context::PROMISE_ALL_INDEX",
        ContextPromiseAllSettledIndex = "Context::PROMISE_ALL_SETTLED_INDEX",
        ContextPromiseAnyIndex = "Context::PROMISE_ANY_INDEX",
        ContextCallSiteFunctionIndex = "Context::CALLSITE_FUNCTION_INDEX",
        ContextDataViewFunctionIndex = "Context::DATA_VIEW_FUN_INDEX",
        ContextInitialStringPrototypeIndex = "Context::INITIAL_STRING_PROTOTYPE_INDEX",

        /// Where a JavaScript frame keeps its argument count, from its
        /// frame pointer (`frame-constants.h`): with its receiver, which
        /// [`Constant::JSArgcReceiverSlots`] counts.
        StandardFrameArgCOffset = "StandardFrameConstants::kArgCOffset",

        /// An unoptimized frame's slots from its frame pointer: its first
        /// register, where its arguments begin above its return address
        /// (`v8dbg_off_fp_args`), and its feedback vector.
        UnoptimizedFrameRegisterFileFromFp = "UnoptimizedFrameConstants::kRegisterFileFromFp",
        UnoptimizedFrameFirstParamFromFp = "UnoptimizedFrameConstants::kFirstParamFromFp",
        UnoptimizedFrameFeedbackVectorFromFp = "UnoptimizedFrameConstants::kFeedbackVectorFromFp",

        /// How many of a JavaScript call's argument count are its receiver
        /// (`globals.h`).
        JSArgcReceiverSlots = "internal::kJSArgcReceiverSlots",
    }
}

impl Constant {
    /// Whether the constant comes from V8's sources, its Torque output or
    /// its headers, rather than from the binary's `v8dbg_` constants: an
    /// instance type, a bit field, a class's or namespace's constant, or a
    /// struct's layout.
    #[must_use]
    pub fn is_torque(self) -> bool {
        let name = self.name();
        name.ends_with("_TYPE") || name.contains("::") || name.contains('(')
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

    /// Where the JS dispatch table is: a `DISPATCH_` kind, with its
    /// address and offset; or why the module's code does not say, a
    /// string, or [`NONE`].
    pub dispatch_kind: U16,
    pub dispatch_reserved: U16,
    pub dispatch_problem: U32,
    pub dispatch_address: U64,
    pub dispatch_offset: U64,

    /// Where the embedded blob's bounds are kept: its code, its code's
    /// size, its data, and its data's size; or why the module's code does
    /// not say, a string, or [`NONE`].
    pub blob_problem: U32,
    pub blob: [U64; 4],

    /// The Node release that built the module: its major, minor, and
    /// patch numbers; or why the module does not say, a string, or
    /// [`NONE`].
    pub node_problem: U32,
    pub node: [U32; 3],
    pub inspector_problem: U32,
    pub inspector: [U64; 20],
    pub offsets: [U32; Field::COUNT],
    pub constants: [I64; Constant::COUNT],
}

/// The version has no JS dispatch table.
const DISPATCH_NONE: u16 = 0;

/// [`DispatchTable::Isolate`].
const DISPATCH_ISOLATE: u16 = 1;

/// [`DispatchTable::Group`].
const DISPATCH_GROUP: u16 = 2;

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

/// A Node release, as `napi_node_version` holds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct NodeRelease {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl std::fmt::Display for NodeRelease {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)
    }
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

/// Where a process keeps the base of its JS dispatch table, the table
/// of 13.6's leaptiering through which a function's
/// [`Field::JSFunctionDispatchHandle`] finds its code: as the module's
/// `ExternalReference::js_dispatch_table_address` reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchTable {
    /// The base is the word `offset` bytes into each isolate, as in 14.6.
    Isolate { offset: u64 },

    /// The base is the word `offset` bytes into the isolate group the
    /// pointer variable at `group` points to, as in 13.6.
    Group { group: ImageAddress, offset: u64 },
}

/// The variable each thread keeps its current isolate in, which a shared
/// library names only in its debug package.
pub const CURRENT_ISOLATE: &str = "_ZN2v88internal18g_current_isolate_E";

/// Where a process keeps the bounds of V8's embedded blob, the builtins
/// compiled ahead of time, which V8 sets when it starts: the variables
/// `v8::Isolate::GetEmbeddedCodeRange` reads. `code` and `data` hold
/// pointers, and `code_size` and `data_size` 32-bit sizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmbeddedBlob {
    pub code: ImageAddress,
    pub code_size: ImageAddress,
    pub data: ImageAddress,
    pub data_size: ImageAddress,
}

/// What a module's loader found of its V8 runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct V8Runtime {
    pub version: V8Version,
    pub flags: u16,

    /// The layout, or why the runtime cannot be read, as for a version
    /// microscope has no layout of.
    pub layout: Result<V8Layout, std::sync::Arc<str>>,

    /// Where the JS dispatch table is; `None` for a version without one,
    /// whose functions hold their code. An error, which leaves the rest of
    /// the runtime readable, when the module's code does not say.
    pub dispatch_table: Result<Option<DispatchTable>, std::sync::Arc<str>>,

    /// Where the embedded blob's bounds are, or why the module's code does
    /// not say, which leaves the rest of the runtime readable.
    pub embedded_blob: Result<EmbeddedBlob, std::sync::Arc<str>>,

    /// The Node release that built the module, or why its code does not
    /// say, as for a V8 that Node did not build.
    pub node: Result<NodeRelease, std::sync::Arc<str>>,
    pub inspector: Result<NodeInspector, std::sync::Arc<str>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeInspector {
    pub registry: ImageAddress,
    pub signal_handler: ImageAddress,
    pub vector: bool,
    pub io: u64,
    pub host_port: u64,
    pub host: u64,
    pub port: u64,
    pub id: u64,
    pub close: Option<(ImageAddress, ImageAddress)>,
    pub startup: Option<InspectorStartup>,
    pub targets: InspectorTargets,
}

impl NodeInspector {
    fn decode(words: [u64; 20]) -> Self {
        let [
            registry,
            vector,
            io,
            host_port,
            host,
            port,
            id,
            signal_handler,
            dispatch,
            stop,
            before_wait,
            after_wait,
            check,
            async_offset,
            target_entry,
            target_source,
            print_start,
            print_end,
            list,
            upgrade,
        ] = words;
        Self {
            registry: ImageAddress::new(registry),
            signal_handler: ImageAddress::new(signal_handler),
            vector: vector != 0,
            io,
            host_port,
            host,
            port,
            id,
            close: (dispatch != 0)
                .then_some((ImageAddress::new(dispatch), ImageAddress::new(stop))),
            startup: (before_wait != 0).then_some(InspectorStartup {
                before_wait: ImageAddress::new(before_wait),
                after_wait: ImageAddress::new(after_wait),
                check: ImageAddress::new(check),
                async_offset,
            }),
            targets: InspectorTargets {
                entry: ImageAddress::new(target_entry),
                source: target_source,
                hidden: [print_start, print_end, list].map(ImageAddress::new),
                upgrade: ImageAddress::new(upgrade),
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InspectorTargets {
    pub entry: ImageAddress,
    pub source: u64,
    pub hidden: [ImageAddress; 3],
    pub upgrade: ImageAddress,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InspectorStartup {
    pub before_wait: ImageAddress,
    pub after_wait: ImageAddress,
    pub check: ImageAddress,
    pub async_offset: u64,
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
        .map(|runtime| encode(strings, runtime))
        .transpose()?;

    builder.owned_table(records.into_iter().collect());
    Ok(())
}

fn encode(strings: &mut StringsBuilder, runtime: &V8Runtime) -> Result<V8RuntimeRecord, TooLarge> {
    let mut problem = |problem: &str| strings.push(problem).map(|id| id.0).ok_or(TooLarge);
    let (layout, layout_problem) = match &runtime.layout {
        Ok(layout) => (layout.clone(), NONE),
        Err(error) => (V8Layout::default(), problem(error)?),
    };
    let (dispatch_kind, dispatch_address, dispatch_offset, dispatch_problem) =
        match &runtime.dispatch_table {
            Ok(None) => (DISPATCH_NONE, 0, 0, NONE),
            Ok(Some(DispatchTable::Isolate { offset })) => (DISPATCH_ISOLATE, 0, *offset, NONE),
            Ok(Some(DispatchTable::Group { group, offset })) => {
                (DISPATCH_GROUP, group.get(), *offset, NONE)
            }
            Err(error) => (DISPATCH_NONE, 0, 0, problem(error)?),
        };
    let (blob, blob_problem) = match &runtime.embedded_blob {
        Ok(blob) => (
            [blob.code, blob.code_size, blob.data, blob.data_size].map(|at| U64::new(at.get())),
            NONE,
        ),
        Err(error) => ([U64::new(0); 4], problem(error)?),
    };
    let (node, node_problem) = match &runtime.node {
        Ok(release) => (
            [release.major, release.minor, release.patch].map(U32::new),
            NONE,
        ),
        Err(error) => ([U32::new(0); 3], problem(error)?),
    };
    let (inspector, inspector_problem) = match &runtime.inspector {
        Ok(layout) => (
            [
                layout.registry.get(),
                u64::from(layout.vector),
                layout.io,
                layout.host_port,
                layout.host,
                layout.port,
                layout.id,
                layout.signal_handler.get(),
                layout.close.map_or(0, |(dispatch, _)| dispatch.get()),
                layout.close.map_or(0, |(_, stop)| stop.get()),
                layout
                    .startup
                    .map_or(0, |startup| startup.before_wait.get()),
                layout.startup.map_or(0, |startup| startup.after_wait.get()),
                layout.startup.map_or(0, |startup| startup.check.get()),
                layout.startup.map_or(0, |startup| startup.async_offset),
                layout.targets.entry.get(),
                layout.targets.source,
                layout.targets.hidden[0].get(),
                layout.targets.hidden[1].get(),
                layout.targets.hidden[2].get(),
                layout.targets.upgrade.get(),
            ]
            .map(U64::new),
            NONE,
        ),
        Err(error) => ([U64::new(0); 20], problem(error)?),
    };

    Ok(V8RuntimeRecord {
        major: runtime.version.major.into(),
        minor: runtime.version.minor.into(),
        build: runtime.version.build.into(),
        patch: runtime.version.patch.into(),
        flags: runtime.flags.into(),
        reserved: 0.into(),
        problem: layout_problem.into(),
        dispatch_kind: dispatch_kind.into(),
        dispatch_reserved: 0.into(),
        dispatch_problem: dispatch_problem.into(),
        dispatch_address: dispatch_address.into(),
        dispatch_offset: dispatch_offset.into(),
        blob_problem: blob_problem.into(),
        blob,
        node_problem: node_problem.into(),
        node,
        inspector_problem: inspector_problem.into(),
        inspector,
        offsets: layout.offsets.map(U32::new),
        constants: layout.constants.map(I64::new),
    })
}

/// The V8 runtime of a validated image, if it has one.
#[must_use]
pub fn runtime(image: &Image) -> Option<V8Runtime> {
    let record: &V8RuntimeRecord = image.table().first()?;
    let problem = |id: u32| -> std::sync::Arc<str> { image.strings().get(StrId(id)).into() };
    let layout = match record.problem.get() {
        NONE => Ok(V8Layout {
            offsets: record.offsets.map(U32::get),
            constants: record.constants.map(I64::get),
        }),
        id => Err(problem(id)),
    };
    let offset = record.dispatch_offset.get();
    let dispatch_table = match (record.dispatch_problem.get(), record.dispatch_kind.get()) {
        (NONE, DISPATCH_ISOLATE) => Ok(Some(DispatchTable::Isolate { offset })),
        (NONE, DISPATCH_GROUP) => Ok(Some(DispatchTable::Group {
            group: ImageAddress::new(record.dispatch_address.get()),
            offset,
        })),
        (NONE, _) => Ok(None),
        (id, _) => Err(problem(id)),
    };
    let [code, code_size, data, data_size] = record.blob.map(|at| ImageAddress::new(at.get()));
    let embedded_blob = match record.blob_problem.get() {
        NONE => Ok(EmbeddedBlob {
            code,
            code_size,
            data,
            data_size,
        }),
        id => Err(problem(id)),
    };
    let [major, minor, patch] = record.node.map(U32::get);
    let node = match record.node_problem.get() {
        NONE => Ok(NodeRelease {
            major,
            minor,
            patch,
        }),
        id => Err(problem(id)),
    };
    let inspector = match record.inspector_problem.get() {
        NONE => Ok(NodeInspector::decode(record.inspector.map(U64::get))),
        id => Err(problem(id)),
    };

    Some(V8Runtime {
        version: V8Version {
            major: record.major.get(),
            minor: record.minor.get(),
            build: record.build.get(),
            patch: record.patch.get(),
        },
        flags: record.flags.get(),
        layout,
        dispatch_table,
        embedded_blob,
        node,
        inspector,
    })
}

/// Checks that an image has at most one runtime, which sets only the
/// flags and the kind of dispatch table this format defines, whose
/// problems name strings, and which leaves no address where it has a
/// problem instead.
pub(super) fn validate(image: &Image) -> Result<(), String> {
    let records: &[V8RuntimeRecord] = image.table();
    if records.len() > 1 {
        return Err(format!("{} V8 runtimes in one image", records.len()));
    }

    let Some(record) = records.first() else {
        return Ok(());
    };

    if record.flags.get() & !DYNAMIC_SYMBOLS != 0
        || record.reserved.get() != 0
        || record.dispatch_reserved.get() != 0
    {
        return Err("the V8 runtime sets flags it does not define".to_owned());
    }

    for problem in [
        record.problem.get(),
        record.dispatch_problem.get(),
        record.blob_problem.get(),
        record.node_problem.get(),
        record.inspector_problem.get(),
    ] {
        if problem != NONE && !image.strings().contains(StrId(problem)) {
            return Err(format!("the V8 runtime's problem {problem} is no string"));
        }
    }

    let mut target_sites = [14, 16, 17, 18, 19].map(|index| record.inspector[index].get());
    target_sites.sort_unstable();
    if record.inspector[1].get() > 1
        || (record.inspector[8].get() == 0) != (record.inspector[9].get() == 0)
        || (record.inspector_problem.get() == NONE
            && (record.inspector[15].get() > 4096
                || target_sites[0] == 0
                || target_sites.windows(2).any(|pair| pair[0] == pair[1])))
        || (record.inspector_problem.get() != NONE
            && record.inspector.iter().any(|value| value.get() != 0))
    {
        return Err("the V8 runtime has an invalid Node inspector layout".to_owned());
    }

    let kind = record.dispatch_kind.get();
    let address = record.dispatch_address.get();
    let offset = record.dispatch_offset.get();
    let dispatch_valid = match kind {
        DISPATCH_NONE => address == 0 && offset == 0,
        DISPATCH_ISOLATE => address == 0 && record.dispatch_problem.get() == NONE,
        DISPATCH_GROUP => record.dispatch_problem.get() == NONE,
        _ => false,
    };
    if !dispatch_valid {
        return Err(format!(
            "the V8 runtime's dispatch table of kind {kind} at {address:#x}+{offset:#x} \
             is not one this format defines"
        ));
    }

    if record.blob_problem.get() != NONE && record.blob.iter().any(|at| at.get() != 0) {
        return Err("the V8 runtime places its embedded blob and says it cannot".to_owned());
    }

    if record.node_problem.get() != NONE && record.node.iter().any(|number| number.get() != 0) {
        return Err("the V8 runtime gives a Node release and says it cannot".to_owned());
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
