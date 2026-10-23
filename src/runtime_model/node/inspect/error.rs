//! `formatError` and the stack handling it calls.

use std::collections::BTreeMap;

use super::format::{Change, Ctx, Fault, Listed, to_js_string, truthy, unreadable, unsupported};
use super::{ErrorDetails, Key, Kind, Object, Slot, Value};

impl<'g> Ctx<'g> {
    /// `formatError`: the stack as inspect shows it, and the keys minus
    /// those the stack already shows, plus the hidden ones it prints.
    pub fn format_error(
        &mut self,
        object: &'g Object,
        details: &'g ErrorDetails,
        constructor: Option<&str>,
        tag: &str,
        keys: &mut Vec<Listed<'g>>,
    ) -> String {
        let stack = match self.stack_string(details) {
            Ok(stack) => stack,
            Err(Fault::Throws) => {
                let tag = if tag.is_empty() { "Error" } else { tag };
                return format!("[object {tag}]");
            }
            Err(Fault::Visible(text)) => return text,
        };

        if !keys.is_empty() {
            remove_key(keys, "stack");
            for (key, value) in [("message", &details.message), ("name", &details.name)] {
                // Hidden when it is not a string, or when the stack shows it.
                let shown = match value {
                    Value::String(text) => match text.whole() {
                        Some(text) => stack.contains(&text),
                        None => return unreadable("a truncated error message"),
                    },
                    _ => true,
                };
                if shown {
                    remove_key(keys, key);
                }
            }
        }

        let name = match &details.name {
            Value::Undefined | Value::Null => "Error".to_owned(),
            Value::String(name) => match name.whole() {
                Some(name) => name,
                None => return unreadable("a truncated error name"),
            },
            Value::Unreadable(reason) => return unreadable(reason),
            _ => return unsupported("an error name that is not a string"),
        };

        let mut hidden = vec!["cause", "errors"];
        if self.has(Change::SuppressedKeys) {
            hidden.extend(["error", "suppressed"]);
        }
        for wanted in hidden {
            let Some(property) = details.hidden.iter().find(|p| key_is(&p.key, wanted)) else {
                continue;
            };
            if keys.iter().any(|(listed, _)| key_is(&listed.key, wanted)) {
                continue;
            }
            if wanted == "errors" {
                match &property.slot {
                    Slot::Data(value) if self.is_array(value) => {}
                    Slot::Accessor { getter: true, .. } => {
                        // Node calls the getter to check for an array.
                        self.approximate = true;
                        continue;
                    }
                    _ => continue,
                }
            }
            keys.push((property, false));
        }

        let stack = self.improve_stack(stack, constructor, &name, tag);

        let position = match &details.message {
            message if !truthy(message) => None,
            Value::String(message) => {
                let Some(message) = message.whole() else {
                    return unreadable("a truncated error message");
                };
                match stack.find(&message) {
                    Some(at) if at > 0 => Some(at + message.len()),
                    _ => None,
                }
            }
            message => match to_js_string(message) {
                // `message.length` is undefined, so the search starts at 0.
                Ok(message) => stack.find(&message).filter(|at| *at > 0).map(|_| 0),
                Err(Fault::Throws) => return unsupported("a message Node's inspect throws on"),
                Err(Fault::Visible(text)) => return text,
            },
        };
        let from = position.unwrap_or(0);
        let start = stack
            .get(from..)
            .and_then(|rest| rest.find("\n    at"))
            .map(|at| at + from);

        let mut stack = match start {
            None => format!("[{stack}]"),
            Some(start) => {
                let lines = match self.stack_frames(object, details, &stack[start + 1..]) {
                    Ok(lines) => lines,
                    Err(text) => return text,
                };
                format!("{}\n{}", &stack[..start], lines.join("\n"))
            }
        };
        if self.indentation != 0 {
            stack = stack.replace('\n', &format!("\n{}", " ".repeat(self.indentation)));
        }
        stack
    }

    /// `getStackString`.
    fn stack_string(&mut self, details: &ErrorDetails) -> Result<String, Fault> {
        match &details.stack {
            Value::String(stack) if stack.length() != 0 => stack
                .whole()
                .ok_or_else(|| Fault::Visible(unreadable("a truncated error stack"))),
            stack if !truthy(stack) => error_to_string(details),
            Value::Unreadable(reason) => Err(Fault::Visible(unreadable(reason))),
            Value::Object(_) => Err(Fault::Visible(unsupported(
                "an error stack that is an object",
            ))),
            stack => {
                let head = error_to_string(details)?;
                self.indentation += 4;
                let text = self.format_value(stack, 0);
                self.indentation -= 4;
                Ok(format!("{head}\n    {text}"))
            }
        }
    }

    /// `getStackFrames`: the frame lines, minus those shared with the cause
    /// and repeats.
    fn stack_frames(
        &mut self,
        object: &Object,
        details: &ErrorDetails,
        stack: &str,
    ) -> Result<Vec<String>, String> {
        let mut frames: Vec<String> = stack.split('\n').map(str::to_owned).collect();

        let cause = object
            .properties
            .iter()
            .chain(&details.hidden)
            .find(|property| key_is(&property.key, "cause"));
        let cause = match cause.map(|property| &property.slot) {
            Some(Slot::Data(value)) => Some(value),
            Some(Slot::Accessor { getter, .. }) => {
                // Node calls the getter.
                self.approximate |= *getter;
                None
            }
            None => None,
        };
        let graph = self.graph;
        if let Some(Value::Object(id)) = cause
            && let Some(cause) = graph.object(*id)
            && let Kind::Error(cause) = &cause.kind
        {
            let cause_stack = match self.stack_string(cause) {
                Ok(stack) => stack,
                Err(Fault::Throws) => {
                    return Err(unsupported("a cause whose stack Node's inspect throws on"));
                }
                Err(Fault::Visible(text)) => return Err(text),
            };
            if let Some(start) = cause_stack.find("\n    at") {
                let cause_frames: Vec<&str> = cause_stack[start + 1..].split('\n').collect();
                let (length, offset) = identical_sequence_range(&frames, &cause_frames);
                if length > 0 {
                    let skipped = length - 2;
                    let message = format!("    ... {skipped} lines matching cause stack trace ...");
                    frames.splice(offset + 1..offset + 1 + skipped, [message]);
                }
            }
        }

        if frames.len() > 10 {
            let ranges = duplicate_frame_ranges(&frames);
            for range in ranges.iter().rev() {
                let lines = range.length * range.repeats;
                let message = format!(
                    "    ... collapsed {lines} duplicate lines matching above {}",
                    if range.repeats > 1 {
                        format!("{} lines {} times...", range.length, range.repeats)
                    } else {
                        "lines ...".to_owned()
                    }
                );
                frames.splice(range.offset..range.offset + lines, [message]);
            }
        }
        Ok(frames)
    }

    /// `improveStack`: renames the stack's first line to inspect's name for
    /// the error when they differ.
    fn improve_stack(
        &self,
        mut stack: String,
        constructor: Option<&str>,
        name: &str,
        tag: &str,
    ) -> String {
        let mut length = name.len();
        let names_it = name.ends_with("Error")
            && stack.starts_with(name)
            && matches!(stack.as_bytes().get(length), None | Some(b':' | b'\n'));
        if constructor.is_none() || names_it {
            let mut fallback = "Error".to_owned();
            if constructor.is_none() {
                let start = leading_name(&stack).or_else(|| whole_error_name(&stack));
                start.unwrap_or_default().clone_into(&mut fallback);
                length = fallback.len();
                if fallback.is_empty() {
                    "Error".clone_into(&mut fallback);
                }
            }
            let mut prefix = self.prefix(constructor, tag, &fallback, "");
            prefix.pop();
            if name != prefix {
                let rest = stack.get(length..).unwrap_or_default();
                stack = if prefix.contains(name) {
                    if length == 0 {
                        format!("{prefix}: {stack}")
                    } else {
                        format!("{prefix}{rest}")
                    }
                } else {
                    format!("{prefix} [{name}]{rest}")
                };
            }
        }
        stack
    }

    /// Whether `Array.isArray` holds for a value, through proxies.
    fn is_array(&self, value: &Value) -> bool {
        let mut value = value;
        loop {
            let Value::Object(id) = value else {
                return false;
            };
            match self.graph.object(*id).map(|object| &object.kind) {
                Some(Kind::Array(_)) => return true,
                Some(Kind::Proxy { target }) => value = target,
                _ => return false,
            }
        }
    }
}

/// `ErrorPrototypeToString`.
fn error_to_string(details: &ErrorDetails) -> Result<String, Fault> {
    let name = match &details.name {
        Value::Undefined => "Error".to_owned(),
        name => to_js_string(name)?,
    };
    let message = match &details.message {
        Value::Undefined => String::new(),
        message => to_js_string(message)?,
    };
    Ok(if name.is_empty() {
        message
    } else if message.is_empty() {
        name
    } else {
        format!("{name}: {message}")
    })
}

/// Whether a key is the string `name`.
fn key_is(key: &Key, name: &str) -> bool {
    matches!(key, Key::String(key) if key.units().iter().copied().eq(name.encode_utf16()))
}

/// Removes the first key named `name`.
fn remove_key(keys: &mut Vec<Listed<'_>>, name: &str) {
    if let Some(at) = keys
        .iter()
        .position(|(property, _)| key_is(&property.key, name))
    {
        keys.remove(at);
    }
}

/// `/^([A-Z][a-z_ A-Z0-9[\]()-]+)(?::|\n {4}at)/`'s group.
fn leading_name(stack: &str) -> Option<&str> {
    let bytes = stack.as_bytes();
    if !bytes.first()?.is_ascii_uppercase() {
        return None;
    }
    let run = bytes[1..]
        .iter()
        .take_while(|b| b.is_ascii_alphanumeric() || b"_ []()-".contains(*b))
        .count();
    if run == 0 {
        return None;
    }
    let end = 1 + run;
    let rest = &stack[end..];
    (rest.starts_with(':') || rest.starts_with("\n    at")).then(|| &stack[..end])
}

/// `/^([a-z_A-Z0-9-]*Error)$/`'s group.
fn whole_error_name(stack: &str) -> Option<&str> {
    (stack.ends_with("Error")
        && stack
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'))
    .then_some(stack)
}

/// `identicalSequenceRange`: the length and start in `a` of the first run
/// of more than three lines of `a` that also appears in `b`.
#[allow(
    clippy::suspicious_operation_groupings,
    reason = "the run in `b` starts at `position`, the run in `a` at `i`"
)]
fn identical_sequence_range(a: &[String], b: &[&str]) -> (usize, usize) {
    for i in 0..a.len().saturating_sub(3) {
        let Some(position) = b.iter().position(|line| *line == a[i]) else {
            continue;
        };
        let rest = b.len() - position;
        if rest > 3 {
            let mut length = 1;
            let max_length = (a.len() - i).min(rest);
            while max_length > length && a[i + length] == b[position + length] {
                length += 1;
            }
            if length > 3 {
                return (length, i);
            }
        }
    }
    (0, 0)
}

/// A block of frames that repeats right after itself.
struct Repeat {
    /// Where the first repetition starts.
    offset: usize,
    length: usize,
    repeats: usize,
}

/// `getDuplicateErrorFrameRanges`.
fn duplicate_frame_ranges(frames: &[String]) -> Vec<Repeat> {
    const MINIMUM: usize = 3;
    let mut result = Vec::new();
    let mut positions: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (i, frame) in frames.iter().enumerate() {
        positions.entry(frame).or_default().push(i);
    }
    if frames.len() - positions.len() <= MINIMUM {
        return result;
    }

    let mut i = 0;
    while i < frames.len().saturating_sub(MINIMUM) {
        let Some(at) = positions.get(frames[i].as_str()) else {
            i += 1;
            continue;
        };
        let last = at.last().copied().unwrap_or(i);
        if at.len() == 1 || last == i {
            i += 1;
            continue;
        }
        let current = at.iter().position(|p| *p == i).map_or(0, |p| p + 1);
        if current == at.len() {
            i += 1;
            continue;
        }
        let mut range = last - i;
        if range < MINIMUM {
            i += 1;
            continue;
        }
        let mut extra_steps = if current + 1 < at.len() {
            // The GCD of the distances to the same line, keeping the divisors
            // found along the way as fallbacks.
            let mut steps: Vec<usize> = Vec::new();
            let mut gcd = 0;
            for p in &at[current..] {
                let mut distance = p - i;
                while distance != 0 {
                    let remainder = gcd % distance;
                    if gcd != 0 && !steps.contains(&gcd) {
                        steps.push(gcd);
                    }
                    gcd = distance;
                    distance = remainder;
                }
                if gcd == 1 {
                    break;
                }
            }
            range = gcd;
            steps.retain(|step| *step != range);
            Some(steps)
        } else {
            None
        };

        let mut max_range = range;
        let mut max_duplicates = 0;
        let mut duplicates = 0;
        let mut next_start = i + range;
        loop {
            let equal = (0..range)
                .take_while(|j| frames.get(next_start + j) == Some(&frames[i + j]))
                .count();
            if equal != range {
                let Some(step) = extra_steps.as_mut().and_then(Vec::pop) else {
                    break;
                };
                if duplicates != 0 && max_range * max_duplicates < range * duplicates {
                    max_range = range;
                    max_duplicates = duplicates;
                }
                range = step;
                next_start = i + range;
                duplicates = 0;
                continue;
            }
            duplicates += 1;
            next_start += range;
        }

        if max_duplicates != 0 && max_range * max_duplicates >= range * duplicates {
            range = max_range;
            duplicates = max_duplicates;
        }
        if duplicates * range >= 3 {
            result.push(Repeat {
                offset: i + range,
                length: range,
                repeats: duplicates,
            });
            i += range * (duplicates + 1) - 1;
        }
        i += 1;
    }
    result
}
