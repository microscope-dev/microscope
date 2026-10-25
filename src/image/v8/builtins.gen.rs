// @generated from the builtin_metadata table of each supported V8
// version's official Node build in MICROSCOPE_NODE, by
// `debug_info::v8::conformance::builtin_ids_match_release`. Do not
// edit; rerun that test with MICROSCOPE_UPDATE_V8=1.
//
// The numbers are V8's, which carries this notice:
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

use super::{Builtin, Numbers};

/// Each version's major and minor version, how many builtins it has,
/// and the number of each builtin an await chain is read by.
pub(super) const BUILTINS: &[(u32, u32, u32, Numbers)] = &[
    (14, 6, 2415, &[
        (Builtin::AsyncFunctionAwaitResolveClosure, 266),
        (Builtin::AsyncFunctionAwaitRejectClosure, 265),
        (Builtin::AsyncGeneratorAwaitResolveClosure, 794),
        (Builtin::AsyncGeneratorAwaitRejectClosure, 795),
        (Builtin::AsyncGeneratorYieldWithAwaitResolveClosure, 796),
        (Builtin::PromiseAllResolveElementClosure, 1162),
        (Builtin::PromiseAllSettledResolveElementClosure, 1163),
        (Builtin::PromiseAllSettledRejectElementClosure, 1164),
        (Builtin::PromiseAnyRejectElementClosure, 1165),
        (Builtin::PromiseCapabilityDefaultResolve, 1154),
    ]),
    (13, 6, 2384, &[
        (Builtin::AsyncFunctionAwaitResolveClosure, 258),
        (Builtin::AsyncFunctionAwaitRejectClosure, 257),
        (Builtin::AsyncGeneratorAwaitResolveClosure, 739),
        (Builtin::AsyncGeneratorAwaitRejectClosure, 740),
        (Builtin::AsyncGeneratorYieldWithAwaitResolveClosure, 741),
        (Builtin::PromiseAllResolveElementClosure, 1382),
        (Builtin::PromiseAllSettledResolveElementClosure, 1383),
        (Builtin::PromiseAllSettledRejectElementClosure, 1384),
        (Builtin::PromiseAnyRejectElementClosure, 1385),
        (Builtin::PromiseCapabilityDefaultResolve, 1374),
    ]),
    (12, 4, 2304, &[
        (Builtin::AsyncFunctionAwaitResolveClosure, 243),
        (Builtin::AsyncFunctionAwaitRejectClosure, 242),
        (Builtin::AsyncGeneratorAwaitResolveClosure, 692),
        (Builtin::AsyncGeneratorAwaitRejectClosure, 693),
        (Builtin::AsyncGeneratorYieldWithAwaitResolveClosure, 694),
        (Builtin::PromiseAllResolveElementClosure, 1329),
        (Builtin::PromiseAllSettledResolveElementClosure, 1330),
        (Builtin::PromiseAllSettledRejectElementClosure, 1331),
        (Builtin::PromiseAnyRejectElementClosure, 1332),
        (Builtin::PromiseCapabilityDefaultResolve, 1322),
    ]),
];
