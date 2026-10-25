//! What microscope reads from Node's inspector that needs no controller:
//! CDP values, the URL patterns breakpoints are set by, and the sockets a
//! process listens on.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;

use nix::unistd::Pid;

use crate::runtime_model::{ReportedLocal, ReportedValue};

/// Whether `frame` runs a script, as the frames of a V8 pause do: V8's
/// builtins, such as `Promise.reject`, record a JavaScript function but no
/// script, and V8 lists no frame for them.
pub(in crate::backend::linux) fn is_script_frame(frame: &crate::StackFrame) -> bool {
    frame.recorded.as_ref().is_some_and(|recorded| {
        recorded.place.is_some()
            && matches!(
                recorded.language,
                crate::SourceLanguage::JavaScript | crate::SourceLanguage::TypeScript
            )
    })
}

/// The variables a `Runtime.getProperties` reply lists for a scope, in
/// its order.
pub(super) fn scope_locals(
    properties: &serde_json::Value,
) -> std::result::Result<Vec<ReportedLocal>, Arc<str>> {
    let listed = properties["result"]
        .as_array()
        .ok_or_else(|| Arc::<str>::from("V8 listed no properties for the scope"))?;
    listed
        .iter()
        .map(|property| {
            let name = property["name"]
                .as_str()
                .ok_or_else(|| Arc::<str>::from("V8 listed a variable with no name"))?;
            // V8 makes a value it cannot give an accessor, which it lists
            // with no value.
            let value = if property.get("value").is_none() {
                ReportedValue::Unavailable
            } else {
                reported_value(&property["value"])
            };
            Ok(ReportedLocal {
                name: name.into(),
                value,
            })
        })
        .collect()
}

/// What a CDP `RemoteObject` says a value is.
fn reported_value(object: &serde_json::Value) -> ReportedValue {
    let text = |key: &str| object[key].as_str().map(Arc::<str>::from);
    match object["type"].as_str() {
        Some("undefined") => ReportedValue::Undefined,
        Some("boolean") => object["value"]
            .as_bool()
            .map_or(ReportedValue::Unknown, ReportedValue::Boolean),
        Some("number") => match (
            object["value"].as_f64(),
            object["unserializableValue"].as_str(),
        ) {
            (Some(value), _) => ReportedValue::Number(value),
            (None, Some("NaN")) => ReportedValue::Number(f64::NAN),
            (None, Some("Infinity")) => ReportedValue::Number(f64::INFINITY),
            (None, Some("-Infinity")) => ReportedValue::Number(f64::NEG_INFINITY),
            (None, Some("-0")) => ReportedValue::Number(-0.0),
            _ => ReportedValue::Unknown,
        },
        Some("string") => text("value").map_or(ReportedValue::Unknown, ReportedValue::String),
        Some("bigint") => object["unserializableValue"]
            .as_str()
            .and_then(|digits| digits.strip_suffix('n'))
            .map_or(ReportedValue::Unknown, |digits| {
                ReportedValue::BigInt(digits.into())
            }),
        Some("symbol") => ReportedValue::Symbol,
        Some("object") if object["subtype"] == "null" => ReportedValue::Null,
        Some("object" | "function") => {
            text("className").map_or(ReportedValue::Unknown, ReportedValue::Object)
        }
        _ => ReportedValue::Unknown,
    }
}

/// A JavaScript regular expression matching the URLs of the scripts at
/// `path`: V8 names a `CommonJS` script by its path and an ES module by its
/// `file:` URL, percent-encoded. A relative path matches the scripts whose
/// paths end with its whole components.
pub(super) fn url_pattern(path: &str) -> String {
    let mut pattern = String::from(if path.starts_with('/') {
        "^(?:file://)?"
    } else {
        "^(?:file://)?(?:.*/)?"
    });
    for character in path.chars() {
        if character.is_ascii_alphanumeric() || "-_~/".contains(character) {
            pattern.push(character);
            continue;
        }
        let literal = if "\\^$.|?*+()[]{}".contains(character) {
            format!("\\{character}")
        } else {
            character.to_string()
        };
        if character == '.' {
            pattern.push_str(&literal);
            continue;
        }
        let mut bytes = [0; 4];
        let encoded = character
            .encode_utf8(&mut bytes)
            .bytes()
            .map(|byte| {
                let hex = format!("{byte:02X}");
                hex.chars()
                    .map(|digit| {
                        if digit.is_ascii_digit() {
                            digit.to_string()
                        } else {
                            format!("[{digit}{}]", digit.to_ascii_lowercase())
                        }
                    })
                    .fold(String::from("%"), |encoded, digit| encoded + &digit)
            })
            .collect::<String>();
        let _ = write!(pattern, "(?:{literal}|{encoded})");
    }
    pattern.push('$');
    pattern
}

/// The loopback TCP sockets process `pid` listens on: its sockets, by the
/// inodes its descriptors name, that `/proc/PID/net/tcp` lists listening on
/// 127.0.0.1.
pub(in crate::backend::linux) fn loopback_listeners(
    pid: Pid,
) -> std::result::Result<Vec<SocketAddr>, Arc<str>> {
    let problem = |error: std::io::Error| -> Arc<str> {
        format!("cannot read the process's sockets: {error}").into()
    };
    let mut inodes = BTreeSet::new();
    for entry in std::fs::read_dir(format!("/proc/{pid}/fd")).map_err(problem)? {
        let Ok(target) = std::fs::read_link(entry.map_err(problem)?.path()) else {
            continue;
        };
        if let Some(inode) = target
            .to_str()
            .and_then(|target| target.strip_prefix("socket:["))
            .and_then(|target| target.strip_suffix(']'))
            .and_then(|inode| inode.parse::<u64>().ok())
        {
            inodes.insert(inode);
        }
    }

    let table = std::fs::read_to_string(format!("/proc/{pid}/net/tcp")).map_err(problem)?;
    Ok(listeners(&table, &inodes))
}

/// The sockets of `table`, as `/proc/net/tcp` writes it, listening on
/// 127.0.0.1 whose inodes are in `inodes`.
fn listeners(table: &str, inodes: &BTreeSet<u64>) -> Vec<SocketAddr> {
    const LISTEN: &str = "0A";

    table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            let (local, state, inode) = (fields.get(1)?, fields.get(3)?, fields.get(9)?);
            if *state != LISTEN || !inodes.contains(&inode.parse().ok()?) {
                return None;
            }
            let (address, port) = local.split_once(':')?;
            // The kernel writes the address as a native-endian word.
            let address = Ipv4Addr::from(u32::from_str_radix(address, 16).ok()?.swap_bytes());
            let port = u16::from_str_radix(port, 16).ok()?;
            address
                .is_loopback()
                .then(|| SocketAddr::V4(SocketAddrV4::new(address, port)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listeners_are_the_processs_loopback_sockets_that_listen() {
        let table = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:A1B2 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 41 1 0 100 0 0 10 0
   1: 00000000:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 42 1 0 100 0 0 10 0
   2: 0100007F:1F91 0100007F:D431 01 00000000:00000000 00:00000000 00000000  1000        0 43 1 0 100 0 0 10 0
   3: 0100007F:1F92 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 44 1 0 100 0 0 10 0
";
        assert_eq!(
            listeners(table, &BTreeSet::from([41, 42, 43])),
            vec!["127.0.0.1:41394".parse::<SocketAddr>().expect("an address")],
            "a socket of another process, one on every address, and a connection are not listeners"
        );
    }
}
