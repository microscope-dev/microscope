//! One Pratt parser over one precedence table.

use super::ast::{
    BinaryOp, Builtin, CWord, CastForm, Field, Node, NodeId, NodeKind, Path, Segment, Separator,
    SizeOf, Tag, Tree, TypeBase, TypeName, UnaryOp,
};

use super::lexer::{Punct, Token, TokenKind, lex};
use super::{Ambiguity, Dialect, MAX_AMBIGUITIES, Span};
use crate::eval::error::{ErrorKind, ExpressionError};

/// The deepest nesting accepted. Binding, running, and printing recurse
/// along the tree, so its height is bounded.
pub const MAX_DEPTH: usize = 64;

/// The most nodes one reading may hold.
pub const MAX_NODES: usize = 1024;

// A node's identifier is a `u32`.
const _: () = assert!(MAX_NODES < u32::MAX as usize);

// The casts a reading chooses are a bitmask over its ambiguities.
const _: () = assert!(MAX_AMBIGUITIES < usize::BITS as usize);

/// Words that are never names unless quoted.
pub const RESERVED: [&str; 8] = [
    "true", "false", "null", "as", "sizeof", "nil", "nullptr", "NULL",
];

/// Words that begin or qualify a type, and so are no bare name.
pub const QUALIFIERS: [&str; 3] = ["const", "volatile", "mut"];

/// Whether an unquoted word reads as a name wherever it stands, so that
/// printing it needs no backticks.
pub fn is_name_word(word: &str) -> bool {
    !RESERVED.contains(&word) && !is_type_word(word)
}

/// Whether an unquoted word names something where no type can stand: C's
/// type words are names there, as Go, Rust, and Zig programs may use them.
fn is_operand_word(word: &str) -> bool {
    !RESERVED.contains(&word)
}

/// Whether a word begins or qualifies a type.
pub fn is_type_word(word: &str) -> bool {
    CWord::parse(word).is_some() || Tag::parse(word).is_some() || QUALIFIERS.contains(&word)
}

// Binding strengths, loosest first.
pub const ASSIGN: u8 = 1;

pub const CONDITIONAL: u8 = 2;
const OR: u8 = 3;
const AND: u8 = 4;

pub const COMPARISON: u8 = 5;
const BIT_OR: u8 = 6;
const BIT_XOR: u8 = 7;
const BIT_AND: u8 = 8;
const SHIFT: u8 = 9;
const ADDITIVE: u8 = 10;
const MULTIPLICATIVE: u8 = 11;

pub const AS: u8 = 12;

pub const PREFIX: u8 = 13;

pub const POSTFIX: u8 = 14;

/// A binary operator's precedence.
pub const fn binary_precedence(op: BinaryOp) -> u8 {
    match op {
        BinaryOp::Mul | BinaryOp::Div | BinaryOp::Rem => MULTIPLICATIVE,
        BinaryOp::Add | BinaryOp::Sub => ADDITIVE,
        BinaryOp::Shl | BinaryOp::Shr => SHIFT,
        BinaryOp::BitAnd => BIT_AND,
        BinaryOp::BitXor => BIT_XOR,
        BinaryOp::BitOr => BIT_OR,
        BinaryOp::Eq | BinaryOp::Ne | BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge => {
            COMPARISON
        }
        BinaryOp::And => AND,
        BinaryOp::Or => OR,
    }
}

pub const fn binary_text(op: BinaryOp) -> &'static str {
    binary_punct(op).text()
}

const fn binary_punct(op: BinaryOp) -> Punct {
    match op {
        BinaryOp::Mul => Punct::Star,
        BinaryOp::Div => Punct::Slash,
        BinaryOp::Rem => Punct::Percent,
        BinaryOp::Add => Punct::Plus,
        BinaryOp::Sub => Punct::Minus,
        BinaryOp::Shl => Punct::Shl,
        BinaryOp::Shr => Punct::Shr,
        BinaryOp::BitAnd => Punct::Amp,
        BinaryOp::BitXor => Punct::Caret,
        BinaryOp::BitOr => Punct::Pipe,
        BinaryOp::Eq => Punct::EqEq,
        BinaryOp::Ne => Punct::Ne,
        BinaryOp::Lt => Punct::Lt,
        BinaryOp::Le => Punct::Le,
        BinaryOp::Gt => Punct::Gt,
        BinaryOp::Ge => Punct::Ge,
        BinaryOp::And => Punct::AndAnd,
        BinaryOp::Or => Punct::OrOr,
    }
}

const fn binary_op(punct: Punct) -> Option<BinaryOp> {
    Some(match punct {
        Punct::Star => BinaryOp::Mul,
        Punct::Slash => BinaryOp::Div,
        Punct::Percent => BinaryOp::Rem,
        Punct::Plus => BinaryOp::Add,
        Punct::Minus => BinaryOp::Sub,
        Punct::Shl => BinaryOp::Shl,
        Punct::Shr => BinaryOp::Shr,
        Punct::Amp => BinaryOp::BitAnd,
        Punct::Caret => BinaryOp::BitXor,
        Punct::Pipe => BinaryOp::BitOr,
        Punct::EqEq => BinaryOp::Eq,
        Punct::Ne => BinaryOp::Ne,
        Punct::Lt => BinaryOp::Lt,
        Punct::Le => BinaryOp::Le,
        Punct::Gt => BinaryOp::Gt,
        Punct::Ge => BinaryOp::Ge,
        Punct::AndAnd => BinaryOp::And,
        Punct::OrOr => BinaryOp::Or,
        _ => return None,
    })
}

/// A compound assignment's operator.
const fn compound(punct: Punct) -> Option<BinaryOp> {
    Some(match punct {
        Punct::PlusEq => BinaryOp::Add,
        Punct::MinusEq => BinaryOp::Sub,
        Punct::StarEq => BinaryOp::Mul,
        Punct::SlashEq => BinaryOp::Div,
        Punct::PercentEq => BinaryOp::Rem,
        Punct::AmpEq => BinaryOp::BitAnd,
        Punct::PipeEq => BinaryOp::BitOr,
        Punct::CaretEq => BinaryOp::BitXor,
        Punct::ShlEq => BinaryOp::Shl,
        Punct::ShrEq => BinaryOp::Shr,
        _ => return None,
    })
}

/// The text of an assignment's operator, `None` for an operator that does
/// not assign.
pub const fn assignment_text(op: Option<BinaryOp>) -> Option<&'static str> {
    Some(match op {
        None => "=",
        Some(BinaryOp::Add) => "+=",
        Some(BinaryOp::Sub) => "-=",
        Some(BinaryOp::Mul) => "*=",
        Some(BinaryOp::Div) => "/=",
        Some(BinaryOp::Rem) => "%=",
        Some(BinaryOp::BitAnd) => "&=",
        Some(BinaryOp::BitOr) => "|=",
        Some(BinaryOp::BitXor) => "^=",
        Some(BinaryOp::Shl) => "<<=",
        Some(BinaryOp::Shr) => ">>=",
        Some(_) => return None,
    })
}

/// One reading per combination of an expression's ambiguities.
pub type Readings = Vec<Result<Tree, ExpressionError>>;

/// Every reading of `text`, with the ambiguities that tell them apart.
pub fn parse(text: &str, dialect: Dialect) -> Result<(Vec<Ambiguity>, Readings), ExpressionError> {
    let tokens = lex(text)?;
    let starts = ambiguities(&tokens);
    if starts.len() > MAX_AMBIGUITIES {
        let span = tokens[starts[MAX_AMBIGUITIES]].span;
        return Err(ExpressionError::new(
            ErrorKind::Limit,
            span,
            format!("an expression may hold at most {MAX_AMBIGUITIES} parenthesized names before `-`, `*`, or `&`"),
        )
        .with_hint("write casts as `x as T`"));
    }

    let mut ambiguity_list = Vec::with_capacity(starts.len());
    for &start in &starts {
        let mut parser = Parser::new(text, &tokens, &starts, 0, dialect);
        parser.position = start + 1;
        let name = parser.path(true)?;
        let close = parser.peek_span();
        ambiguity_list.push(Ambiguity {
            name,
            span: tokens[start].span.to(close),
        });
    }

    let readings: Vec<_> = (0..1_usize << starts.len())
        .map(|casts| Parser::new(text, &tokens, &starts, casts, dialect).expression_tree())
        .collect();
    let first_error = match readings.first() {
        Some(Err(error)) if readings.iter().all(Result::is_err) => Some(error.clone()),
        _ => None,
    };

    if let Some(error) = first_error {
        return Err(error);
    }

    Ok((ambiguity_list, readings))
}

/// The tokens that begin a parenthesized name before `-`, `*`, or `&`.
fn ambiguities(tokens: &[Token]) -> Vec<usize> {
    let ident = |index: usize| match tokens.get(index).map(|token| &token.kind) {
        Some(TokenKind::Ident(word)) => Some(word.as_str()),
        _ => None,
    };

    let punct = |index: usize| match tokens.get(index).map(|token| &token.kind) {
        Some(TokenKind::Punct(punct)) => Some(*punct),
        _ => None,
    };

    let segment = |index: usize| {
        tokens
            .get(index)
            .is_some_and(|token| is_segment(&token.kind))
    };

    let mut starts = Vec::new();
    for open in 0..tokens.len() {
        if punct(open) != Some(Punct::OpenParen)
            || open > 0
                && ident(open - 1).is_some_and(|word| {
                    matches!(
                        word,
                        "sizeof" | "len" | "cap" | "offsetof" | "container_of" | "global"
                    ) || Builtin::parse(word).is_some()
                })
        {
            continue;
        }

        let mut index = open + 1;
        if punct(index) == Some(Punct::ColonColon) {
            index += 1;
        }

        if !segment(index) {
            continue;
        }

        index += 1;
        while matches!(punct(index), Some(Punct::ColonColon | Punct::Dot)) && segment(index + 1) {
            index += 2;
        }

        if punct(index) == Some(Punct::CloseParen)
            && matches!(
                punct(index + 1),
                Some(Punct::Minus | Punct::Star | Punct::Amp)
            )
        {
            starts.push(open);
        }
    }

    starts
}

struct Parser<'tokens> {
    text: &'tokens str,
    tokens: &'tokens [Token],
    position: usize,
    nodes: Vec<Node>,
    depth: usize,

    /// The tokens that begin ambiguities, in order.
    ambiguities: &'tokens [usize],

    /// Bit `i` set reads ambiguity `i` as a cast.
    casts: usize,
    dialect: Dialect,
}

impl<'tokens> Parser<'tokens> {
    const fn new(
        text: &'tokens str,
        tokens: &'tokens [Token],
        ambiguities: &'tokens [usize],
        casts: usize,
        dialect: Dialect,
    ) -> Self {
        Self {
            text,
            tokens,
            position: 0,
            nodes: Vec::new(),
            depth: 0,
            ambiguities,
            casts,
            dialect,
        }
    }

    fn expression_tree(mut self) -> Result<Tree, ExpressionError> {
        let root = self.expression(ASSIGN)?;
        if !matches!(self.peek(), TokenKind::End) {
            return Err(self.unexpected("an operator or the end of the expression"));
        }

        let tree = Tree {
            nodes: self.nodes,
            root,
        };

        // A range only ever is the whole expression.
        for (index, node) in tree.nodes.iter().enumerate() {
            if matches!(node.kind, NodeKind::Range { .. }) && index != tree.root.0 as usize {
                return Err(ExpressionError::syntax(
                    node.span,
                    "a range must be the whole expression",
                ));
            }
        }

        Ok(tree)
    }

    fn peek(&self) -> &TokenKind {
        &self.tokens[self.position.min(self.tokens.len() - 1)].kind
    }

    fn peek_at(&self, offset: usize) -> &TokenKind {
        &self.tokens[(self.position + offset).min(self.tokens.len() - 1)].kind
    }

    fn peek_span(&self) -> Span {
        self.tokens[self.position.min(self.tokens.len() - 1)].span
    }

    fn peek_punct(&self) -> Option<Punct> {
        match self.peek() {
            TokenKind::Punct(punct) => Some(*punct),
            _ => None,
        }
    }

    fn peek_word(&self) -> Option<&str> {
        match self.peek() {
            TokenKind::Ident(word) => Some(word),
            _ => None,
        }
    }

    const fn previous_span(&self) -> Span {
        self.tokens[self.position.saturating_sub(1)].span
    }

    fn advance(&mut self) -> Span {
        let span = self.peek_span();
        self.position = (self.position + 1).min(self.tokens.len() - 1);
        span
    }

    fn eat(&mut self, punct: Punct) -> bool {
        let found = self.peek_punct() == Some(punct);
        if found {
            self.advance();
        }

        found
    }

    fn expect(&mut self, punct: Punct, context: &str) -> Result<Span, ExpressionError> {
        if self.peek_punct() == Some(punct) {
            return Ok(self.advance());
        }

        Err(self.unexpected(&format!("`{}` {context}", punct.text())))
    }

    fn unexpected(&self, expected: &str) -> ExpressionError {
        let found = match self.peek() {
            TokenKind::End => "the end of the expression".to_owned(),
            _ => format!("`{}`", self.peek_span().text(self.text)),
        };

        ExpressionError::syntax(
            self.peek_span(),
            format!("expected {expected}, found {found}"),
        )
    }

    fn push(&mut self, kind: NodeKind, span: Span) -> Result<NodeId, ExpressionError> {
        if self.nodes.len() >= MAX_NODES {
            return Err(ExpressionError::new(
                ErrorKind::Limit,
                span,
                format!("an expression may have at most {MAX_NODES} parts"),
            ));
        }

        let id = NodeId(u32::try_from(self.nodes.len()).unwrap_or(u32::MAX));
        self.nodes.push(Node { kind, span });
        Ok(id)
    }

    fn span(&self, id: NodeId) -> Span {
        self.nodes[id.0 as usize].span
    }

    fn enter(&mut self) -> Result<(), ExpressionError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(ExpressionError::new(
                ErrorKind::Limit,
                self.peek_span(),
                format!("an expression may nest at most {MAX_DEPTH} deep"),
            ));
        }

        Ok(())
    }

    const fn leave(&mut self) {
        self.depth -= 1;
    }

    /// An expression whose operators bind at least as tightly as
    /// `minimum`.
    fn expression(&mut self, minimum: u8) -> Result<NodeId, ExpressionError> {
        self.enter()?;
        let result = self.expression_inner(minimum);
        self.leave();
        result
    }

    fn expression_inner(&mut self, minimum: u8) -> Result<NodeId, ExpressionError> {
        let mut left = self.prefix()?;
        loop {
            if self.peek_word() == Some("as") {
                if AS < minimum {
                    break;
                }

                self.advance();
                let (ty, _) = self.type_name(false)?;
                let span = self.span(left).to(ty.span);
                left = self.push(
                    NodeKind::Cast {
                        ty,
                        operand: left,
                        form: CastForm::As,
                    },
                    span,
                )?;
                continue;
            }

            let Some(punct) = self.peek_punct() else {
                break;
            };

            if let Some(op) = binary_op(punct) {
                let strength = binary_precedence(op);
                if strength < minimum {
                    break;
                }

                self.advance();
                let right = self.expression(strength + 1)?;
                let span = self.span(left).to(self.span(right));
                left = self.push(NodeKind::Binary { op, left, right }, span)?;
                if strength == COMPARISON
                    && self.peek_punct().and_then(binary_op).map(binary_precedence)
                        == Some(COMPARISON)
                {
                    return Err(ExpressionError::syntax(
                        self.peek_span(),
                        "comparisons do not chain",
                    )
                    .with_hint("join comparisons with `&&`"));
                }
            } else if punct == Punct::Question {
                if CONDITIONAL < minimum {
                    break;
                }

                self.advance();
                let then = self.expression(CONDITIONAL)?;
                self.expect(Punct::Colon, "to separate the branches of `?`")?;
                let otherwise = self.expression(CONDITIONAL)?;
                let span = self.span(left).to(self.span(otherwise));
                left = self.push(
                    NodeKind::Conditional {
                        condition: left,
                        then,
                        otherwise,
                    },
                    span,
                )?;
            } else if punct == Punct::Assign || compound(punct).is_some() {
                let op = compound(punct);
                if ASSIGN < minimum {
                    break;
                }

                self.advance();
                let value = self.expression(ASSIGN)?;
                let span = self.span(left).to(self.span(value));
                left = self.push(
                    NodeKind::Assign {
                        op,
                        target: left,
                        value,
                    },
                    span,
                )?;
            } else {
                break;
            }
        }

        Ok(left)
    }

    /// A prefix operator's operand, or a postfix expression.
    fn prefix(&mut self) -> Result<NodeId, ExpressionError> {
        self.enter()?;
        let result = self.prefix_inner();
        self.leave();
        result
    }

    fn prefix_inner(&mut self) -> Result<NodeId, ExpressionError> {
        let start = self.peek_span();
        let op = match self.peek_punct() {
            Some(Punct::Minus) => Some(UnaryOp::Neg),
            Some(Punct::Bang) => Some(UnaryOp::Not),
            Some(Punct::Tilde) => Some(UnaryOp::BitNot),
            Some(Punct::Star) => Some(UnaryOp::Deref),
            Some(Punct::Amp) => Some(UnaryOp::AddressOf),
            Some(Punct::OpenParen) => return self.parenthesized(),
            _ => None,
        };

        if let Some(op) = op {
            self.advance();
            let operand = self.prefix()?;
            let span = start.to(self.span(operand));
            return self.push(NodeKind::Unary { op, operand }, span);
        }

        if self.peek_word() == Some("sizeof") {
            return self.size_of();
        }

        let primary = self.primary()?;
        self.postfix(primary)
    }

    fn size_of(&mut self) -> Result<NodeId, ExpressionError> {
        let start = self.advance();
        self.expect(Punct::OpenParen, "after `sizeof`")?;
        let checkpoint = self.position;
        if let Ok((ty, certain)) = self.type_name(true)
            && self.peek_punct() == Some(Punct::CloseParen)
            && certain
        {
            let close = self.advance();
            return self.push(NodeKind::SizeOf(SizeOf::Type(ty)), start.to(close));
        }

        self.position = checkpoint;
        let operand = self.expression(ASSIGN)?;
        let close = self.expect(Punct::CloseParen, "to close `sizeof(`")?;
        self.push(NodeKind::SizeOf(SizeOf::Operand(operand)), start.to(close))
    }

    /// `offsetof(TYPE, member)`, which only views write.
    fn offset_of(&mut self) -> Result<NodeId, ExpressionError> {
        let start = self.advance();
        self.expect(Punct::OpenParen, "after `offsetof`")?;
        let (ty, _) = self.type_name(true)?;
        self.expect(Punct::Comma, "between `offsetof`'s type and member")?;
        let member = self.word_argument("a member's name")?;
        let close = self.expect(Punct::CloseParen, "to close `offsetof(`")?;
        self.push(NodeKind::OffsetOf { ty, member }, start.to(close))
    }

    /// `global(NAME)`, which only views write.
    fn global(&mut self) -> Result<NodeId, ExpressionError> {
        let start = self.advance();
        self.expect(Punct::OpenParen, "after `global`")?;
        let name = self.word_argument("a global's name")?;
        let close = self.expect(Punct::CloseParen, "to close `global(`")?;
        self.push(NodeKind::Global(name), start.to(close))
    }

    /// `container_of(PTR, TYPE, member)`, which only views write.
    fn container_of(&mut self) -> Result<NodeId, ExpressionError> {
        let start = self.advance();
        self.expect(Punct::OpenParen, "after `container_of`")?;
        let pointer = self.expression(ASSIGN)?;
        self.expect(Punct::Comma, "between `container_of`'s pointer and type")?;
        let (ty, _) = self.type_name(true)?;
        self.expect(Punct::Comma, "between `container_of`'s type and member")?;
        let member = self.word_argument("a member's name")?;
        let close = self.expect(Punct::CloseParen, "to close `container_of(`")?;
        self.push(
            NodeKind::ContainerOf {
                pointer,
                ty,
                member,
            },
            start.to(close),
        )
    }

    /// A plain or backticked name a view's built-in takes as an argument.
    fn word_argument(&mut self, expected: &str) -> Result<String, ExpressionError> {
        let (TokenKind::Ident(word) | TokenKind::Quoted(word)) = self.peek().clone() else {
            return Err(self.unexpected(expected));
        };

        self.advance();
        Ok(word)
    }

    /// `(` begins a cast or a parenthesized expression.
    fn parenthesized(&mut self) -> Result<NodeId, ExpressionError> {
        let open = self.position;
        let start = self.peek_span();
        if let Some(index) = self.ambiguities.iter().position(|&token| token == open) {
            if self.casts & (1 << index) != 0 {
                self.advance();
                let (ty, _) = self.type_name(true)?;
                self.expect(Punct::CloseParen, "to close the cast's type")?;
                return self.prefix_cast(start, ty);
            }

            let inner = self.grouped()?;
            return self.postfix(inner);
        }

        self.advance();
        if let Ok((ty, certain)) = self.type_name(true)
            && self.peek_punct() == Some(Punct::CloseParen)
            && (certain || begins_operand_only(self.peek_at(1)))
        {
            self.advance();
            return self.prefix_cast(start, ty);
        }

        self.position = open;
        let inner = self.grouped()?;
        self.postfix(inner)
    }

    fn prefix_cast(&mut self, start: Span, ty: TypeName) -> Result<NodeId, ExpressionError> {
        if !begins_operand(self.peek()) {
            return Err(self.unexpected(&format!(
                "an operand for the cast to `{}`",
                super::print::type_text(&ty)
            )));
        }

        let operand = self.prefix()?;
        let span = start.to(self.span(operand));
        self.push(
            NodeKind::Cast {
                ty,
                operand,
                form: CastForm::Prefix,
            },
            span,
        )
    }

    /// `( expression )`; the expression's span takes in the parentheses, so
    /// whatever contains it points at balanced text. A comma after the
    /// expression makes a tuple, as `(a, b)` and `(a,)` are, and `()` is
    /// the empty one.
    fn grouped(&mut self) -> Result<NodeId, ExpressionError> {
        let open = self.expect(Punct::OpenParen, "")?;
        if self.peek_punct() == Some(Punct::CloseParen) {
            let close = self.advance();
            return self.tuple(Vec::new(), open.to(close));
        }

        let inner = self.expression(ASSIGN)?;
        if self.peek_punct() == Some(Punct::Comma) {
            let items = self.items(inner, Punct::CloseParen)?;
            let close = self.expect(Punct::CloseParen, "to close the tuple")?;
            return self.tuple(items, open.to(close));
        }

        let close = self.expect(Punct::CloseParen, "to close `(`")?;
        self.nodes[inner.0 as usize].span = open.to(close);
        Ok(inner)
    }

    /// The items of a tuple, from the comma after its first: each comma
    /// is followed by another item, or ends the tuple before `close`.
    fn items(&mut self, first: NodeId, close: Punct) -> Result<Vec<NodeId>, ExpressionError> {
        let mut items = vec![first];
        while self.eat(Punct::Comma) && self.peek_punct() != Some(close) {
            items.push(self.expression(ASSIGN)?);
        }

        Ok(items)
    }

    fn tuple(&mut self, items: Vec<NodeId>, span: Span) -> Result<NodeId, ExpressionError> {
        if self.dialect == Dialect::View {
            return Err(ExpressionError::syntax(
                span,
                "views write no tuples: a tuple is only the key of a map an expression indexes",
            ));
        }

        self.push(NodeKind::Tuple(items), span)
    }

    fn primary(&mut self) -> Result<NodeId, ExpressionError> {
        let span = self.peek_span();
        let called = self.peek_at(1) == &TokenKind::Punct(Punct::OpenParen);
        let view_call = called && self.dialect == Dialect::View;
        let kind = match self.peek().clone() {
            TokenKind::Ident(word) => match word.as_str() {
                "true" | "false" => {
                    self.advance();
                    NodeKind::Bool(word == "true")
                }

                // `nil` is Go's spelling of `null`, and means the same.
                "null" | "nil" => {
                    self.advance();
                    NodeKind::Null
                }
                "nullptr" | "NULL" => {
                    return Err(ExpressionError::syntax(
                        span,
                        format!("`{word}` is spelled `null`"),
                    )
                    .with_hint("write `null`"));
                }
                "offsetof" if view_call => return self.offset_of(),
                "container_of" if view_call => return self.container_of(),
                "global" if view_call => return self.global(),
                "len" | "cap" if called => {
                    self.advance();
                    self.advance();
                    let operand = self.expression(ASSIGN)?;
                    let close = self.expect(Punct::CloseParen, &format!("to close `{word}(`"))?;
                    let kind = if word == "len" {
                        NodeKind::Len(operand)
                    } else {
                        NodeKind::Cap(operand)
                    };

                    return self.push(kind, span.to(close));
                }
                function if view_call && Builtin::parse(function).is_some() => {
                    let Some(function) = Builtin::parse(function) else {
                        return Err(self.unexpected("an operand"));
                    };

                    self.advance();
                    self.advance();
                    let operand = self.expression(ASSIGN)?;
                    let close = self.expect(
                        Punct::CloseParen,
                        &format!("to close `{}(`", function.name()),
                    )?;
                    return self.push(NodeKind::Call { function, operand }, span.to(close));
                }
                "as" | "sizeof" => return Err(self.unexpected("an operand")),
                _ => NodeKind::Name(self.path(false)?),
            },
            TokenKind::Quoted(_) | TokenKind::Punct(Punct::ColonColon) => {
                NodeKind::Name(self.path(false)?)
            }
            TokenKind::Register(name) => {
                self.advance();
                NodeKind::Register(name)
            }
            TokenKind::Integer { value, suffix } => {
                self.advance();
                NodeKind::Integer { value, suffix }
            }
            TokenKind::Float(value) => {
                self.advance();
                NodeKind::Float(value)
            }
            TokenKind::Char(value) => {
                self.advance();
                NodeKind::Char(value)
            }
            TokenKind::Text(bytes) => {
                self.advance();
                NodeKind::Text(bytes)
            }
            TokenKind::Punct(_) | TokenKind::End => return Err(self.unexpected("an operand")),
        };

        let span = span.to(self.previous_span());
        if matches!(self.peek(), TokenKind::Punct(Punct::OpenParen))
            && matches!(kind, NodeKind::Name(_))
        {
            return Err(ExpressionError::syntax(
                span.to(self.peek_span()),
                "expressions cannot call functions",
            ));
        }

        self.push(kind, span)
    }

    /// A name and the names `::` joins to it, and in a type name `.` too,
    /// as in Go's `main.point`. In an expression a `.` selects a member,
    /// which binding may find is part of a global's dotted name.
    fn path(&mut self, dots: bool) -> Result<Path, ExpressionError> {
        let global = self.eat(Punct::ColonColon);
        let mut segments = vec![self.segment(Separator::Colons)?];
        loop {
            let separator = match self.peek_punct() {
                Some(Punct::ColonColon) => Separator::Colons,
                Some(Punct::Dot)
                    if is_segment(self.peek_at(1)) && (dots || self.dots_then_colons()) =>
                {
                    Separator::Dot
                }
                _ => break,
            };

            self.advance();
            segments.push(self.segment(separator)?);
        }

        Ok(Path { global, segments })
    }

    /// Whether the `.`-joined names ahead continue into `::`, as a
    /// file-qualified name such as `one.c::duplicate` does.
    fn dots_then_colons(&self) -> bool {
        let mut offset = 0;
        while self.peek_at(offset) == &TokenKind::Punct(Punct::Dot)
            && is_segment(self.peek_at(offset + 1))
        {
            offset += 2;
        }

        offset > 0 && self.peek_at(offset) == &TokenKind::Punct(Punct::ColonColon)
    }

    fn segment(&mut self, separator: Separator) -> Result<Segment, ExpressionError> {
        let span = self.peek_span();
        let name = match self.peek() {
            TokenKind::Quoted(name) => name.clone(),
            TokenKind::Ident(word) if is_operand_word(word) => word.clone(),
            _ => return Err(self.unexpected("a name")),
        };

        self.advance();
        Ok(Segment {
            separator,
            name,
            span,
        })
    }

    fn postfix(&mut self, mut base: NodeId) -> Result<NodeId, ExpressionError> {
        loop {
            match self.peek_punct() {
                Some(punct @ (Punct::Dot | Punct::Arrow)) => {
                    self.advance();
                    let field_span = self.peek_span();
                    let field = match self.peek().clone() {
                        // A member name stands where no literal can, so a
                        // reserved word there is the name, as JavaScript's
                        // `values.null` is.
                        TokenKind::Ident(word) | TokenKind::Quoted(word) => Field::Named(word),
                        TokenKind::Integer {
                            value,
                            suffix: None,
                        } => Field::Index(u32::try_from(value).unwrap_or(u32::MAX)),
                        _ => return Err(self.unexpected("a member name")),
                    };

                    self.advance();
                    let span = self.span(base).to(field_span);
                    base = self.push(
                        NodeKind::Member {
                            base,
                            field,
                            field_span,
                            arrow: punct == Punct::Arrow,
                        },
                        span,
                    )?;
                }
                Some(Punct::OpenBracket) => {
                    self.advance();

                    // `[:end]` and `[:]` slice from the start.
                    if self.peek_punct() == Some(Punct::Colon) {
                        base = self.slice(base, None)?;
                        continue;
                    }

                    let index = self.expression(ASSIGN)?;

                    // `x[a, b]` indexes by the tuple `(a, b)`.
                    if self.peek_punct() == Some(Punct::Comma) {
                        let items = self.items(index, Punct::CloseBracket)?;
                        let span = self.span(index).to(self.previous_span());
                        let index = self.tuple(items, span)?;
                        let close = self.expect(Punct::CloseBracket, "to close `[`")?;
                        let span = self.span(base).to(close);
                        base = self.push(NodeKind::Index { base, index }, span)?;
                        continue;
                    }

                    if self.peek_punct() == Some(Punct::Colon) {
                        base = self.slice(base, Some(index))?;
                    } else if self.eat(Punct::DotDot) {
                        let end = self.expression(ASSIGN)?;
                        let close = self.expect(Punct::CloseBracket, "to close the range")?;
                        let span = self.span(base).to(close);
                        base = self.push(
                            NodeKind::Range {
                                base,
                                start: index,
                                end,
                            },
                            span,
                        )?;
                    } else {
                        let close = self.expect(Punct::CloseBracket, "to close `[`")?;
                        let span = self.span(base).to(close);
                        base = self.push(NodeKind::Index { base, index }, span)?;
                    }
                }
                _ => return Ok(base),
            }
        }
    }

    /// The rest of `base[start:end]`, from its `:`: the end may be left out.
    fn slice(&mut self, base: NodeId, start: Option<NodeId>) -> Result<NodeId, ExpressionError> {
        self.expect(Punct::Colon, "to separate a slice's bounds")?;
        let end = if self.peek_punct() == Some(Punct::CloseBracket) {
            None
        } else {
            Some(self.expression(ASSIGN)?)
        };

        let close = self.expect(Punct::CloseBracket, "to close the slice")?;
        let span = self.span(base).to(close);
        self.push(NodeKind::Slice { base, start, end }, span)
    }

    /// A type name, and whether it can only be a type: a bare path could
    /// also name a value. A pointer is `T*` inside a cast's parentheses,
    /// where `(*p)` dereferences, and `*T` after `as`, where a trailing `*`
    /// multiplies.
    fn type_name(&mut self, parenthesized: bool) -> Result<(TypeName, bool), ExpressionError> {
        let start = self.peek_span();
        let mut pointers = 0_u8;
        let mut qualified = false;
        loop {
            if !parenthesized && self.eat(Punct::Star) {
                pointers = pointers.saturating_add(1);
            } else if self
                .peek_word()
                .is_some_and(|word| QUALIFIERS.contains(&word))
            {
                self.advance();
                qualified = true;
            } else {
                break;
            }
        }

        let base = if let Some(tag) = self.peek_word().and_then(Tag::parse) {
            self.advance();
            TypeBase::Tagged(tag, self.path(true)?)
        } else if self.peek_word().and_then(CWord::parse).is_some() {
            let mut words = Vec::new();
            loop {
                if let Some(word) = self.peek_word().and_then(CWord::parse) {
                    words.push(word);
                } else if self
                    .peek_word()
                    .is_some_and(|word| QUALIFIERS.contains(&word))
                {
                    qualified = true;
                } else {
                    break;
                }

                self.advance();
            }

            words.sort_unstable();
            TypeBase::CWords(words)
        } else {
            TypeBase::Named(self.path(true)?)
        };

        let mut end = self.previous_span();
        loop {
            if self
                .peek_word()
                .is_some_and(|word| QUALIFIERS.contains(&word))
            {
                qualified = true;
            } else if parenthesized && self.peek_punct() == Some(Punct::Star) {
                pointers = pointers.saturating_add(1);
            } else {
                break;
            }

            end = self.advance();
        }

        let ty = TypeName {
            base,
            pointers,
            span: start.to(end),
        };

        let certain = qualified || !is_bare(&ty);
        Ok((ty, certain))
    }
}

/// Whether a type is a bare path, which could also be a value's name.
pub const fn is_bare(ty: &TypeName) -> bool {
    ty.pointers == 0 && matches!(ty.base, TypeBase::Named(_))
}

fn is_segment(token: &TokenKind) -> bool {
    match token {
        TokenKind::Quoted(_) => true,
        TokenKind::Ident(word) => is_name_word(word),
        _ => false,
    }
}

/// Whether a token can begin an operand.
fn begins_operand(token: &TokenKind) -> bool {
    begins_operand_only(token)
        || matches!(
            token,
            TokenKind::Punct(Punct::Minus | Punct::Star | Punct::Amp)
        )
}

/// Whether a token can begin an operand but cannot continue an expression.
fn begins_operand_only(token: &TokenKind) -> bool {
    match token {
        TokenKind::Ident(word) => word != "as",
        TokenKind::Quoted(_)
        | TokenKind::Register(_)
        | TokenKind::Integer { .. }
        | TokenKind::Float(_)
        | TokenKind::Char(_)
        | TokenKind::Text(_) => true,
        TokenKind::Punct(punct) => matches!(
            punct,
            Punct::OpenParen | Punct::Bang | Punct::Tilde | Punct::ColonColon
        ),
        TokenKind::End => false,
    }
}
