//! What the text before a debug console's cursor asks to complete.
//!
//! Completion reads the text as far as it needs and never more: the part
//! being completed is the name the cursor ends, and what comes before it
//! decides what kind of name that is. A member's base is the postfix
//! expression before its `.` or `->`, which the session evaluates to list
//! its members. This reads text alone, so a fuzz target includes it.

/// What the text before the cursor asks to complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Completing<'a> {
    /// A member of the value `base` names, after `.` or `->`.
    Member { base: &'a str },

    /// A register, after `$`.
    Register,

    /// A name inside `qualifier`, after `qualifier::`.
    Qualified { qualifier: &'a str },

    /// A name, or, as the line's first word, a command.
    Name { first: bool },

    /// Nothing: the text ends in an operator no name follows yet.
    Nothing,
}

/// What the text asks to complete, the part being completed, and the byte
/// offset where that part starts.
pub fn completing(typed: &str) -> (Completing<'_>, &str, usize) {
    let start = typed
        .char_indices()
        .rev()
        .take_while(|(_, character)| is_name(*character))
        .last()
        .map_or(typed.len(), |(index, _)| index);
    let partial = &typed[start..];
    let before = &typed[..start];

    let kind = if before.ends_with('$') {
        Completing::Register
    } else if let Some(operator) = before
        .strip_suffix("->")
        .or_else(|| before.strip_suffix('.').filter(|rest| !rest.ends_with('.')))
    {
        base_start(operator).map_or(Completing::Nothing, |base| Completing::Member {
            base: &operator[base..],
        })
    } else if let Some(qualifier) = before.strip_suffix("::") {
        match base_start(qualifier) {
            Some(base) if !qualifier[base..].contains(['.', '[', '(', '$']) => {
                Completing::Qualified {
                    qualifier: &qualifier[base..],
                }
            }
            _ if qualifier.trim().is_empty() => Completing::Qualified { qualifier: "" },
            _ => Completing::Nothing,
        }
    } else if partial.is_empty() && before.trim_end().ends_with(['>', '<', '=', '!', '&', '|']) {
        Completing::Nothing
    } else {
        Completing::Name {
            first: before.trim().is_empty(),
        }
    };

    (kind, partial, start)
}

const fn is_name(character: char) -> bool {
    character.is_ascii_alphanumeric() || character == '_'
}

/// What joins the names of a postfix expression.
const JOINERS: [&[u8]; 3] = [b"->", b"::", b"."];

/// Where the postfix expression that ends the text starts: names joined by
/// `.`, `->`, and `::`, indexed by `[…]`, or a parenthesized expression,
/// such as `p->items[i].next` or `(*node)`. `None` when the text ends in
/// none.
fn base_start(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut at = bytes.len();
    loop {
        // One operand ends at `at`.
        let end = at;
        match bytes.get(at.checked_sub(1)?)? {
            b']' => {
                // An index needs the operand it indexes.
                at = opening(bytes, at - 1, b'[', b']')?;
                continue;
            }
            b')' => {
                at = opening(bytes, at - 1, b'(', b')')?;

                // A length or size reads as one operand with its name.
                while at > 0 && is_name(char::from(bytes[at - 1])) {
                    at -= 1;
                }
            }
            b'`' => {
                at = text[..at - 1].rfind('`')?;
            }
            byte if is_name(char::from(*byte)) => {
                while at > 0 && is_name(char::from(bytes[at - 1])) {
                    at -= 1;
                }

                if at > 0 && bytes[at - 1] == b'$' {
                    at -= 1;
                }
            }
            _ => {}
        }

        if at == end {
            return None;
        }

        match JOINERS.iter().find(|joiner| bytes[..at].ends_with(joiner)) {
            // `::name` names the outermost scope.
            Some(&b"::") if at == 2 || !is_name(char::from(bytes[at - 3])) => {
                return Some(at - 2);
            }
            Some(joiner) => at -= joiner.len(),
            None => return Some(at),
        }
    }
}

/// The index of the bracket that opens the group closing at `close`.
fn opening(bytes: &[u8], close: usize, open: u8, shut: u8) -> Option<usize> {
    let mut depth = 0_usize;
    for index in (0..=close).rev() {
        if bytes[index] == shut {
            depth += 1;
        } else if bytes[index] == open {
            depth -= 1;
            if depth == 0 {
                return Some(index);
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_text_before_the_cursor_decides_what_is_completed() {
        let member = |base| Completing::Member { base };
        for (typed, kind, partial) in [
            ("wat", Completing::Name { first: true }, "wat"),
            ("  wat", Completing::Name { first: true }, "wat"),
            ("print poin", Completing::Name { first: false }, "poin"),
            ("x + cou", Completing::Name { first: false }, "cou"),
            ("", Completing::Name { first: true }, ""),
            ("origin.", member("origin"), ""),
            ("origin.y", member("origin"), "y"),
            ("where->", member("where"), ""),
            ("1 + where->ne", member("where"), "ne"),
            ("p->items[i + 1].next->", member("p->items[i + 1].next"), ""),
            ("m[1][2].", member("m[1][2]"), ""),
            ("(*where).", member("(*where)"), ""),
            ("*where.", member("where"), ""),
            ("(T)x.", member("x"), ""),
            ("len(items).", member("len(items)"), ""),
            ("`odd name`.", member("`odd name`"), ""),
            ("$rax.", member("$rax"), ""),
            ("ns::value.", member("ns::value"), ""),
            ("::value.", member("::value"), ""),
            ("print ::value.f", member("::value"), "f"),
            ("$r", Completing::Register, "r"),
            ("x + $", Completing::Register, ""),
            ("ns::cou", Completing::Qualified { qualifier: "ns" }, "cou"),
            ("a::b::", Completing::Qualified { qualifier: "a::b" }, ""),
            ("::cou", Completing::Qualified { qualifier: "" }, "cou"),
            ("x >", Completing::Nothing, ""),
            ("x == ", Completing::Nothing, ""),
            ("1 + .", Completing::Nothing, ""),
            ("a[1.", member("1"), ""),
            // A range's dots access no member.
            ("a[1..en", Completing::Name { first: false }, "en"),
            ("a)].", Completing::Nothing, ""),
        ] {
            let (found, completed, start) = completing(typed);
            assert_eq!((found, completed), (kind, partial), "{typed:?}");
            assert_eq!(&typed[start..], partial, "{typed:?}");
        }
    }
}
