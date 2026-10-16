//! CPython, as the module that carries it describes it: an image is a
//! Python runtime when it has a `.PyRuntime` section, as `libpython3.x.so`
//! has, and an executable that links CPython statically or embeds it.
//!
//! The provider gives the evaluation loop and the call machinery around
//! it their code role.

use object::{Object, ObjectSection};

use crate::{CodeRole, FunctionInfo, SymbolInfo};

/// The section CPython's runtime state is in, which it names so that
/// debuggers can find it.
pub(super) const RUNTIME_SECTION: &str = ".PyRuntime";

/// Whether the module carries a Python runtime.
pub(super) fn carries_runtime(object: &object::File<'_>) -> bool {
    object
        .section_by_name(RUNTIME_SECTION)
        .is_some_and(|section| section.size() > 0)
}

/// Gives the interpreter's functions and symbols their role: the module
/// carries a Python runtime, so CPython's names are CPython's code.
pub(super) fn assign_roles(functions: &mut [FunctionInfo], symbols: &mut [SymbolInfo]) {
    for function in functions {
        if function.role == CodeRole::Ordinary
            && let Some(role) = super::roles::cpython_role(&function.name)
        {
            function.role = role;
        }
    }

    for symbol in symbols {
        if symbol.role == CodeRole::Ordinary
            && let Some(role) = super::roles::cpython_role(&symbol.name)
        {
            symbol.role = role;
        }
    }
}
