//! An error's stack formatted from the raw call sites V8 captured, as Node
//! formats them when `stack` is first read (Q8).

use super::inspect::Value;
use super::objects::{Object, Problem};
use super::values::Builder;

/// The text Node would give `error.stack`, from its call sites `sites`.
pub(super) fn format(
    builder: &mut Builder<'_>,
    error: Object,
    sites: Object,
) -> Result<Value, Problem> {
    let _ = (builder, error, sites);
    Err(
        "the error's stack was never formatted, and microscope does not yet format its call sites"
            .into(),
    )
}
