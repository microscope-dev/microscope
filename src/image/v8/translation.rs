use std::sync::Arc;

use super::V8Version;

#[rustfmt::skip]
#[path = "translation.gen.rs"]
mod generated;

macro_rules! opcodes {
    ($($variant:ident = $name:literal,)*) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum TranslationOpcode {
            $($variant,)*
        }

        impl TranslationOpcode {
            #[must_use]
            pub const fn name(self) -> &'static str {
                match self {
                    $(Self::$variant => $name,)*
                }
            }

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

#[expect(
    clippy::indexing_slicing,
    reason = "evaluated at compile time, where an index out of bounds fails the build"
)]
const _: () = {
    let mut at = 0;
    while at < generated::TRANSLATIONS.len() {
        let own = &generated::TRANSLATIONS[at];
        assert!(
            own.js_frame_opcodes <= own.frame_opcodes,
            "a version has more JavaScript frame opcodes than frame opcodes",
        );
        assert!(
            own.frame_opcodes as usize <= own.opcodes.len(),
            "a version has more frame opcodes than opcodes",
        );
        assert!(
            own.opcodes.len() <= u8::MAX as usize + 1,
            "a version has more opcodes than an opcode byte holds",
        );

        let mut number = 0;
        while number < own.opcodes.len() {
            let mut later = number + 1;
            while later < own.opcodes.len() {
                assert!(
                    own.opcodes[number].0 as usize != own.opcodes[later].0 as usize,
                    "a version numbers one opcode twice",
                );
                later += 1;
            }
            number += 1;
        }

        let mut later = at + 1;
        while later < generated::TRANSLATIONS.len() {
            let other = &generated::TRANSLATIONS[later];
            assert!(
                own.major != other.major || own.minor != other.minor,
                "a version has two rows of translation opcodes",
            );
            later += 1;
        }

        at += 1;
    }
};

#[derive(Debug)]
pub struct Translations {
    pub major: u32,
    pub minor: u32,
    pub js_frame_opcodes: u8,
    pub frame_opcodes: u8,
    pub opcodes: &'static [(TranslationOpcode, u8)],
}

impl Translations {
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

    #[must_use]
    pub const fn count(&self) -> usize {
        self.opcodes.len()
    }

    #[must_use]
    pub fn opcode(&self, number: usize) -> Option<(TranslationOpcode, u8)> {
        self.opcodes.get(number).copied()
    }

    #[must_use]
    pub fn number(&self, opcode: TranslationOpcode) -> Option<usize> {
        self.opcodes.iter().position(|&(own, _)| own == opcode)
    }

    #[must_use]
    pub fn is_js_frame(&self, number: usize) -> bool {
        number < usize::from(self.js_frame_opcodes)
    }

    #[must_use]
    pub fn is_frame(&self, number: usize) -> bool {
        number < usize::from(self.frame_opcodes)
    }
}
