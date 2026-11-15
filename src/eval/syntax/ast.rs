use super::Span;
use crate::eval::number::Float;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId(pub(super) u32);

#[derive(Debug, Clone)]
pub struct Tree {
    pub(super) nodes: Vec<Node>,
    pub(super) root: NodeId,
}

impl Tree {
    pub const fn root(&self) -> NodeId {
        self.root
    }

    #[expect(
        clippy::indexing_slicing,
        reason = "the parser makes every id from the length of the tree's nodes"
    )]
    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id.0 as usize]
    }

    pub fn kind(&self, id: NodeId) -> &NodeKind {
        &self.node(id).kind
    }

    pub fn span(&self, id: NodeId) -> Span {
        self.node(id).span
    }

    #[cfg(any(test, feature = "fuzzing"))]
    pub fn check_spans(&self, text: &str) -> Result<(), String> {
        for (index, node) in self.nodes.iter().enumerate() {
            let span = node.span;
            let slice = text
                .get(span.start as usize..span.end as usize)
                .filter(|_| span.start <= span.end)
                .ok_or_else(|| format!("node {index}'s span {span:?} is outside `{text}`"))?;
            if !balanced(slice) {
                return Err(format!("`{slice}` is unbalanced, in `{text}`"));
            }

            for child in node.kind.children() {
                let inner = self.span(child);
                if inner.start < span.start || inner.end > span.end {
                    return Err(format!(
                        "`{}` lies outside its parent `{slice}`",
                        inner.text(text)
                    ));
                }
            }
        }

        Ok(())
    }

    pub fn same_shape(&self, other: &Self) -> bool {
        self.same_node(self.root, other, other.root)
    }

    fn same_node(&self, id: NodeId, other: &Self, other_id: NodeId) -> bool {
        let children = |tree: &Self, id| tree.kind(id).children();
        self.kind(id).same_leaf(other.kind(other_id))
            && children(self, id).len() == children(other, other_id).len()
            && children(self, id)
                .iter()
                .zip(children(other, other_id).iter())
                .all(|(&left, &right)| self.same_node(left, other, right))
    }
}

#[derive(Debug, Clone)]
pub struct Node {
    pub kind: NodeKind,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Path {
    pub global: bool,
    pub segments: Vec<Segment>,
}

#[derive(Debug, Clone)]
pub struct Segment {
    pub separator: Separator,
    pub name: String,
    pub span: Span,
}

impl PartialEq for Segment {
    fn eq(&self, other: &Self) -> bool {
        self.separator == other.separator && self.name == other.name
    }
}

impl Eq for Segment {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Separator {
    Colons,
    Dot,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeName {
    pub base: TypeBase,

    pub pointers: u8,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeBase {
    Named(Path),

    CWords(Vec<CWord>),

    Tagged(Tag, Path),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CWord {
    Signed,
    Unsigned,
    Short,
    Long,
    Char,
    Int,
    Float,
    Double,
}

impl CWord {
    pub fn parse(word: &str) -> Option<Self> {
        Some(match word {
            "signed" => Self::Signed,
            "unsigned" => Self::Unsigned,
            "short" => Self::Short,
            "long" => Self::Long,
            "char" => Self::Char,
            "int" => Self::Int,
            "float" => Self::Float,
            "double" => Self::Double,
            _ => return None,
        })
    }

    pub const fn text(self) -> &'static str {
        match self {
            Self::Signed => "signed",
            Self::Unsigned => "unsigned",
            Self::Short => "short",
            Self::Long => "long",
            Self::Char => "char",
            Self::Int => "int",
            Self::Float => "float",
            Self::Double => "double",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tag {
    Struct,
    Union,
    Enum,
    Class,
}

impl Tag {
    pub fn parse(word: &str) -> Option<Self> {
        Some(match word {
            "struct" => Self::Struct,
            "union" => Self::Union,
            "enum" => Self::Enum,
            "class" => Self::Class,
            _ => return None,
        })
    }

    pub const fn text(self) -> &'static str {
        match self {
            Self::Struct => "struct",
            Self::Union => "union",
            Self::Enum => "enum",
            Self::Class => "class",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Suffix {
    Int {
        width: u8,
        signed: bool,
    },

    /// `isize` or `usize`, as wide as the target's addresses.
    Size {
        signed: bool,
    },
    F32,
    F64,

    BigInt,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Field {
    Named(String),

    Index(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Builtin {
    Inner,
}

impl Builtin {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "inner" => Some(Self::Inner),
            _ => None,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Inner => "inner",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SizeOf {
    Type(TypeName),

    Operand(NodeId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    Neg,

    Not,

    BitNot,

    Deref,

    AddressOf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    Mul,
    Div,
    Rem,
    Add,
    Sub,
    Shl,
    Shr,
    BitAnd,
    BitXor,
    BitOr,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CastForm {
    Prefix,

    As,
}

#[derive(Debug, Clone)]
pub enum NodeKind {
    Name(Path),

    Register(String),
    Integer {
        value: u128,
        suffix: Option<Suffix>,
    },
    Float(Float),

    Char(u32),

    Text(Vec<u8>),
    Bool(bool),
    Null,
    Unary {
        op: UnaryOp,
        operand: NodeId,
    },
    Binary {
        op: BinaryOp,
        left: NodeId,
        right: NodeId,
    },
    Conditional {
        condition: NodeId,
        then: NodeId,
        otherwise: NodeId,
    },

    Assign {
        op: Option<BinaryOp>,
        target: NodeId,
        value: NodeId,
    },
    Cast {
        ty: TypeName,
        operand: NodeId,
        form: CastForm,
    },

    Member {
        base: NodeId,
        field: Field,
        field_span: Span,
        arrow: bool,
    },
    Index {
        base: NodeId,
        index: NodeId,
    },

    Range {
        base: NodeId,
        start: NodeId,
        end: NodeId,
    },

    Slice {
        base: NodeId,
        start: Option<NodeId>,
        end: Option<NodeId>,
    },
    SizeOf(SizeOf),

    OffsetOf {
        ty: TypeName,
        member: String,
    },

    ContainerOf {
        pointer: NodeId,
        ty: TypeName,
        member: String,
    },

    Global(String),
    Len(NodeId),
    Cap(NodeId),

    Call {
        function: Builtin,
        operand: NodeId,
    },

    Tuple(Vec<NodeId>),
}

impl NodeKind {
    pub fn children(&self) -> Vec<NodeId> {
        match self {
            Self::Name(_)
            | Self::Register(_)
            | Self::Integer { .. }
            | Self::Float(_)
            | Self::Char(_)
            | Self::Text(_)
            | Self::Bool(_)
            | Self::Null
            | Self::SizeOf(SizeOf::Type(_))
            | Self::OffsetOf { .. }
            | Self::Global(_) => Vec::new(),
            Self::Unary { operand, .. }
            | Self::Cast { operand, .. }
            | Self::SizeOf(SizeOf::Operand(operand))
            | Self::Len(operand)
            | Self::Cap(operand)
            | Self::Call { operand, .. }
            | Self::ContainerOf {
                pointer: operand, ..
            }
            | Self::Member { base: operand, .. } => vec![*operand],
            Self::Binary { left, right, .. } => vec![*left, *right],
            Self::Assign { target, value, .. } => vec![*target, *value],
            Self::Index { base, index } => vec![*base, *index],
            Self::Conditional {
                condition,
                then,
                otherwise,
            } => vec![*condition, *then, *otherwise],
            Self::Range { base, start, end } => vec![*base, *start, *end],
            Self::Slice { base, start, end } => {
                std::iter::once(*base).chain(*start).chain(*end).collect()
            }
            Self::Tuple(items) => items.clone(),
        }
    }

    fn same_leaf(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Name(left), Self::Name(right)) => left == right,
            (Self::Register(left), Self::Register(right))
            | (Self::Global(left), Self::Global(right)) => left == right,
            (
                Self::Integer { value, suffix },
                Self::Integer {
                    value: other_value,
                    suffix: other_suffix,
                },
            ) => value == other_value && suffix == other_suffix,
            (Self::Float(left), Self::Float(right)) => left.to_value() == right.to_value(),
            (Self::Char(left), Self::Char(right)) => left == right,
            (Self::Text(left), Self::Text(right)) => left == right,
            (Self::Bool(left), Self::Bool(right)) => left == right,
            (Self::Null, Self::Null)
            | (Self::Index { .. }, Self::Index { .. })
            | (Self::Conditional { .. }, Self::Conditional { .. })
            | (Self::Range { .. }, Self::Range { .. })
            | (Self::Len(_), Self::Len(_))
            | (Self::Cap(_), Self::Cap(_))
            | (Self::SizeOf(SizeOf::Operand(_)), Self::SizeOf(SizeOf::Operand(_))) => true,
            (
                Self::Slice { start, end, .. },
                Self::Slice {
                    start: other_start,
                    end: other_end,
                    ..
                },
            ) => start.is_some() == other_start.is_some() && end.is_some() == other_end.is_some(),
            (Self::Tuple(items), Self::Tuple(other)) => items.len() == other.len(),
            (Self::Unary { op, .. }, Self::Unary { op: other, .. }) => op == other,
            (
                Self::Call { function, .. },
                Self::Call {
                    function: other, ..
                },
            ) => function == other,
            (Self::Binary { op, .. }, Self::Binary { op: other, .. }) => op == other,
            (Self::Assign { op, .. }, Self::Assign { op: other, .. }) => op == other,
            (
                Self::Cast { ty, form, .. },
                Self::Cast {
                    ty: other_ty,
                    form: other_form,
                    ..
                },
            ) => same_type(ty, other_ty) && form == other_form,
            (
                Self::Member { field, arrow, .. },
                Self::Member {
                    field: other_field,
                    arrow: other_arrow,
                    ..
                },
            ) => field == other_field && arrow == other_arrow,
            (Self::SizeOf(SizeOf::Type(left)), Self::SizeOf(SizeOf::Type(right))) => {
                same_type(left, right)
            }

            (
                Self::OffsetOf { ty, member },
                Self::OffsetOf {
                    ty: other_ty,
                    member: other_member,
                },
            )
            | (
                Self::ContainerOf { ty, member, .. },
                Self::ContainerOf {
                    ty: other_ty,
                    member: other_member,
                    ..
                },
            ) => same_type(ty, other_ty) && member == other_member,
            _ => false,
        }
    }
}

fn same_type(left: &TypeName, right: &TypeName) -> bool {
    left.base == right.base && left.pointers == right.pointers
}

#[cfg(any(test, feature = "fuzzing"))]
fn balanced(text: &str) -> bool {
    let mut open = Vec::new();
    let mut quote = None;
    let mut escaped = false;
    for character in text.chars() {
        match (quote, character) {
            (Some(_), _) if escaped => escaped = false,
            (Some('"' | '\''), '\\') => escaped = true,
            (Some(end), character) if character == end => quote = None,
            (None, '"' | '\'' | '`') => quote = Some(character),
            (None, '(' | '[') => open.push(character),
            (None, close @ (')' | ']')) => {
                let expected = if close == ')' { '(' } else { '[' };
                if open.pop() != Some(expected) {
                    return false;
                }
            }
            _ => {}
        }
    }

    open.is_empty()
}
