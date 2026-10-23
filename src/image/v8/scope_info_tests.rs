//! The translated `ScopeInfo` slices against layouts derived by hand from
//! each version's `scope-info.tq`, on synthetic objects.

use super::{ScopeInfoSlice, Slice, V8Version, scope_info_slice};

const VERSIONS: [(u32, u32); 3] = [(12, 4), (13, 6), (14, 6)];

/// A word no field the slices read holds: not a Smi, so a read of the
/// wrong word fails or decodes as something else.
const FILLER: u64 = 0x0bad_f00d_0000_0011;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Type {
    Class,
    Eval,
    Function,
    Module,
    Script,
    Catch,
    Block,
    With,
    ShadowRealm,
    ReplMode,
}

const TYPES: [Type; 10] = [
    Type::Class,
    Type::Eval,
    Type::Function,
    Type::Module,
    Type::Script,
    Type::Catch,
    Type::Block,
    Type::With,
    Type::ShadowRealm,
    Type::ReplMode,
];

/// `ScopeType`'s value, from `globals.h`: 13.6 moved `SCRIPT_SCOPE` first
/// and added `REPL_MODE_SCOPE`.
fn type_value(version: (u32, u32), kind: Type) -> Option<u32> {
    let order: &[Type] = if version == (12, 4) {
        &TYPES[..9]
    } else {
        &[
            Type::Script,
            Type::ReplMode,
            Type::Class,
            Type::Eval,
            Type::Function,
            Type::Module,
            Type::Catch,
            Type::Block,
            Type::With,
            Type::ShadowRealm,
        ]
    };
    let index = order.iter().position(|own| *own == kind)?;
    u32::try_from(index).ok()
}

/// What the conditions of `scope-info.tq` read.
#[derive(Debug, Clone, Copy)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each is a flag of ScopeFlags"
)]
struct Scope {
    kind: Type,
    context_locals: u64,
    module_variables: u64,
    sloppy_eval: bool,
    saved_class_variable: bool,

    /// `VariableAllocationInfo`: `NONE` is 0.
    function_variable: u32,
    inferred_name: bool,
    outer: bool,

    /// 12.4's `has_locals_block_list` and `is_empty`.
    locals_block_list: bool,
    empty: bool,
}

impl Scope {
    const fn new(kind: Type, context_locals: u64) -> Self {
        Self {
            kind,
            context_locals,
            module_variables: 0,
            sloppy_eval: false,
            saved_class_variable: false,
            function_variable: 0,
            inferred_name: false,
            outer: false,
            locals_block_list: false,
            empty: false,
        }
    }

    /// `ScopeFlags`, whose bits every version places alike up to bit 27.
    fn flags(&self, version: (u32, u32)) -> u32 {
        let mut flags = type_value(version, self.kind).expect("a type the version has");
        flags |= u32::from(self.sloppy_eval) << 4;
        flags |= u32::from(self.saved_class_variable) << 10;
        flags |= self.function_variable << 12;
        flags |= u32::from(self.inferred_name) << 14;
        flags |= u32::from(self.outer) << 22;
        if version == (12, 4) {
            flags |= u32::from(self.locals_block_list) << 28;
            flags |= u32::from(self.empty) << 29;
        }
        flags
    }

    /// The object's words: 12.4 keeps its flags as a Smi; later versions
    /// as a uint32 beside 4 bytes of padding, which hold filler here.
    fn memory(&self, version: (u32, u32)) -> Vec<u64> {
        let smi = |value: u64| value << 32;
        let mut words = vec![FILLER; 8];
        words[0] = 0x3a5e_0000_1001;
        if version == (12, 4) {
            words[1] = smi(u64::from(self.flags(version)));
        } else {
            words[1] = 0xdead_beef_0000_0000 | u64::from(self.flags(version));
        }

        words[2] = smi(1);
        words[3] = smi(self.context_locals);
        if self.kind == Type::Module {
            words[if version == (12, 4) { 4 } else { 6 }] = smi(self.module_variables);
        }
        words
    }
}

/// Each slice's place, laid out by hand from the version's
/// `scope-info.tq`: each part after the last, of its length in items of
/// its size in bytes.
fn expected(version: (u32, u32), scope: &Scope) -> Vec<(ScopeInfoSlice, Option<Slice>)> {
    use ScopeInfoSlice as S;

    let module = scope.kind == Type::Module;
    let n = scope.context_locals;
    let inline = n < 75;
    let positions = matches!(
        scope.kind,
        Type::Function | Type::Script | Type::Eval | Type::Module | Type::ShadowRealm
    ) || (!scope.empty && scope.kind == Type::Class);

    let mut parts = vec![
        (S::ModuleVariableCount, u64::from(module), 8),
        (S::ContextLocalNames, if inline { n } else { 0 }, 8),
        (S::ContextLocalNamesHashtable, u64::from(!inline), 8),
        (S::ContextLocalInfos, n, 8),
        (
            S::SavedClassVariableInfo,
            u64::from(scope.saved_class_variable),
            8,
        ),
        (
            S::FunctionVariableInfo,
            u64::from(scope.function_variable != 0),
            16,
        ),
        (S::InferredFunctionName, u64::from(scope.inferred_name), 8),
    ];
    if version == (12, 4) {
        parts.push((S::PositionInfo, u64::from(positions), 16));
    }
    parts.push((S::OuterScopeInfo, u64::from(scope.outer), 8));
    if version == (12, 4) {
        parts.push((S::LocalsBlockList, u64::from(scope.locals_block_list), 8));
    }
    parts.push((S::ModuleInfo, u64::from(module), 8));
    let module_variables = if module { scope.module_variables } else { 0 };
    parts.push((S::ModuleVariables, module_variables, 24));
    if version >= (13, 6) {
        parts.push((S::DependentCode, u64::from(scope.sloppy_eval), 8));
    }
    if version >= (14, 6) {
        let function = scope.kind == Type::Function;
        parts.push((S::UnusedParameterBits, u64::from(function), 8));
    }

    // 12.4's variable part begins after its flags and two counts; later
    // ones' after the fixed position info too.
    let mut offset = if version == (12, 4) { 32 } else { 48 };
    let mut places: Vec<_> = parts
        .into_iter()
        .map(|(slice, length, size)| {
            let place = Slice { offset, length };
            offset += length * size;
            (slice, Some(place))
        })
        .collect();
    for &slice in ScopeInfoSlice::ALL {
        if places.iter().all(|(own, _)| *own != slice) {
            places.push((slice, None));
        }
    }
    places
}

fn v8(version: (u32, u32)) -> V8Version {
    V8Version {
        major: version.0,
        minor: version.1,
        build: 0,
        patch: 0,
    }
}

/// The translated slice, read from `words`.
fn slice(
    version: (u32, u32),
    slice: ScopeInfoSlice,
    words: &[u64],
) -> Result<Option<Slice>, std::sync::Arc<str>> {
    scope_info_slice(v8(version), slice, &mut |offset| {
        assert_eq!(offset % 8, 0, "a read of a word at +{offset}");
        words.get(usize::try_from(offset / 8).ok()?).copied()
    })
}

fn check(version: (u32, u32), scope: &Scope) {
    let words = scope.memory(version);
    for (own, place) in expected(version, scope) {
        assert_eq!(
            slice(version, own, &words),
            Ok(place),
            "V8 {version:?}, {own:?} of {scope:?}"
        );
    }
}

/// Every scope type each version has, with context locals on both sides
/// of the inline limit, and every flag a slice's presence depends on.
#[test]
fn every_slice_is_where_scope_info_tq_puts_it() {
    let mut cases = 0;
    for version in VERSIONS {
        for kind in TYPES {
            if type_value(version, kind).is_none() {
                continue;
            }

            for context_locals in [0, 1, 74, 75, 76, 200] {
                for bits in 0u32..(1 << 7) {
                    // Only 12.4 reads the last two flags.
                    if version != (12, 4) && bits >= 32 {
                        continue;
                    }

                    for function_variable in [0, 1, 2] {
                        let mut scope = Scope::new(kind, context_locals);
                        scope.module_variables = u64::from(bits & 1) * 3;
                        scope.sloppy_eval = bits & 2 != 0;
                        scope.saved_class_variable = bits & 4 != 0;
                        scope.inferred_name = bits & 8 != 0;
                        scope.outer = bits & 16 != 0;
                        scope.locals_block_list = bits & 32 != 0;
                        scope.empty = bits & 64 != 0;
                        scope.function_variable = function_variable;
                        check(version, &scope);
                        cases += 1;
                    }
                }
            }
        }
    }

    assert_eq!(cases, (9 * 128 + 20 * 32) * 6 * 3);
}

/// A few places written out, so that a layout misread from
/// `scope-info.tq` alike by the oracle and the translation shows.
#[test]
fn slices_of_three_scopes_are_at_the_offsets_worked_by_hand() {
    use ScopeInfoSlice as S;

    // A 14.6 function with two context locals, a context-allocated
    // function variable, and an inferred name.
    let mut function = Scope::new(Type::Function, 2);
    function.function_variable = 2;
    function.inferred_name = true;
    let words = function.memory((14, 6));
    let at = |offset, length| Ok(Some(Slice { offset, length }));
    assert_eq!(slice((14, 6), S::ModuleVariableCount, &words), at(48, 0));
    assert_eq!(slice((14, 6), S::ContextLocalNames, &words), at(48, 2));
    assert_eq!(
        slice((14, 6), S::ContextLocalNamesHashtable, &words),
        at(64, 0)
    );
    assert_eq!(slice((14, 6), S::ContextLocalInfos, &words), at(64, 2));
    assert_eq!(slice((14, 6), S::FunctionVariableInfo, &words), at(80, 1));
    assert_eq!(slice((14, 6), S::InferredFunctionName, &words), at(96, 1));
    assert_eq!(slice((14, 6), S::OuterScopeInfo, &words), at(104, 0));
    assert_eq!(slice((14, 6), S::UnusedParameterBits, &words), at(104, 1));
    assert_eq!(slice((14, 6), S::PositionInfo, &words), Ok(None));

    // A 13.6 module with 80 context locals, past the inline limit, and
    // three module variables.
    let mut module = Scope::new(Type::Module, 80);
    module.module_variables = 3;
    let words = module.memory((13, 6));
    assert_eq!(slice((13, 6), S::ModuleVariableCount, &words), at(48, 1));
    assert_eq!(slice((13, 6), S::ContextLocalNames, &words), at(56, 0));
    assert_eq!(
        slice((13, 6), S::ContextLocalNamesHashtable, &words),
        at(56, 1)
    );
    assert_eq!(slice((13, 6), S::ContextLocalInfos, &words), at(64, 80));
    assert_eq!(slice((13, 6), S::ModuleInfo, &words), at(704, 1));
    assert_eq!(slice((13, 6), S::ModuleVariables, &words), at(712, 3));
    assert_eq!(slice((13, 6), S::DependentCode, &words), at(784, 0));
    assert_eq!(slice((13, 6), S::UnusedParameterBits, &words), Ok(None));

    // A 12.4 class scope with exactly the inline limit of locals: an
    // empty one has no positions.
    let mut class = Scope::new(Type::Class, 75);
    let words = class.memory((12, 4));
    assert_eq!(slice((12, 4), S::ContextLocalNames, &words), at(32, 0));
    assert_eq!(
        slice((12, 4), S::ContextLocalNamesHashtable, &words),
        at(32, 1)
    );
    assert_eq!(slice((12, 4), S::ContextLocalInfos, &words), at(40, 75));
    assert_eq!(slice((12, 4), S::PositionInfo, &words), at(640, 1));
    class.empty = true;
    let words = class.memory((12, 4));
    assert_eq!(slice((12, 4), S::PositionInfo, &words), at(640, 0));
    assert_eq!(slice((12, 4), S::DependentCode, &words), Ok(None));
}

/// A version with no translation, a word that cannot be read, and a
/// count that is no Smi are errors, never a place.
#[test]
fn unreadable_or_unlike_scope_infos_are_errors() {
    let scope = Scope::new(Type::Function, 3);
    let words = scope.memory((14, 6));
    let error = scope_info_slice(v8((11, 3)), ScopeInfoSlice::ContextLocalNames, &mut |_| {
        Some(0)
    })
    .expect_err("no translation");
    assert!(error.contains("11.3 has no ScopeInfo slices"), "{error}");

    // Every version reads the flags first, then the count at +24.
    for version in VERSIONS {
        let words = scope.memory(version);
        let error = scope_info_slice(
            v8(version),
            ScopeInfoSlice::InferredFunctionName,
            &mut |at| (at != 24).then(|| words[usize::try_from(at / 8).expect("small")]),
        )
        .expect_err("an unreadable word");
        assert!(error.contains("+24 cannot be read"), "{error}");
    }

    let mut corrupt = words;
    corrupt[3] |= 1;
    let error = slice((14, 6), ScopeInfoSlice::ContextLocalInfos, &corrupt)
        .expect_err("a count that is no Smi");
    assert!(error.contains("is not a Smi"), "{error}");
}
