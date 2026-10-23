// @generated from V8's src/deoptimizer/translation-opcode.h for each
// supported version, which MICROSCOPE_V8_TORQUE holds, by
// `debug_info::v8::conformance::translations_match_torque`. Do not
// edit; rerun that test with MICROSCOPE_UPDATE_V8=1.
//
// The opcodes are V8's, which carries this notice:
//
// Copyright 2014, the V8 project authors. All rights reserved.
// Redistribution and use in source and binary forms, with or without
// modification, are permitted provided that the following conditions are
// met:
//
//     * Redistributions of source code must retain the above copyright
//       notice, this list of conditions and the following disclaimer.
//     * Redistributions in binary form must reproduce the above
//       copyright notice, this list of conditions and the following
//       disclaimer in the documentation and/or other materials provided
//       with the distribution.
//     * Neither the name of Google Inc. nor the names of its
//       contributors may be used to endorse or promote products derived
//       from this software without specific prior written permission.
//
// THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS
// "AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT
// LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR
// A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT
// OWNER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
// SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT
// LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE,
// DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY
// THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
// (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
// OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

use super::TranslationOpcode as T;
use super::Translations;

pub(super) const TRANSLATIONS: &[Translations] = &[
    Translations {
        major: 12,
        minor: 4,
        js_frame_opcodes: 4,
        frame_opcodes: 10,
        opcodes: &[
            (T::InterpretedFrameWithReturn, 5),
            (T::InterpretedFrameWithoutReturn, 3),
            (T::JavaScriptBuiltinContinuationFrame, 3),
            (T::JavaScriptBuiltinContinuationWithCatchFrame, 3),
            (T::ConstructCreateStubFrame, 2),
            (T::ConstructInvokeStubFrame, 1),
            (T::BuiltinContinuationFrame, 3),
            (T::JsToWasmBuiltinContinuationFrame, 4),
            (T::WasmInlinedIntoJsFrame, 3),
            (T::InlinedExtraArguments, 2),
            (T::ArgumentsElements, 1),
            (T::ArgumentsLength, 0),
            (T::BeginWithoutFeedback, 3),
            (T::BeginWithFeedback, 3),
            (T::BoolRegister, 1),
            (T::BoolStackSlot, 1),
            (T::CapturedObject, 1),
            (T::DoubleRegister, 1),
            (T::DoubleStackSlot, 1),
            (T::HoleyDoubleRegister, 1),
            (T::HoleyDoubleStackSlot, 1),
            (T::DuplicatedObject, 1),
            (T::FloatRegister, 1),
            (T::FloatStackSlot, 1),
            (T::Int32Register, 1),
            (T::Int32StackSlot, 1),
            (T::Int64Register, 1),
            (T::Int64StackSlot, 1),
            (T::SignedBigInt64Register, 1),
            (T::SignedBigInt64StackSlot, 1),
            (T::UnsignedBigInt64Register, 1),
            (T::UnsignedBigInt64StackSlot, 1),
            (T::OptimizedOut, 0),
            (T::Literal, 1),
            (T::Register, 1),
            (T::TaggedStackSlot, 1),
            (T::Uint32Register, 1),
            (T::Uint32StackSlot, 1),
            (T::UpdateFeedback, 2),
            (T::MatchPreviousTranslation, 1),
        ],
    },
    Translations {
        major: 13,
        minor: 6,
        js_frame_opcodes: 4,
        frame_opcodes: 11,
        opcodes: &[
            (T::InterpretedFrameWithReturn, 6),
            (T::InterpretedFrameWithoutReturn, 4),
            (T::JavaScriptBuiltinContinuationFrame, 3),
            (T::JavaScriptBuiltinContinuationWithCatchFrame, 3),
            (T::ConstructCreateStubFrame, 2),
            (T::ConstructInvokeStubFrame, 1),
            (T::BuiltinContinuationFrame, 3),
            (T::JsToWasmBuiltinContinuationFrame, 4),
            (T::WasmInlinedIntoJsFrame, 3),
            (T::LiftoffFrame, 3),
            (T::InlinedExtraArguments, 3),
            (T::ArgumentsElements, 1),
            (T::ArgumentsLength, 0),
            (T::RestLength, 0),
            (T::BeginWithoutFeedback, 3),
            (T::BeginWithFeedback, 3),
            (T::BoolRegister, 1),
            (T::BoolStackSlot, 1),
            (T::CapturedObject, 1),
            (T::StringConcat, 0),
            (T::DoubleRegister, 1),
            (T::DoubleStackSlot, 1),
            (T::Simd128StackSlot, 1),
            (T::HoleyDoubleRegister, 1),
            (T::HoleyDoubleStackSlot, 1),
            (T::Simd128Register, 1),
            (T::DuplicatedObject, 1),
            (T::FloatRegister, 1),
            (T::FloatStackSlot, 1),
            (T::Int32Register, 1),
            (T::Int32StackSlot, 1),
            (T::Int64Register, 1),
            (T::Int64StackSlot, 1),
            (T::SignedBigInt64Register, 1),
            (T::SignedBigInt64StackSlot, 1),
            (T::UnsignedBigInt64Register, 1),
            (T::UnsignedBigInt64StackSlot, 1),
            (T::OptimizedOut, 0),
            (T::Literal, 1),
            (T::Register, 1),
            (T::TaggedStackSlot, 1),
            (T::Uint32Register, 1),
            (T::Uint32StackSlot, 1),
            (T::UpdateFeedback, 2),
            (T::MatchPreviousTranslation, 1),
        ],
    },
    Translations {
        major: 14,
        minor: 6,
        js_frame_opcodes: 4,
        frame_opcodes: 11,
        opcodes: &[
            (T::InterpretedFrameWithReturn, 6),
            (T::InterpretedFrameWithoutReturn, 4),
            (T::JavaScriptBuiltinContinuationFrame, 3),
            (T::JavaScriptBuiltinContinuationWithCatchFrame, 3),
            (T::ConstructCreateStubFrame, 2),
            (T::ConstructInvokeStubFrame, 1),
            (T::BuiltinContinuationFrame, 3),
            (T::JsToWasmBuiltinContinuationFrame, 4),
            (T::WasmInlinedIntoJsFrame, 3),
            (T::LiftoffFrame, 3),
            (T::InlinedExtraArguments, 3),
            (T::ArgumentsElements, 1),
            (T::ArgumentsLength, 0),
            (T::RestLength, 0),
            (T::BeginWithoutFeedback, 3),
            (T::BeginWithFeedback, 3),
            (T::BoolRegister, 1),
            (T::BoolStackSlot, 1),
            (T::CapturedObject, 1),
            (T::StringConcat, 0),
            (T::DoubleRegister, 1),
            (T::DoubleStackSlot, 1),
            (T::Simd128StackSlot, 1),
            (T::HoleyDoubleRegister, 1),
            (T::HoleyDoubleStackSlot, 1),
            (T::Simd128Register, 1),
            (T::DuplicatedObject, 1),
            (T::FloatRegister, 1),
            (T::FloatStackSlot, 1),
            (T::Int32Register, 1),
            (T::Int32StackSlot, 1),
            (T::Int64Register, 1),
            (T::Int64StackSlot, 1),
            (T::SignedBigInt64Register, 1),
            (T::SignedBigInt64StackSlot, 1),
            (T::UnsignedBigInt64Register, 1),
            (T::UnsignedBigInt64StackSlot, 1),
            (T::OptimizedOut, 0),
            (T::Literal, 1),
            (T::Register, 1),
            (T::TaggedStackSlot, 1),
            (T::Uint32Register, 1),
            (T::Uint32StackSlot, 1),
            (T::UpdateFeedback, 2),
            (T::MatchPreviousTranslation, 1),
        ],
    },
];
