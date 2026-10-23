//! Prints a reading in normal form, which parses back to the same readings.

use std::fmt::Write as _;

use super::ast::{
    CastForm, Field, NodeId, NodeKind, Path, Separator, SizeOf, Suffix, Tree, TypeBase, TypeName,
    UnaryOp,
};

use super::parser::{
    AS, ASSIGN, COMPARISON, CONDITIONAL, POSTFIX, PREFIX, assignment_text, binary_precedence,
    binary_text, is_bare, is_name_word,
};

use crate::eval::number::Float;

/// Printed where a node has no text of its own, which no reading's node
/// should lack.
const UNPRINTABLE: &str = "<?>";

pub fn print(tree: &Tree) -> String {
    let mut printer = Printer {
        tree,
        out: String::new(),
    };

    printer.node(tree.root());
    printer.out
}

/// A type as a cast's parentheses would hold it.
pub fn type_text(ty: &TypeName) -> String {
    let mut out = base_text(&ty.base);
    out.extend(std::iter::repeat_n('*', usize::from(ty.pointers)));
    out
}

/// A type as `as` takes it.
fn as_type_text(ty: &TypeName) -> String {
    let mut out: String = std::iter::repeat_n('*', usize::from(ty.pointers)).collect();
    out.push_str(&base_text(&ty.base));
    out
}

fn base_text(base: &TypeBase) -> String {
    match base {
        TypeBase::Named(path) => path_text(path),
        TypeBase::CWords(words) => words
            .iter()
            .map(|word| word.text())
            .collect::<Vec<_>>()
            .join(" "),
        TypeBase::Tagged(tag, path) => format!("{} {}", tag.text(), path_text(path)),
    }
}

pub fn path_text(path: &Path) -> String {
    let mut out = String::new();
    for (index, segment) in path.segments.iter().enumerate() {
        if index > 0 || path.global {
            out.push_str(match segment.separator {
                Separator::Colons => "::",
                Separator::Dot => ".",
            });
        }

        out.push_str(&name_text(&segment.name));
    }

    out
}

/// A name, in backticks unless it reads back as itself.
pub fn name_text(name: &str) -> String {
    let plain = name
        .bytes()
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        && is_name_word(name);
    if plain {
        name.to_owned()
    } else {
        format!("`{name}`")
    }
}

struct Printer<'tree> {
    tree: &'tree Tree,
    out: String,
}

impl Printer<'_> {
    fn precedence(&self, id: NodeId) -> u8 {
        match self.tree.kind(id) {
            NodeKind::Assign { .. } => ASSIGN,
            NodeKind::Conditional { .. } => CONDITIONAL,
            NodeKind::Binary { op, .. } => binary_precedence(*op),
            NodeKind::Cast {
                form: CastForm::As, ..
            } => AS,
            NodeKind::Unary { .. }
            | NodeKind::Cast {
                form: CastForm::Prefix,
                ..
            }
            | NodeKind::SizeOf(_)
            | NodeKind::OffsetOf { .. } => PREFIX,
            _ => POSTFIX,
        }
    }

    /// Prints a child that must bind at least as tightly as `minimum`.
    fn child(&mut self, id: NodeId, minimum: u8) {
        if self.precedence(id) < minimum {
            self.out.push('(');
            self.node(id);
            self.out.push(')');
        } else {
            self.node(id);
        }
    }

    /// The text a child would print, for decisions about what follows it.
    fn child_text(&self, id: NodeId, minimum: u8) -> String {
        let mut printer = Printer {
            tree: self.tree,
            out: String::new(),
        };

        printer.child(id, minimum);
        printer.out
    }

    #[expect(clippy::too_many_lines, reason = "one arm per kind of node")]
    fn node(&mut self, id: NodeId) {
        match self.tree.kind(id) {
            NodeKind::Unary { op, operand } => self.unary(*op, *operand),
            NodeKind::Binary { op, left, right } => {
                let strength = binary_precedence(*op);
                let left_minimum = if strength == COMPARISON {
                    strength + 1
                } else {
                    strength
                };

                self.child(*left, left_minimum);
                self.out.push(' ');
                self.out.push_str(binary_text(*op));
                self.out.push(' ');
                self.child(*right, strength + 1);
            }

            NodeKind::Conditional {
                condition,
                then,
                otherwise,
            } => {
                self.child(*condition, CONDITIONAL + 1);
                self.out.push_str(" ? ");
                self.child(*then, CONDITIONAL);
                self.out.push_str(" : ");
                self.child(*otherwise, CONDITIONAL);
            }
            NodeKind::Assign { op, target, value } => {
                self.child(*target, CONDITIONAL);
                self.out.push(' ');
                self.out
                    .push_str(assignment_text(*op).unwrap_or(UNPRINTABLE));
                self.out.push(' ');
                self.child(*value, ASSIGN);
            }
            NodeKind::Cast { ty, operand, form } => self.cast(ty, *operand, *form),
            NodeKind::Member {
                base, field, arrow, ..
            } => self.member(*base, field, *arrow),
            NodeKind::Index { base, index } => {
                self.child(*base, POSTFIX);
                self.out.push('[');
                self.child(*index, ASSIGN);
                self.out.push(']');
            }
            NodeKind::Range { base, start, end } => {
                self.child(*base, POSTFIX);
                self.out.push('[');
                self.child(*start, ASSIGN);
                self.out.push_str("..");
                self.child(*end, ASSIGN);
                self.out.push(']');
            }
            NodeKind::Slice { base, start, end } => {
                self.child(*base, POSTFIX);
                self.out.push('[');
                if let Some(start) = start {
                    self.child(*start, ASSIGN);
                }

                self.out.push(':');
                if let Some(end) = end {
                    self.child(*end, ASSIGN);
                }

                self.out.push(']');
            }
            NodeKind::SizeOf(SizeOf::Type(ty)) => {
                self.out.push_str("sizeof(");

                // A bare name alone would be measured as a value if it
                // names one; a qualifier keeps it a type.
                if is_bare(ty) {
                    self.out.push_str("const ");
                }

                self.out.push_str(&type_text(ty));
                self.out.push(')');
            }
            NodeKind::SizeOf(SizeOf::Operand(operand)) => {
                self.out.push_str("sizeof(");
                self.child(*operand, ASSIGN);
                self.out.push(')');
            }
            NodeKind::OffsetOf { ty, member } => {
                self.out.push_str("offsetof(");
                self.out.push_str(&type_text(ty));
                self.out.push_str(", ");
                self.out.push_str(&name_text(member));
                self.out.push(')');
            }

            NodeKind::ContainerOf {
                pointer,
                ty,
                member,
            } => {
                self.out.push_str("container_of(");
                self.child(*pointer, ASSIGN);
                self.out.push_str(", ");
                self.out.push_str(&type_text(ty));
                self.out.push_str(", ");
                self.out.push_str(&name_text(member));
                self.out.push(')');
            }
            NodeKind::Global(name) => {
                self.out.push_str("global(");
                self.out.push_str(&name_text(name));
                self.out.push(')');
            }
            NodeKind::Len(operand) => {
                self.out.push_str("len(");
                self.child(*operand, ASSIGN);
                self.out.push(')');
            }
            NodeKind::Cap(operand) => {
                self.out.push_str("cap(");
                self.child(*operand, ASSIGN);
                self.out.push(')');
            }
            NodeKind::Call { function, operand } => {
                self.out.push_str(function.name());
                self.out.push('(');
                self.child(*operand, ASSIGN);
                self.out.push(')');
            }
            NodeKind::Tuple(items) => {
                self.out.push('(');
                for (position, item) in items.iter().enumerate() {
                    if position > 0 {
                        self.out.push_str(", ");
                    }

                    self.child(*item, ASSIGN);
                }

                // `(a,)` is a tuple where `(a)` only groups.
                if items.len() == 1 {
                    self.out.push(',');
                }

                self.out.push(')');
            }
            leaf => self.leaf(leaf),
        }
    }

    fn leaf(&mut self, kind: &NodeKind) {
        match kind {
            NodeKind::Name(path) => self.out.push_str(&path_text(path)),
            NodeKind::Register(name) => {
                self.out.push('$');
                self.out.push_str(name);
            }
            NodeKind::Integer { value, suffix } => {
                let _ = write!(self.out, "{value}");
                let sign = |signed| if signed { 'i' } else { 'u' };
                match *suffix {
                    Some(Suffix::Int { width, signed }) => {
                        let _ = write!(self.out, "{}{width}", sign(signed));
                    }
                    Some(Suffix::Size { signed }) => {
                        let _ = write!(self.out, "{}size", sign(signed));
                    }
                    Some(Suffix::BigInt) => self.out.push('n'),
                    Some(Suffix::F32 | Suffix::F64) | None => {}
                }
            }
            NodeKind::Float(value) => self.float(*value),
            NodeKind::Char(value) => {
                self.out.push('\'');
                match char::from_u32(*value) {
                    Some('\'') => self.out.push_str("\\'"),
                    Some(character) => escape_char(&mut self.out, character),
                    None => {}
                }

                self.out.push('\'');
            }
            NodeKind::Text(bytes) => {
                self.out.push('"');
                escape_bytes(&mut self.out, bytes);
                self.out.push('"');
            }
            NodeKind::Bool(value) => self.out.push_str(if *value { "true" } else { "false" }),
            NodeKind::Null => self.out.push_str("null"),
            _ => self.out.push_str(UNPRINTABLE),
        }
    }

    fn unary(&mut self, op: UnaryOp, operand: NodeId) {
        self.out.push_str(match op {
            UnaryOp::Neg => "-",
            UnaryOp::Not => "!",
            UnaryOp::BitNot => "~",
            UnaryOp::Deref => "*",
            UnaryOp::AddressOf => "&",
        });
        let operand = self.child_text(operand, PREFIX);

        // `& &x` must not read as `&&`.
        if op == UnaryOp::AddressOf && operand.starts_with('&') {
            self.out.push(' ');
        }

        self.out.push_str(&operand);
    }

    fn cast(&mut self, ty: &TypeName, operand: NodeId, form: CastForm) {
        if form == CastForm::As {
            self.child(operand, AS);
            self.out.push_str(" as ");
            self.out.push_str(&as_type_text(ty));
            return;
        }

        self.out.push('(');
        self.out.push_str(&type_text(ty));
        self.out.push(')');
        let operand = self.child_text(operand, PREFIX);

        // `(T)-x` with a bare name would also read as subtraction.
        if is_bare(ty) && operand.starts_with(['-', '*', '&']) {
            let _ = write!(self.out, "({operand})");
        } else {
            self.out.push_str(&operand);
        }
    }

    fn member(&mut self, base: NodeId, field: &Field, arrow: bool) {
        // `(1).5` selects a field where `1.5` would be a float.
        let force = matches!(self.tree.kind(base), NodeKind::Integer { suffix: None, .. });
        if force {
            self.out.push('(');
            self.node(base);
            self.out.push(')');
        } else {
            self.child(base, POSTFIX);
        }

        self.out.push_str(if arrow { "->" } else { "." });
        match field {
            Field::Named(name) => self.out.push_str(&name_text(name)),
            Field::Index(index) => {
                let _ = write!(self.out, "{index}");
            }
        }
    }

    fn float(&mut self, value: Float) {
        match value.to_value() {
            crate::FloatValue::Binary32(bits) => {
                let _ = write!(self.out, "{:?}f32", f32::from_bits(bits));
            }
            crate::FloatValue::Binary64(bits) => {
                let _ = write!(self.out, "{:?}", f64::from_bits(bits));
            }

            // Literals are never x87 values.
            _ => {
                let _ = write!(self.out, "{value}");
            }
        }
    }
}

fn escape_char(out: &mut String, character: char) {
    match character {
        '\n' => out.push_str("\\n"),
        '\r' => out.push_str("\\r"),
        '\t' => out.push_str("\\t"),
        '\0' => out.push_str("\\0"),
        '\\' => out.push_str("\\\\"),
        character if character.is_control() => {
            let _ = write!(out, "\\u{{{:x}}}", u32::from(character));
        }
        character => out.push(character),
    }
}

fn escape_bytes(out: &mut String, bytes: &[u8]) {
    for chunk in bytes.utf8_chunks() {
        for character in chunk.valid().chars() {
            if character == '"' {
                out.push_str("\\\"");
            } else {
                escape_char(out, character);
            }
        }

        for byte in chunk.invalid() {
            let _ = write!(out, "\\x{byte:02x}");
        }
    }
}
