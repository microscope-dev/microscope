//! Per-version deoptimization opcodes describing an optimized frame's
//! JavaScript frames. Each opcode byte is followed by VLQ-encoded operands.
//! `translation.gen.rs` records opcode numbers and operand counts from each
//! version's `translation-opcode.h`.

use std::sync::Arc;

use super::V8Version;

#[rustfmt::skip]
#[path = "translation.gen.rs"]
mod generated;

/// Defines [`TranslationOpcode`], each with V8's name.
macro_rules! opcodes {
    ($($variant:ident = $name:literal,)*) => {
        /// A translation opcode, by V8's name; 12.4's `JAVA_SCRIPT_` is
        /// spelled `JAVASCRIPT_`, as later versions do.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum TranslationOpcode {
            $($variant,)*
        }

        impl TranslationOpcode {
            /// The opcode's name, as V8 spells it.
            #[must_use]
            pub const fn name(self) -> &'static str {
                match self {
                    $(Self::$variant => $name,)*
                }
            }

            /// Looks up an opcode by its V8 name.
            #[must_use]
            #[cfg_attr(
                not(test),
                expect(dead_code, reason = "the generator of the opcodes names them by it")
            )]
            pub fn named(name: &str) -> Option<Self> {
                [$(Self::$variant,)*].into_iter().find(|opcode| opcode.name() == name)
            }
        }
    };
}

opcodes! {
    InterpretedFrameWithReturn = "INTERPRETED_FRAME_WITH_RETURN",
    InterpretedFrameWithoutReturn = "INTERPRETED_FRAME_WITHOUT_RETURN",
    JavaScriptBuiltinContinuationFrame = "JAVASCRIPT_BUILTIN_CONTINUATION_FRAME",
    JavaScriptBuiltinContinuationWithCatchFrame = "JAVASCRIPT_BUILTIN_CONTINUATION_WITH_CATCH_FRAME",
    ConstructCreateStubFrame = "CONSTRUCT_CREATE_STUB_FRAME",
    ConstructInvokeStubFrame = "CONSTRUCT_INVOKE_STUB_FRAME",
    BuiltinContinuationFrame = "BUILTIN_CONTINUATION_FRAME",
    JsToWasmBuiltinContinuationFrame = "JS_TO_WASM_BUILTIN_CONTINUATION_FRAME",
    WasmInlinedIntoJsFrame = "WASM_INLINED_INTO_JS_FRAME",
    LiftoffFrame = "LIFTOFF_FRAME",
    InlinedExtraArguments = "INLINED_EXTRA_ARGUMENTS",
    ArgumentsElements = "ARGUMENTS_ELEMENTS",
    ArgumentsLength = "ARGUMENTS_LENGTH",
    RestLength = "REST_LENGTH",
    BeginWithoutFeedback = "BEGIN_WITHOUT_FEEDBACK",
    BeginWithFeedback = "BEGIN_WITH_FEEDBACK",
    BoolRegister = "BOOL_REGISTER",
    BoolStackSlot = "BOOL_STACK_SLOT",
    CapturedObject = "CAPTURED_OBJECT",
    StringConcat = "STRING_CONCAT",
    DoubleRegister = "DOUBLE_REGISTER",
    DoubleStackSlot = "DOUBLE_STACK_SLOT",
    Simd128StackSlot = "SIMD128_STACK_SLOT",
    HoleyDoubleRegister = "HOLEY_DOUBLE_REGISTER",
    HoleyDoubleStackSlot = "HOLEY_DOUBLE_STACK_SLOT",
    Simd128Register = "SIMD128_REGISTER",
    DuplicatedObject = "DUPLICATED_OBJECT",
    FloatRegister = "FLOAT_REGISTER",
    FloatStackSlot = "FLOAT_STACK_SLOT",
    Int32Register = "INT32_REGISTER",
    Int32StackSlot = "INT32_STACK_SLOT",
    Int64Register = "INT64_REGISTER",
    Int64StackSlot = "INT64_STACK_SLOT",
    SignedBigInt64Register = "SIGNED_BIGINT64_REGISTER",
    SignedBigInt64StackSlot = "SIGNED_BIGINT64_STACK_SLOT",
    UnsignedBigInt64Register = "UNSIGNED_BIGINT64_REGISTER",
    UnsignedBigInt64StackSlot = "UNSIGNED_BIGINT64_STACK_SLOT",
    OptimizedOut = "OPTIMIZED_OUT",
    Literal = "LITERAL",
    Register = "REGISTER",
    TaggedStackSlot = "TAGGED_STACK_SLOT",
    Uint32Register = "UINT32_REGISTER",
    Uint32StackSlot = "UINT32_STACK_SLOT",
    UpdateFeedback = "UPDATE_FEEDBACK",
    MatchPreviousTranslation = "MATCH_PREVIOUS_TRANSLATION",
}

/// One version's translation opcodes: each opcode's number is its place in
/// `opcodes`, with its operand count. The first `js_frame_opcodes` begin a
/// JavaScript frame, and the first `frame_opcodes` a frame of any kind.
#[derive(Debug)]
pub struct Translations {
    pub major: u32,
    pub minor: u32,
    pub js_frame_opcodes: u8,
    pub frame_opcodes: u8,
    pub opcodes: &'static [(TranslationOpcode, u8)],
}

impl Translations {
    /// The opcodes of V8 `version`, or why microscope has none.
    pub fn of(version: V8Version) -> Result<&'static Self, Arc<str>> {
        generated::TRANSLATIONS
            .iter()
            .find(|own| (own.major, own.minor) == (version.major, version.minor))
            .ok_or_else(|| {
                format!(
                    "V8 {}.{} has no translation opcodes in this build of microscope",
                    version.major, version.minor
                )
                .into()
            })
    }

    /// `kNumTranslationOpcodes`: a byte of at least this many is an
    /// implicit `MATCH_PREVIOUS_TRANSLATION`.
    #[must_use]
    pub const fn count(&self) -> usize {
        self.opcodes.len()
    }

    /// The opcode numbered `number`, with its operand count, or `None` for
    /// a number past the last.
    #[must_use]
    pub fn opcode(&self, number: usize) -> Option<(TranslationOpcode, u8)> {
        self.opcodes.get(number).copied()
    }

    /// The number of `opcode`, or `None` for one the version does not have.
    #[must_use]
    pub fn number(&self, opcode: TranslationOpcode) -> Option<usize> {
        self.opcodes.iter().position(|&(own, _)| own == opcode)
    }

    /// Whether the opcode numbered `number` begins a JavaScript frame:
    /// an interpreted frame or a JavaScript builtin's continuation.
    #[must_use]
    pub fn is_js_frame(&self, number: usize) -> bool {
        number < usize::from(self.js_frame_opcodes)
    }

    /// Whether the opcode numbered `number` begins a frame.
    #[must_use]
    pub fn is_frame(&self, number: usize) -> bool {
        number < usize::from(self.frame_opcodes)
    }
}
