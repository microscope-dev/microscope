use microscope::{StopContext, VariableSnapshot};

use super::Presenter;
pub use super::completing::{Completing, completing};

pub async fn candidates(
    presenter: &Presenter<'_>,
    completing: Completing<'_>,
    partial: &str,
    command: &str,
    context: Option<StopContext>,
    variables: Option<&VariableSnapshot>,
) -> Vec<(String, &'static str)> {
    let mut candidates = Vec::new();
    match completing {
        Completing::Name { first: false } if command == "info" => {
            for subcommand in crate::cli::commands::INFO_SUBCOMMANDS {
                candidates.push((subcommand.to_owned(), "value"));
            }
        }
        Completing::Name { first: false } if command == "handle" => {
            for code in microscope::signal_codes() {
                if let Some(name) = microscope::signal_name(code) {
                    candidates.push((name, "value"));
                }
            }
        }
        Completing::Name { first } => {
            if first {
                for spec in crate::cli::commands::COMMANDS {
                    candidates.push((spec.name.to_owned(), "keyword"));
                }
            }

            for variable in variables
                .iter()
                .flat_map(|snapshot| snapshot.variables.iter())
            {
                candidates.push((variable.name.to_string(), "variable"));
            }

            for (_, image) in presenter.code.modules() {
                for global in image.globals() {
                    if global.name == global.qualified_name {
                        candidates.push((global.name.to_string(), "variable"));
                    } else if global.qualified_name.starts_with(partial) {
                        candidates.push((global.qualified_name.to_string(), "variable"));
                    }
                }
            }
        }
        Completing::Qualified { qualifier } => {
            let prefix = if qualifier.is_empty() {
                String::new()
            } else {
                format!("{qualifier}::")
            };

            for (_, image) in presenter.code.modules() {
                for global in image.globals() {
                    if let Some(rest) = global.qualified_name.strip_prefix(prefix.as_str()) {
                        candidates.push((rest.to_owned(), "variable"));
                    }
                }
            }
        }
        Completing::Member { base } => {
            if let Some(context) = context {
                for member in presenter.member_names(context, base).await {
                    candidates.push((member, "field"));
                }
            }
        }
        Completing::Register => {
            if let Some(context) = context
                && let Ok(registers) = presenter.handle.at(context).registers().await
            {
                for register in registers.registers.iter() {
                    candidates.push((register.register.name.to_string(), "variable"));
                }
            }
        }
        Completing::Nothing => {}
    }

    candidates
}

pub fn matching(
    candidates: Vec<(String, &'static str)>,
    partial: &str,
    limit: usize,
) -> Vec<(String, &'static str)> {
    let mut seen = std::collections::BTreeSet::new();
    candidates
        .into_iter()
        .filter(|(label, _)| label.starts_with(partial) && seen.insert(label.clone()))
        .take(limit)
        .collect()
}
