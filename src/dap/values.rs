//! Rows as the client's variables. What a row holds is shared with every
//! front end ([`crate::present`]); how it is named and expanded later is the
//! adapter's.

use microscope::{RegisterValue, StopContext};
use serde_json::{Map, Value, json};

use super::handles::{Exhausted, Location, References, Variables};
use crate::cli::format::register_bytes;
use crate::present::{Expand, Row};

/// What the client accepts in a variable, and how it wants values shown.
#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    /// Whether to include each value's type.
    pub types: bool,

    /// Whether to include memory references.
    pub memory: bool,

    /// Whether to show integers in hexadecimal.
    pub hex: bool,
}

/// How values show when a request does not say.
#[derive(Debug, Clone, Copy, Default)]
pub struct Display {
    /// Whether integers show in hexadecimal.
    pub hex: bool,
}

/// Presents a row as a `Variable`, with a reference that expands it.
pub fn variable(
    row: Row,
    context: StopContext,
    options: Options,
    references: &mut References,
) -> Result<Map<String, Value>, Exhausted> {
    let mut variable = Map::new();
    variable.insert("name".to_owned(), row.name.as_str().into());
    variable.insert("value".to_owned(), row.text.as_str().into());

    if options.types {
        variable.insert(
            "type".to_owned(),
            row.type_name.as_deref().unwrap_or("<unknown type>").into(),
        );
    }

    if let Some((module, location)) = row.declaration.clone() {
        variable.insert(
            "declarationLocationReference".to_owned(),
            references
                .location(Location::Declared { module, location })?
                .into(),
        );
    }

    if let Some(path) = row.evaluate_name() {
        variable.insert("evaluateName".to_owned(), path.to_string().into());
    }

    let mut reference = 0;
    if let Some(details) = row.details {
        match details.expand {
            Some(Expand::Children {
                reference: children,
                counts,
                indexed,
            }) => {
                if let Some(count) = counts.indexed {
                    variable.insert("indexedVariables".to_owned(), count.into());
                }

                if let Some(count) = counts.named {
                    variable.insert("namedVariables".to_owned(), count.into());
                }

                reference = references.variables(Variables::Children {
                    context,
                    reference: children,
                    path: row.path,
                    indexed,
                })?;
            }
            Some(Expand::Pointee(dereference)) => {
                reference = references.variables(Variables::Pointee {
                    context,
                    reference: dereference,
                    name: row.name.as_str().into(),
                    path: row.path,
                })?;
            }
            None => {}
        }

        // A pointer to a function leads to the function's code.
        if let Some(address) = details.code {
            variable.insert(
                "valueLocationReference".to_owned(),
                references.location(Location::Code(address))?.into(),
            );
        }

        if options.memory
            && let Some(address) = details.memory
        {
            variable.insert("memoryReference".to_owned(), format!("{address:#x}").into());
        }

        let mut attributes = Vec::new();
        if !details.editable {
            attributes.push("readOnly");
        }

        if details.constant {
            attributes.push("constant");
        }

        if details.text_view {
            attributes.push("rawString");
        }

        let kind = if details.aggregate { "class" } else { "data" };
        variable.insert(
            "presentationHint".to_owned(),
            json!({"kind": kind, "attributes": attributes}),
        );
    }

    variable.insert("variablesReference".to_owned(), reference.into());
    Ok(variable)
}

/// Presents a register, which never expands and which expressions read but
/// cannot assign.
/// A register's row: one the innermost frame holds can be set, as the
/// thread's own; a caller's is what unwinding recovered.
pub fn register(
    value: &RegisterValue,
    byte_order: microscope::ByteOrder,
    innermost: bool,
) -> Map<String, Value> {
    let mut variable = Map::new();
    let name = value.register.name.as_ref();
    variable.insert("name".to_owned(), name.into());
    if name
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || character == '_')
    {
        variable.insert("evaluateName".to_owned(), format!("${name}").into());
    }

    let attributes = if innermost && value.bytes.is_some() {
        json!([])
    } else {
        json!(["readOnly"])
    };

    variable.insert(
        "presentationHint".to_owned(),
        json!({"kind": "data", "attributes": attributes}),
    );
    variable.insert(
        "value".to_owned(),
        value
            .bytes
            .as_ref()
            .map_or_else(
                || "<not saved>".to_owned(),
                |bytes| register_bytes(bytes, byte_order),
            )
            .into(),
    );
    variable.insert("variablesReference".to_owned(), 0.into());
    variable
}

/// A row that marks where an inspection stopped short, such as at its
/// memory limit.
pub fn truncation(description: String) -> Map<String, Value> {
    let mut variable = Map::new();
    variable.insert("name".to_owned(), "<truncated>".into());
    variable.insert("value".to_owned(), description.into());
    variable.insert("variablesReference".to_owned(), 0.into());
    variable.insert(
        "presentationHint".to_owned(),
        json!({"kind": "virtual", "attributes": ["readOnly"]}),
    );
    variable
}
