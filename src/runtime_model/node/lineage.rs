//! The identities that choose a JavaScript object's view: those of the
//! constructors its prototype chain names, nearest first.
//!
//! A class is its constructor function's name, in a path made from the
//! script that defines the function: `node:internal/url` gives
//! `node::internal::url::URL`, and `/app/models.js` gives
//! `app::models::Point`. A builtin, which has no script, has its name
//! alone. Each prototype names its class by its own `constructor` data
//! property; one without such a function names none.

use std::sync::Arc;

use super::lookup::{self, Key};
use super::objects::{Object, Problem, Reader};
use super::own::Held;
use super::present::Handle;
use crate::image::v8::{Constant, Field};
use crate::runtime_model::{InterpretedHandle, interpreted_identity};
use crate::{SourceLanguage, TypeIdentity};

/// Prototype chains are a few objects long; stop after this many.
const MAX_PROTOTYPES: usize = 64;

/// The identities of the classes the prototype `class` and the prototypes
/// after it name, nearest first.
pub(super) fn lineage(
    reader: &Reader<'_>,
    class: InterpretedHandle,
) -> Result<Vec<TypeIdentity>, Problem> {
    let Handle::Tagged(word) = Handle::of(class) else {
        return Err("a number has no class".into());
    };
    let mut lineage = Vec::new();
    let mut holder = reader.object(word)?;
    for _ in 0..MAX_PROTOTYPES {
        let Some(prototype) = holder else {
            return Ok(lineage);
        };
        if let Some((Held::Data(constructor), _)) =
            lookup::own(reader, prototype, Key::Name("constructor"))?
            && let Some(constructor) = reader.object(constructor)?
            && let Some(identity) = identity(reader, constructor)?
        {
            lineage.push(identity);
        }
        holder = lookup::prototype(reader, prototype)?;
    }
    Err(format!("{word:#x} has too many prototypes").into())
}

/// The identity of the class whose constructor is `function`, or `None`
/// for a constructor that is no function or has no name of its own.
fn identity(reader: &Reader<'_>, function: Object) -> Result<Option<TypeIdentity>, Problem> {
    let functions = reader.constant(Constant::FirstJSFunctionType)?
        ..=reader.constant(Constant::LastJSFunctionType)?;
    if !functions.contains(&reader.instance_type(function)?) {
        return Ok(None);
    }
    let shared = reader.typed_field(
        function,
        Field::JSFunctionSharedFunctionInfo,
        Constant::SharedFunctionInfoType,
    )?;
    // The function's own name, as `function.name` starts out, never one
    // V8 inferred from where the function was assigned.
    let Some(name) = super::names::shared_name(reader, shared)?.0 else {
        return Ok(None);
    };
    let name = reader.string(name)?;
    if name.is_empty() {
        return Ok(None);
    }
    let script = super::frames::script(reader, shared)?
        .map(|script| super::frames::script_name(reader, script))
        .transpose()?
        .flatten();
    let path = script.as_deref().map(module_path).unwrap_or_default();
    Ok(Some(TypeIdentity {
        path: path.into(),
        ..interpreted_identity(SourceLanguage::JavaScript, None, &name)
    }))
}

/// The module path of the script named `name`: `node` and the parts of a
/// `node:` id, or a file's directories and its name without its extension.
fn module_path(name: &str) -> Vec<Arc<str>> {
    if let Some(id) = name.strip_prefix("node:") {
        return std::iter::once("node")
            .chain(id.split('/'))
            .filter(|part| !part.is_empty())
            .map(Arc::from)
            .collect();
    }
    let path = super::frames::file_url_path(name).unwrap_or_else(|| name.to_owned());
    let mut parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    if let Some(file) = parts.pop() {
        let stem = std::path::Path::new(file)
            .file_stem()
            .and_then(std::ffi::OsStr::to_str)
            .unwrap_or(file);
        parts.push(stem);
    }
    parts.into_iter().map(Arc::from).collect()
}

#[cfg(test)]
mod tests {
    use super::module_path;

    fn path(name: &str) -> Vec<String> {
        module_path(name).iter().map(ToString::to_string).collect()
    }

    #[test]
    fn a_script_names_its_module_path() {
        assert_eq!(path("node:internal/url"), ["node", "internal", "url"]);
        assert_eq!(path("node:buffer"), ["node", "buffer"]);
        assert_eq!(path("/app/lib/models.js"), ["app", "lib", "models"]);
        assert_eq!(
            path("/app/lib/models.test.mjs"),
            ["app", "lib", "models.test"]
        );
        assert_eq!(path("file:///app/my%20dir/x.mjs"), ["app", "my dir", "x"]);
        assert_eq!(path("named-by-url.js"), ["named-by-url"]);
    }
}
