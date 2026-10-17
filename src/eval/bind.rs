//! Binding: resolving an expression's names and types in one scope into a
//! typed program, and refusing what its types do not allow.

use std::sync::Arc;

use super::error::{ErrorKind, ExpressionError};
use super::ir::{Capacity, Comparison, Constant, Conversion, Length, Node, Op, Program};
use super::number::{BitOperator, Bits, Exact, FloatFormat, FloatOperator, IntType, Integer};
use super::syntax::ast::{
    BinaryOp, Builtin, Field, NodeId, NodeKind, Path, Separator, SizeOf, Suffix, Tree, TypeBase,
    TypeName, UnaryOp,
};

use super::syntax::{Expression, Span};
use super::target::{Lookup, Refusal, Scope, StepKind, TypeLookup, TypeQuery};
use super::types::{
    Category, Ty, builtin, c_type_key, category, is_character, representation, size_of, type_name,
};

use crate::{CBaseType, RecordMemberLayout, TypeKind, TypeReference};

/// Whether an expression may assign.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Reading only: hovers, watches, conditions, and log messages.
    Read,

    /// Assigning too: the console and `set var`.
    Assign,
}

/// The most nodes a bound program may hold.
const MAX_BOUND_NODES: usize = 4096;

/// The most wrappers `inner` steps through.
const MAX_INNER_STEPS: usize = 64;

type Bound<S> = Node<<S as Scope>::Object, <S as Scope>::Step>;
type BindResult<S> = Result<Bound<S>, ExpressionError>;

/// What a bound program's root is made into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Finish {
    /// The value or place as written.
    AsWritten,

    /// Its truth.
    Truth,

    /// Its value: a place of a scalar type is read, and any other place
    /// stays one.
    Value,
}

/// Binds `expression` in `scope`, choosing for each ambiguity the reading
/// its name has there.
pub fn bind<S: Scope>(
    expression: &Expression,
    scope: &S,
    mode: Mode,
) -> Result<Program<S::Object, S::Step>, ExpressionError> {
    bind_as(expression, scope, mode, Finish::AsWritten)
}

/// Binds a breakpoint's condition, whose value is its truth.
pub fn bind_condition<S: Scope>(
    expression: &Expression,
    scope: &S,
) -> Result<Program<S::Object, S::Step>, ExpressionError> {
    bind_as(expression, scope, Mode::Read, Finish::Truth)
}

/// Binds an expression whose value a scope computes once and keeps, as a
/// view's `let` does: a place of a scalar type is read, and any other
/// place stays one.
pub fn bind_value<S: Scope>(
    expression: &Expression,
    scope: &S,
) -> Result<Program<S::Object, S::Step>, ExpressionError> {
    bind_as(expression, scope, Mode::Read, Finish::Value)
}

/// Whether values of a category convert only through pointers to them.
const fn aggregate(category: &Category) -> bool {
    matches!(
        category,
        Category::Record | Category::Array { .. } | Category::RuntimeArray | Category::Slice(_)
    )
}

fn bind_as<S: Scope>(
    expression: &Expression,
    scope: &S,
    mode: Mode,
    finish: Finish,
) -> Result<Program<S::Object, S::Step>, ExpressionError> {
    let mut casts = 0;
    for (index, ambiguity) in expression.ambiguities().iter().enumerate() {
        if Binder::names_value(scope, &ambiguity.name)? {
            continue;
        }

        let ty = TypeName {
            base: TypeBase::Named(ambiguity.name.clone()),
            pointers: 0,
            span: ambiguity.span,
        };

        if Binder::resolve_type(scope, &ty).is_ok() {
            casts |= 1 << index;
        } else {
            return Err(ExpressionError::new(
                ErrorKind::UnknownName,
                ambiguity.span,
                format!(
                    "`{}` names neither a value nor a type",
                    super::syntax::path_text(&ambiguity.name)
                ),
            ));
        }
    }

    let tree = expression.reading(casts).map_err(Clone::clone)?;
    let mut binder = Binder::new(scope, expression.text(), mode, tree);
    let root = match tree.kind(tree.root()) {
        NodeKind::Assign { op, target, value } if mode == Mode::Assign => {
            binder.assignment(*op, *target, *value, tree.span(tree.root()))?
        }
        _ => binder.bind(tree.root())?,
    };

    let root = if matches!(root.op, Op::Range { .. } | Op::Assign { .. }) {
        root
    } else {
        binder.settle(root)?
    };

    let root = match finish {
        Finish::AsWritten => root,
        Finish::Truth => binder.truth(root)?,
        Finish::Value => binder.value(root)?,
    };

    // A string the expression writes is only compared; one it computes
    // from the program's text, or a stop's `$path`, is a value of its own.
    if matches!(root.op, Op::Constant(Constant::Text(_)))
        && !matches!(tree.kind(tree.root()), NodeKind::Register(_))
    {
        return Err(ExpressionError::new(
            ErrorKind::Type,
            root.span,
            "a string is only compared with the program's text",
        ));
    }

    Ok(Program { root })
}

struct Binder<'a, S: Scope> {
    scope: &'a S,
    text: &'a str,
    mode: Mode,
    tree: &'a Tree,
    nodes: usize,
}

impl<'a, S: Scope> Binder<'a, S> {
    const fn new(scope: &'a S, text: &'a str, mode: Mode, tree: &'a Tree) -> Self {
        Self {
            scope,
            text,
            mode,
            tree,
            nodes: 0,
        }
    }

    const fn tree(&self) -> &'a Tree {
        self.tree
    }

    fn error(span: Span, kind: ErrorKind, message: impl Into<String>) -> ExpressionError {
        ExpressionError::new(kind, span, message)
    }

    /// A broken invariant of the binder, which no expression can cause.
    fn broken(span: Span, fact: &str) -> ExpressionError {
        Self::error(span, ErrorKind::Unsupported, format!("internal: {fact}"))
    }

    /// `$arg0` to `$arg5`, `$ret`, or `$path`: what the call that stopped
    /// the thread passed, returned, or named.
    fn call_value(&mut self, name: &str, span: Span) -> BindResult<S> {
        let unsupported = |message: &str| Self::error(span, ErrorKind::Unsupported, message);
        let call = self
            .scope
            .call_values()
            .ok_or_else(|| unsupported("only a system call's stop has its arguments and result"))?;
        let (constant, ty) = match name {
            "ret" => {
                let returned = call
                    .returned
                    .ok_or_else(|| unsupported("the call has not returned"))?;
                (Integer::Exact(Exact::from(i128::from(returned))), Ty::Exact)
            }
            "path" => {
                let path = call
                    .path
                    .clone()
                    .ok_or_else(|| unsupported("the call names no path microscope could read"))?;
                return self.node(Op::Constant(Constant::Text(path)), Ty::Text, span);
            }
            _ => {
                let index = name
                    .strip_prefix("arg")
                    .and_then(|index| index.parse::<usize>().ok())
                    .filter(|&index| index < 6)
                    .ok_or_else(|| unsupported("a call has six arguments"))?;
                (Integer::Exact(Exact::from(call.args[index])), Ty::Exact)
            }
        };

        self.node(Op::Constant(Constant::Integer(constant)), ty, span)
    }

    fn refused(span: Span, refusal: Refusal) -> ExpressionError {
        ExpressionError::new(refusal.kind, span, refusal.message)
    }

    fn ambiguous(span: Span, name: &str, candidates: &[String]) -> ExpressionError {
        let candidates: Vec<String> = candidates
            .iter()
            .map(|candidate| format!("`{candidate}`"))
            .collect();
        Self::error(
            span,
            ErrorKind::AmbiguousName,
            format!(
                "`{name}` is ambiguous; select one of {}",
                candidates.join(", ")
            ),
        )
    }

    /// The type a planned step reaches, refused when it reaches nothing the
    /// debugger can compute with.
    fn reached(
        result: Option<TypeReference>,
        span: Span,
        message: impl FnOnce() -> String,
    ) -> Result<Ty, ExpressionError> {
        result
            .map(Ty::Program)
            .ok_or_else(|| Self::error(span, ErrorKind::Unsupported, message()))
    }

    /// An operand whose type the debugger cannot compute with.
    fn unsupported(&self, span: Span, reason: &str) -> ExpressionError {
        Self::error(
            span,
            ErrorKind::Unsupported,
            format!("`{}` cannot be computed with: {reason}", self.quote(span)),
        )
    }

    /// The text a span covers, for messages.
    fn quote(&self, span: Span) -> &str {
        span.text(self.text)
    }

    fn node(&mut self, op: Op<S::Object, S::Step>, ty: Ty, span: Span) -> BindResult<S> {
        self.nodes += 1;
        if self.nodes > MAX_BOUND_NODES {
            return Err(Self::error(
                span,
                ErrorKind::Limit,
                "the expression binds to too many operations",
            ));
        }

        Ok(Node { op, ty, span })
    }

    fn category(&self, ty: &Ty) -> Category {
        category(self.scope, ty)
    }

    #[expect(clippy::too_many_lines, reason = "one arm per kind of syntax node")]
    fn bind(&mut self, id: NodeId) -> BindResult<S> {
        let span = self.tree().span(id);
        match self.tree().kind(id).clone() {
            NodeKind::Name(path) => self.name(&path, span),
            NodeKind::Register(name) if name == "task" => self.node(Op::Task, Ty::Exact, span),
            // The frame names its future `$future`, which no source
            // variable can be named.
            NodeKind::Register(name) if name == "future" => {
                match self
                    .scope
                    .lookup("$future", false)
                    .map_err(|refusal| Self::refused(span, refusal))?
                {
                    Lookup::Object { object, ty } => self.object("$future", object, ty, span),
                    _ => Err(Self::error(
                        span,
                        ErrorKind::UnknownName,
                        "the selected frame runs no future",
                    )),
                }
            }
            NodeKind::Register(name) if call_value(&name) => self.call_value(&name, span),
            NodeKind::Register(name) => {
                let register = self.scope.register(&name).ok_or_else(|| {
                    Self::error(
                        span,
                        ErrorKind::UnknownName,
                        format!("there is no register `${name}`"),
                    )
                })?;
                let Some(int) = IntType::new(register.width, false) else {
                    return Err(Self::error(
                        span,
                        ErrorKind::Unsupported,
                        format!("`${name}` has a width of {} bits", register.width),
                    ));
                };

                let ty = Ty::Int(int);
                self.node(Op::Register(register), ty, span)
            }
            NodeKind::Integer { value, suffix } => self.integer(value, suffix, false, span),
            NodeKind::Float(value) => self.node(
                Op::Constant(Constant::Float(value)),
                Ty::Float(value.format()),
                span,
            ),
            NodeKind::Char(value) => self.node(
                Op::Constant(Constant::Integer(Integer::Exact(Exact::from(u128::from(
                    value,
                ))))),
                Ty::Exact,
                span,
            ),
            NodeKind::Text(bytes) => self.node(Op::Constant(Constant::Text(bytes)), Ty::Text, span),
            NodeKind::Bool(value) => self.node(Op::Constant(Constant::Bool(value)), Ty::Bool, span),
            NodeKind::Null => self.node(Op::Constant(Constant::Pointer(0)), Ty::Null, span),
            NodeKind::Unary { op, operand } => self.unary(op, operand, span),
            NodeKind::Binary { op, left, right } => self.binary(op, left, right, span),
            NodeKind::Conditional {
                condition,
                then,
                otherwise,
            } => self.conditional(condition, then, otherwise, span),
            NodeKind::Assign { .. } => match self.mode {
                Mode::Read => Err(Self::error(
                    span,
                    ErrorKind::Mode,
                    "assignments are only made in the console and with `set var`",
                )),
                Mode::Assign => Err(Self::error(
                    span,
                    ErrorKind::Mode,
                    "an assignment must be the whole expression",
                )),
            },
            NodeKind::Cast { ty, operand, .. } => {
                let target = Self::resolve_type(self.scope, &ty)?;
                let operand = self.bind(operand)?;
                self.convert(operand, target, span)
            }

            NodeKind::Member {
                base,
                field,
                field_span,
                arrow,
            } => {
                // `a.b.c` may name a global, as Go's `main.counter` does: the
                // longest name the scope knows is taken first.
                if let Some(path) = self.dotted(id) {
                    return self.name(&path, span);
                }

                let base = self.bind(base)?;
                self.member(base, &field, field_span, arrow, span)
            }
            NodeKind::Index { .. } => self.index(id),
            NodeKind::Range { base, start, end } => {
                let base = self.bind(base)?;
                let base = self.settle(base)?;
                if !base.is_place()
                    || !matches!(
                        self.category(&base.ty),
                        Category::Array { .. } | Category::Slice(_)
                    )
                {
                    return Err(Self::error(
                        base.span,
                        ErrorKind::Type,
                        format!("`{}` is not an array or slice", self.quote(base.span)),
                    ));
                }

                let start = self.bound_index(start)?;
                let end = self.bound_index(end)?;
                let ty = base.ty.clone();
                self.node(
                    Op::Range {
                        base: Box::new(base),
                        start: Some(Box::new(start)),
                        end: Some(Box::new(end)),
                    },
                    ty,
                    span,
                )
            }
            NodeKind::Slice { base, start, end } => self.slice(id, base, start, end, span),
            NodeKind::SizeOf(SizeOf::Type(ty)) => {
                let ty = Self::resolve_type(self.scope, &ty)?;
                self.size_constant(&ty, span)
            }
            NodeKind::SizeOf(SizeOf::Operand(operand)) => {
                if let NodeKind::Name(path) = self.tree().kind(operand).clone()
                    && !Self::names_value(self.scope, &path)?
                {
                    let name = TypeName {
                        base: TypeBase::Named(path),
                        pointers: 0,
                        span: self.tree().span(operand),
                    };

                    let ty = Self::resolve_type(self.scope, &name)?;
                    return self.size_constant(&ty, span);
                }

                let operand = self.bind(operand)?;
                let operand = self.settle(operand)?;
                self.size_constant(&operand.ty, span)
            }
            NodeKind::OffsetOf { ty, member } => {
                let ty = Self::resolve_type(self.scope, &ty)?;
                self.offset_constant(&ty, &member, span)
            }

            NodeKind::ContainerOf {
                pointer,
                ty,
                member,
            } => {
                let pointer = self.bind(pointer)?;
                let target = Self::resolve_type(self.scope, &ty)?;
                self.container_of(pointer, target, &member, span)
            }
            NodeKind::Global(name) => match self.scope.global(&name) {
                Ok(Lookup::Object { object, ty }) => self.object(&name, object, ty, span),
                Ok(Lookup::Ambiguous(candidates)) => Err(Self::ambiguous(span, &name, &candidates)),
                Ok(_) => Err(Self::error(
                    span,
                    ErrorKind::UnknownName,
                    format!("the module defines no global `{name}`"),
                )),
                Err(refusal) => Err(Self::refused(span, refusal)),
            },
            NodeKind::Len(operand) => {
                let operand = self.bind(operand)?;
                self.length(operand, span)
            }
            NodeKind::Cap(operand) => {
                let operand = self.bind(operand)?;
                self.capacity(operand, span)
            }

            NodeKind::Call {
                function: Builtin::Inner,
                operand,
            } => {
                let operand = self.bind(operand)?;
                self.inner(operand, span)
            }
        }
    }

    /// `inner(x)`: while `x` is a record with exactly one member of
    /// non-zero size, and no base, that member. Zero-sized markers, such as
    /// Rust's `PhantomData`, are not counted.
    fn inner(&mut self, mut node: Bound<S>, span: Span) -> BindResult<S> {
        for _ in 0..MAX_INNER_STEPS {
            node = self.settle(node)?;
            let Ty::Program(reference) = node.ty else {
                return Ok(node);
            };

            if !node.is_place() {
                return Ok(node);
            }

            let Ok((_, info)) = representation(self.scope, reference) else {
                return Ok(node);
            };

            let TypeKind::Record { members, bases, .. } = &info.kind else {
                return Ok(node);
            };

            let sized = members
                .iter()
                .filter(|member| {
                    representation(self.scope, member.type_ref)
                        .ok()
                        .and_then(|(_, info)| info.byte_size)
                        != Some(0)
                })
                .collect::<Vec<_>>();
            let ([member], true) = (sized.as_slice(), bases.is_empty()) else {
                return Ok(node);
            };

            let Some(name) = member.name.as_deref() else {
                return Ok(node);
            };

            node = self.member(node, &Field::Named(name.to_owned()), span, false, span)?;
        }

        Err(Self::error(
            span,
            ErrorKind::Limit,
            format!("`inner` stepped through {MAX_INNER_STEPS} wrappers"),
        ))
    }

    // ---- Names ----

    /// A chain of `.name` selections from a name, as one dotted path.
    fn dotted(&self, id: NodeId) -> Option<Path> {
        match self.tree().kind(id) {
            NodeKind::Name(path) => Some(path.clone()),
            NodeKind::Member {
                base,
                field: Field::Named(name),
                field_span,
                arrow: false,
            } => {
                let mut path = self.dotted(*base)?;
                path.segments.push(super::syntax::ast::Segment {
                    separator: Separator::Dot,
                    name: name.clone(),
                    span: *field_span,
                });
                Some(path)
            }
            _ => None,
        }
    }

    /// The text of a path's first `count` segments.
    fn prefix_text(path: &Path, count: usize) -> String {
        let mut text = String::new();
        for (index, segment) in path.segments.iter().take(count).enumerate() {
            if index > 0 {
                text.push_str(match segment.separator {
                    Separator::Colons => "::",
                    Separator::Dot => ".",
                });
            }

            text.push_str(&segment.name);
        }

        text
    }

    /// Where a path may split into a name and member selections: before
    /// each `.`, longest name first.
    fn splits(path: &Path) -> impl Iterator<Item = usize> + '_ {
        (1..=path.segments.len()).rev().filter(|&count| {
            path.segments
                .get(count)
                .is_none_or(|next| next.separator == Separator::Dot)
        })
    }

    /// Whether a path names a value, possibly through members.
    fn names_value(scope: &S, path: &Path) -> Result<bool, ExpressionError> {
        for count in Self::splits(path) {
            let span = path.segments[0].span.to(path.segments[count - 1].span);
            match scope
                .lookup(&Self::prefix_text(path, count), path.global)
                .map_err(|refusal| Self::refused(span, refusal))?
            {
                Lookup::NotFound => {}
                _ => return Ok(true),
            }
        }

        Ok(false)
    }

    fn name(&mut self, path: &Path, span: Span) -> BindResult<S> {
        for count in Self::splits(path) {
            let name = Self::prefix_text(path, count);
            let name_span = path.segments[0].span.to(path.segments[count - 1].span);
            let found = self
                .scope
                .lookup(&name, path.global)
                .map_err(|refusal| Self::refused(name_span, refusal))?;
            let mut node = match found {
                Lookup::NotFound => continue,
                Lookup::Ambiguous(candidates) => {
                    return Err(Self::ambiguous(name_span, &name, &candidates));
                }
                Lookup::Enumerator { value, ty } => {
                    if count < path.segments.len() {
                        return Err(Self::error(
                            span,
                            ErrorKind::Type,
                            format!("the enumerator `{name}` has no members"),
                        ));
                    }

                    return self.enumerator(value, ty, span);
                }
                Lookup::Constant(value) => {
                    if count < path.segments.len() {
                        return Err(Self::error(
                            span,
                            ErrorKind::Type,
                            format!("the constant `{name}` has no members"),
                        ));
                    }

                    return self.node(
                        Op::Constant(Constant::Integer(Integer::Exact(value))),
                        Ty::Exact,
                        span,
                    );
                }
                Lookup::Bound { object, ty } => self.node(Op::Bound(object), ty, name_span)?,
                Lookup::Object { object, ty } => self.object(&name, object, ty, name_span)?,
                Lookup::Dynamic(object) => self.node(Op::Object(object), Ty::Dynamic, name_span)?,
            };

            for segment in &path.segments[count..] {
                let member_span = path.segments[0].span.to(segment.span);
                node = self.member(
                    node,
                    &Field::Named(segment.name.clone()),
                    segment.span,
                    false,
                    member_span,
                )?;
            }

            return Ok(node);
        }

        let name = Self::prefix_text(path, path.segments.len());
        Err(Self::error(
            span,
            ErrorKind::UnknownName,
            format!("no variable is named `{name}` here"),
        ))
    }

    fn object(
        &mut self,
        name: &str,
        object: S::Object,
        ty: Result<TypeReference, Arc<str>>,
        span: Span,
    ) -> BindResult<S> {
        let ty = ty.map_err(|reason| {
            Self::error(
                span,
                ErrorKind::Unsupported,
                format!("`{name}` has a malformed type: {reason}"),
            )
        })?;
        self.node(Op::Object(object), Ty::Program(ty), span)
    }

    fn enumerator(&mut self, value: Exact, ty: TypeReference, span: Span) -> BindResult<S> {
        let ty = Ty::Program(ty);
        let integer = match self.category(&ty) {
            Category::Integer { int: Some(int), .. } => Integer::Typed(Bits::truncate(int, value)),
            _ => Integer::Exact(value),
        };

        self.node(Op::Constant(Constant::Integer(integer)), ty, span)
    }

    // ---- Literals ----

    fn integer(
        &mut self,
        value: u128,
        suffix: Option<Suffix>,
        negated: bool,
        span: Span,
    ) -> BindResult<S> {
        let exact = Exact::from(value);
        let exact = if negated {
            exact
                .neg()
                .map_err(|error| Self::error(span, ErrorKind::Arithmetic, error.to_string()))?
        } else {
            exact
        };

        let int = match suffix {
            None => {
                return self.node(
                    Op::Constant(Constant::Integer(Integer::Exact(exact))),
                    Ty::Exact,
                    span,
                );
            }
            Some(Suffix::Int { width, signed }) => IntType::new(width, signed),
            Some(Suffix::Size { signed }) => {
                IntType::new(self.scope.pointer_size().saturating_mul(8), signed)
            }
            Some(Suffix::F32 | Suffix::F64) => None,
        };

        let int =
            int.ok_or_else(|| Self::error(span, ErrorKind::Syntax, "the suffix names no type"))?;
        let bits = Bits::exactly(int, exact).map_err(|_| {
            Self::error(
                span,
                ErrorKind::Arithmetic,
                format!(
                    "{exact} does not fit {}",
                    type_name(self.scope, &Ty::Int(int))
                ),
            )
        })?;
        self.node(
            Op::Constant(Constant::Integer(Integer::Typed(bits))),
            Ty::Int(int),
            span,
        )
    }

    // ---- Types ----

    /// The type a type name means: a built-in, then the program's.
    fn resolve_type(scope: &S, name: &TypeName) -> Result<Ty, ExpressionError> {
        let span = name.span;
        let query = match &name.base {
            TypeBase::Named(path) => {
                if let [segment] = path.segments.as_slice()
                    && !path.global
                    && let Some(ty) = builtin(&segment.name, scope.pointer_size())
                {
                    return Ok(Self::pointers(ty, name.pointers));
                }

                TypeQuery {
                    name: Self::prefix_text(path, path.segments.len()),
                    tag: None,
                }
            }
            TypeBase::CWords(words) => TypeQuery {
                name: c_type_key(words).ok_or_else(|| {
                    Self::error(span, ErrorKind::UnknownName, "the words spell no C type")
                })?,
                tag: None,
            },
            TypeBase::Tagged(tag, path) => TypeQuery {
                name: Self::prefix_text(path, path.segments.len()),
                tag: Some(*tag),
            },
        };

        match scope.lookup_type(&query) {
            TypeLookup::Found(ty) => Ok(Self::pointers(Ty::Program(ty), name.pointers)),
            TypeLookup::Ambiguous(candidates) => Err(Self::error(
                span,
                ErrorKind::AmbiguousName,
                format!(
                    "several types are named `{}`: {}",
                    query.name,
                    candidates.join(", ")
                ),
            )),
            // C's base types are the target's where the program describes
            // none, as a Go program describes no `unsigned char`.
            TypeLookup::NotFound => match &name.base {
                TypeBase::CWords(_)
                    if let Some(c) = CBaseType::from_name(&query.name)
                        && scope.c_base_type(c).is_some() =>
                {
                    Ok(Self::pointers(Ty::C(c), name.pointers))
                }
                _ => Err(Self::error(
                    span,
                    ErrorKind::UnknownName,
                    format!("no type is named `{}`", query.name),
                )),
            },
        }
    }

    fn pointers(mut ty: Ty, count: u8) -> Ty {
        for _ in 0..count {
            ty = Ty::Pointer(Arc::new(ty));
        }

        ty
    }

    fn size_constant(&mut self, ty: &Ty, span: Span) -> BindResult<S> {
        let size = size_of(self.scope, ty).ok_or_else(|| {
            Self::error(
                span,
                ErrorKind::Type,
                format!("`{}` has no size", type_name(self.scope, ty)),
            )
        })?;
        self.node(
            Op::Constant(Constant::Integer(Integer::Exact(Exact::from(u128::from(
                size,
            ))))),
            Ty::Exact,
            span,
        )
    }

    /// `offsetof(TYPE, member)`: the byte offset of one of a record's own
    /// members, as its debug information places it.
    fn offset_constant(&mut self, ty: &Ty, member: &str, span: Span) -> BindResult<S> {
        let name = type_name(self.scope, ty);
        let Ty::Program(reference) = ty else {
            return Err(Self::error(
                span,
                ErrorKind::Type,
                format!("`{name}` has no members"),
            ));
        };

        let info = representation(self.scope, *reference)
            .map_err(|reason| Self::error(span, ErrorKind::Type, reason.to_string()))?
            .1;
        let TypeKind::Record { members, .. } = &info.kind else {
            return Err(Self::error(
                span,
                ErrorKind::Type,
                format!("`{name}` has no members"),
            ));
        };

        let found = members
            .iter()
            .find(|candidate| candidate.name.as_deref() == Some(member))
            .ok_or_else(|| {
                Self::error(
                    span,
                    ErrorKind::Type,
                    format!("`{name}` has no member `{member}` of its own"),
                )
            })?;
        let RecordMemberLayout::ByteOffset(offset) = found.layout else {
            return Err(Self::error(
                span,
                ErrorKind::Type,
                format!("`{member}` of `{name}` is not at a whole byte"),
            ));
        };

        self.node(
            Op::Constant(Constant::Integer(Integer::Exact(Exact::from(u128::from(
                offset,
            ))))),
            Ty::Exact,
            span,
        )
    }

    /// `container_of(PTR, TYPE, member)`: the address of the `TYPE` whose
    /// `member` is at `PTR`, the member's offset before it.
    fn container_of(
        &mut self,
        pointer: Bound<S>,
        target: Ty,
        member: &str,
        span: Span,
    ) -> BindResult<S> {
        let pointer = self.value(pointer)?;
        let category = self.category(&pointer.ty);
        let Category::Pointer(target_of) = &category else {
            return Err(self.type_error(&pointer, &category, "is not a pointer"));
        };

        // The pointer points to the member's type, or to nothing in
        // particular, so a mistyped view cannot locate the wrong object.
        let offset = self.offset_constant(&target, member, span)?;
        let member_type = match &target {
            Ty::Program(reference) => {
                representation(self.scope, *reference)
                    .ok()
                    .and_then(|(_, info)| match &info.kind {
                        TypeKind::Record { members, .. } => members
                            .iter()
                            .find(|candidate| candidate.name.as_deref() == Some(member))
                            .map(|candidate| candidate.type_ref),
                        _ => None,
                    })
            }
            _ => None,
        };

        let canonical = |ty: TypeReference| representation(self.scope, ty).ok().map(|(ty, _)| ty);
        let points_to_member = match (target_of, member_type) {
            (None | Some(Ty::Void), _) => true,
            (Some(Ty::Program(points_to)), Some(member_type)) => canonical(*points_to)
                .zip(canonical(member_type))
                .is_some_and(|(left, right)| self.scope.same_type(left, right)),
            _ => false,
        };

        if !points_to_member {
            return Err(Self::error(
                pointer.span,
                ErrorKind::Type,
                format!(
                    "points to `{}`, not to the type of `{member}`",
                    target_of
                        .as_ref()
                        .map_or_else(|| "void".to_owned(), |ty| type_name(self.scope, ty))
                ),
            ));
        }

        let ty = pointer.ty.clone();
        let moved = self.node(
            Op::Offset {
                pointer: Box::new(pointer),
                count: Box::new(offset),
                scale: 1,
                backward: true,
            },
            ty,
            span,
        )?;
        self.convert(moved, Ty::Pointer(Arc::new(target)), span)
    }

    // ---- Places and values ----

    /// A language reference stands for what it refers to.
    fn settle(&mut self, node: Bound<S>) -> BindResult<S> {
        if matches!(self.category(&node.ty), Category::Reference) {
            return self.deref_place(node);
        }

        Ok(node)
    }

    /// Follows a place holding a pointer or reference, with the provider's
    /// own step, which knows implicit pointers and address classes.
    fn deref_place(&mut self, node: Bound<S>) -> BindResult<S> {
        let span = node.span;
        let Ty::Program(from) = node.ty else {
            return Err(Self::broken(
                span,
                "only program types are stored in places",
            ));
        };

        let planned = self
            .scope
            .plan(from, StepKind::Deref)
            .map_err(|refusal| Self::refused(span, refusal))?;
        let ty = planned.result.map_or_else(
            || {
                Err(Self::error(
                    span,
                    ErrorKind::Type,
                    format!("`{}` points at nothing usable", self.quote(span)),
                ))
            },
            |ty| Ok(Ty::Program(ty)),
        )?;
        self.node(
            Op::Step {
                base: Box::new(node),
                step: planned.step,
                indices: Vec::new(),
                follows: true,
            },
            ty,
            span,
        )
    }

    /// The place a pointer value points at.
    fn deref_value(&mut self, pointer: Bound<S>, target: &Ty, span: Span) -> BindResult<S> {
        match target {
            Ty::Program(reference) => self.node(
                Op::At {
                    address: Box::new(pointer),
                    pointee: *reference,
                },
                target.clone(),
                span,
            ),
            Ty::Void => Err(Self::error(
                pointer.span,
                ErrorKind::Type,
                format!("`{}` points at void", self.quote(pointer.span)),
            )
            .with_hint("cast it to a pointer to the type it points at")),
            other => self.node(
                Op::Raw {
                    address: Box::new(pointer),
                },
                other.clone(),
                span,
            ),
        }
    }

    /// Dereferences a pointer-typed node, stored or computed.
    fn deref(&mut self, node: Bound<S>, span: Span) -> BindResult<S> {
        let node = self.settle(node)?;
        match self.category(&node.ty) {
            Category::Pointer(Some(_)) if node.is_place() && matches!(node.ty, Ty::Program(_)) => {
                let mut stepped = self.deref_place(node)?;
                stepped.span = span;
                Ok(stepped)
            }
            Category::Pointer(Some(pointee)) => {
                let value = self.value(node)?;
                self.deref_value(value, &pointee, span)
            }
            Category::Pointer(None) => Err(Self::error(
                node.span,
                ErrorKind::Type,
                format!("`{}` points at void", self.quote(node.span)),
            )
            .with_hint("cast it to a pointer to the type it points at")),
            Category::Array { .. } => {
                // `*array` is its first element.
                let pointer = self.operand(node)?;
                let Ty::Pointer(element) = pointer.ty.clone() else {
                    return Err(Self::broken(span, "an array decays to a pointer"));
                };

                self.deref_value(pointer, &element, span)
            }

            // A value a view presents as another, as a smart pointer
            // presents what it points to, dereferences to that one.
            category => match &node.ty {
                Ty::Program(from)
                    if node.is_place() && !matches!(category, Category::Opaque(_)) =>
                {
                    match self.scope.plan(*from, StepKind::Deref) {
                        Ok(super::target::Planned {
                            step,
                            result: Some(ty),
                            ..
                        }) => self.node(
                            Op::Step {
                                base: Box::new(node),
                                step,
                                indices: Vec::new(),
                                follows: true,
                            },
                            Ty::Program(ty),
                            span,
                        ),
                        _ => Err(self.type_error(&node, &category, "is not a pointer")),
                    }
                }
                _ => Err(self.type_error(&node, &category, "is not a pointer")),
            },
        }
    }

    fn type_error(&self, node: &Bound<S>, category: &Category, what: &str) -> ExpressionError {
        if let Category::Opaque(reason) = category {
            return self.unsupported(node.span, reason);
        }

        Self::error(
            node.span,
            ErrorKind::Type,
            format!(
                "`{}` {what}; its type is `{}`",
                self.quote(node.span),
                type_name(self.scope, &node.ty)
            ),
        )
    }

    /// The value of a node: a place's scalar is loaded.
    fn value(&mut self, node: Bound<S>) -> BindResult<S> {
        let node = self.settle(node)?;
        if !node.is_place() {
            return Ok(node);
        }

        match self.category(&node.ty) {
            Category::Integer { .. }
            | Category::Float(_)
            | Category::Bool
            | Category::Pointer(_) => {
                let (ty, span) = (node.ty.clone(), node.span);
                self.node(Op::Load(Box::new(node)), ty, span)
            }
            Category::Opaque(reason) => Err(self.unsupported(node.span, &reason)),
            _ => Ok(node),
        }
    }

    /// An operand of arithmetic or comparison: arrays decay to a pointer to
    /// their first element.
    fn operand(&mut self, node: Bound<S>) -> BindResult<S> {
        let node = self.settle(node)?;
        if let Category::Array { element, .. } = self.category(&node.ty)
            && node.is_place()
        {
            let span = node.span;
            return self.node(
                Op::Decay(Box::new(node)),
                Ty::Pointer(Arc::new(Ty::Program(element))),
                span,
            );
        }

        self.value(node)
    }

    fn integer_value(&mut self, node: Bound<S>) -> BindResult<S> {
        let node = self.value(node)?;
        match self.category(&node.ty) {
            Category::Integer { .. } => Ok(node),
            category => Err(self.type_error(&node, &category, "is not an integer")),
        }
    }

    /// A truth value: booleans, or numbers and pointers, true when nonzero.
    fn truth(&mut self, node: Bound<S>) -> BindResult<S> {
        let node = self.operand(node)?;
        match self.category(&node.ty) {
            Category::Bool => Ok(node),
            category @ (Category::Integer { .. }
            | Category::Float(_)
            | Category::Pointer(_)
            | Category::Null
            | Category::Dynamic) => {
                // A value of a dynamic type is true as its language says,
                // as Python's empty containers are false.
                let node = if matches!(category, Category::Dynamic) {
                    self.dynamic_value(node)?
                } else {
                    node
                };

                let span = node.span;
                self.node(
                    Op::Convert {
                        operand: Box::new(node),
                        conversion: Conversion::Truth,
                    },
                    Ty::Bool,
                    span,
                )
            }
            category => Err(self.type_error(&node, &category, "has no truth value")),
        }
    }

    // ---- Operators ----

    fn unary(&mut self, op: UnaryOp, operand: NodeId, span: Span) -> BindResult<S> {
        if op == UnaryOp::Neg
            && let NodeKind::Integer { value, suffix } = self.tree().kind(operand).clone()
        {
            // A negative literal of a type, such as `-128i8`, is written as
            // the negation of a literal that alone would not fit.
            return self.integer(value, suffix, true, span);
        }

        let operand = self.bind(operand)?;
        if matches!(op, UnaryOp::Neg | UnaryOp::BitNot) && self.is_dynamic(&operand) {
            let operand = self.dynamic_value(operand)?;
            return self.node(
                Op::DynamicUnary {
                    op,
                    operand: Box::new(operand),
                },
                Ty::Dynamic,
                span,
            );
        }

        match op {
            UnaryOp::Neg => {
                let operand = self.value(operand)?;
                match self.category(&operand.ty) {
                    Category::Integer { .. } => {
                        self.node(Op::Negate(Box::new(operand)), Ty::Exact, span)
                    }
                    Category::Float(format) => {
                        self.node(Op::FloatNegate(Box::new(operand)), Ty::Float(format), span)
                    }
                    category => Err(self.type_error(&operand, &category, "is not a number")),
                }
            }
            UnaryOp::Not => {
                let operand = self.truth(operand)?;
                self.node(Op::Not(Box::new(operand)), Ty::Bool, span)
            }
            UnaryOp::BitNot => {
                let operand = self.integer_value(operand)?;
                let ty = operand.ty.clone();
                self.node(Op::BitNot(Box::new(operand)), ty, span)
            }
            UnaryOp::Deref => self.deref(operand, span),
            UnaryOp::AddressOf => {
                let operand = self.settle(operand)?;
                if !operand.is_place() {
                    return Err(Self::error(
                        operand.span,
                        ErrorKind::NotAnLvalue,
                        format!(
                            "`{}` is a computed value, which has no address",
                            self.quote(operand.span)
                        ),
                    ));
                }

                // A value of a dynamic type is at its object's address,
                // which holds no type the evaluator knows.
                let pointee = if matches!(operand.ty, Ty::Dynamic) {
                    Ty::Void
                } else {
                    operand.ty.clone()
                };

                let ty = Ty::Pointer(Arc::new(pointee));
                self.node(Op::AddressOf(Box::new(operand)), ty, span)
            }
        }
    }

    /// Binds an operand that may be an enumerator of the other operand's
    /// enumeration, found there when the scope finds no single value by its
    /// name.
    fn bind_beside(&mut self, id: NodeId, other: Option<&Bound<S>>) -> BindResult<S> {
        let bound = self.bind(id);
        let Err(error) = &bound else {
            return bound;
        };

        let (Some(other), NodeKind::Name(path)) = (other, self.tree().kind(id)) else {
            return bound;
        };

        if !matches!(
            error.kind,
            ErrorKind::UnknownName | ErrorKind::AmbiguousName
        ) || path.segments.len() != 1
            || path.global
        {
            return bound;
        }

        let Category::Integer {
            enumerators: Some(enumerators),
            ..
        } = self.category(&other.ty)
        else {
            return bound;
        };

        let Ty::Program(enumeration) = other.ty else {
            return bound;
        };

        let name = &path.segments[0].name;
        let Some(found) = enumerators
            .iter()
            .find(|enumerator| enumerator.name.as_ref() == name)
        else {
            return bound;
        };

        self.enumerator(Exact::from(found.value), enumeration, self.tree().span(id))
    }

    /// Binds both operands of a binary operator, letting either name an
    /// enumerator of the other's enumeration.
    fn operands(
        &mut self,
        left: NodeId,
        right: NodeId,
    ) -> Result<(Bound<S>, Bound<S>), ExpressionError> {
        match self.bind(left) {
            Ok(left) => {
                let right = self.bind_beside(right, Some(&left))?;
                Ok((left, right))
            }
            Err(error) => {
                let Ok(right) = self.bind(right) else {
                    return Err(error);
                };

                let left = self.bind_beside(left, Some(&right))?;
                Ok((left, right))
            }
        }
    }

    fn binary(&mut self, op: BinaryOp, left: NodeId, right: NodeId, span: Span) -> BindResult<S> {
        if matches!(op, BinaryOp::And | BinaryOp::Or) {
            let left = self.bind(left)?;
            let left = self.truth(left)?;
            let right = self.bind(right)?;
            let right = self.truth(right)?;
            return self.node(
                Op::Logical {
                    and: op == BinaryOp::And,
                    left: Box::new(left),
                    right: Box::new(right),
                },
                Ty::Bool,
                span,
            );
        }

        if matches!(op, BinaryOp::Eq | BinaryOp::Ne) {
            let negate = op == BinaryOp::Ne;
            if let Some(test) = self.holds_variant(left, right, negate, span)? {
                return Ok(test);
            }

            if let Some(test) = self.holds_variant(right, left, negate, span)? {
                return Ok(test);
            }
        }

        let (left, right) = self.operands(left, right)?;
        self.binary_bound(op, left, right, span)
    }

    /// `value == Name`: whether a tagged union holds its variant `Name`,
    /// when `name` is a bare name the scope finds no single value by and
    /// one of the union's variants has. `None` for any other comparison.
    fn holds_variant(
        &mut self,
        value: NodeId,
        name: NodeId,
        negate: bool,
        span: Span,
    ) -> Result<Option<Bound<S>>, ExpressionError> {
        let NodeKind::Name(path) = self.tree().kind(name) else {
            return Ok(None);
        };

        if path.segments.len() != 1 || path.global {
            return Ok(None);
        }

        let variant = path.segments[0].name.clone();
        if let Err(error) = self.bind(name) {
            if !matches!(
                error.kind,
                ErrorKind::UnknownName | ErrorKind::AmbiguousName
            ) {
                return Ok(None);
            }
        } else {
            return Ok(None);
        }

        let Ok(bound) = self.bind(value) else {
            return Ok(None);
        };

        let base = self.settle(bound)?;
        let Ty::Program(from) = base.ty else {
            return Ok(None);
        };

        let Ok((_, info)) = representation(self.scope, from) else {
            return Ok(None);
        };

        let TypeKind::Variant { variants, .. } = &info.kind else {
            return Ok(None);
        };

        let names = |variant: &crate::Variant| {
            variant
                .name
                .clone()
                .into_iter()
                .chain(match &*variant.members {
                    [member] => member.name.clone(),
                    _ => None,
                })
        };

        if !variants
            .iter()
            .any(|candidate| names(candidate).any(|known| *known == *variant))
        {
            return Ok(None);
        }

        if !base.is_place() {
            return Err(Self::error(
                base.span,
                ErrorKind::Type,
                format!(
                    "`{}` is a computed value, whose variant cannot be read",
                    self.quote(base.span)
                ),
            ));
        }

        let step = self
            .scope
            .plan(from, StepKind::Member(&variant))
            .map_err(|refusal| Self::refused(self.tree().span(name), refusal))?
            .step;
        self.node(
            Op::Holds {
                base: Box::new(base),
                step,
                negate,
            },
            Ty::Bool,
            span,
        )
        .map(Some)
    }

    /// A binary operator other than `&&` and `||` on bound operands.
    fn binary_bound(
        &mut self,
        op: BinaryOp,
        left: Bound<S>,
        right: Bound<S>,
        span: Span,
    ) -> BindResult<S> {
        let comparison = matches!(
            op,
            BinaryOp::Eq | BinaryOp::Ne | BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge
        );
        if !comparison && (self.is_dynamic(&left) || self.is_dynamic(&right)) {
            return self.dynamic_arithmetic(op, left, right, span);
        }

        match op {
            BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Rem => {
                self.arithmetic(op, left, right, span)
            }
            BinaryOp::BitAnd | BinaryOp::BitOr | BinaryOp::BitXor => {
                let left = self.integer_value(left)?;
                let right = self.integer_value(right)?;
                let ty = self.bitwise_type(&left.ty, &right.ty);
                let op = match op {
                    BinaryOp::BitAnd => BitOperator::And,
                    BinaryOp::BitOr => BitOperator::Or,
                    _ => BitOperator::Xor,
                };

                self.node(
                    Op::Bitwise {
                        op,
                        left: Box::new(left),
                        right: Box::new(right),
                    },
                    ty,
                    span,
                )
            }
            BinaryOp::Shl | BinaryOp::Shr => {
                let value = self.integer_value(left)?;
                let amount = self.integer_value(right)?;
                let ty = value.ty.clone();
                self.node(
                    Op::Shift {
                        left: op == BinaryOp::Shl,
                        value: Box::new(value),
                        amount: Box::new(amount),
                    },
                    ty,
                    span,
                )
            }
            _ => self.comparison(op, left, right, span),
        }
    }

    /// The type a bit operation keeps: the wider typed operand's, the
    /// unsigned one's at equal widths, the left's when both agree.
    fn bitwise_type(&self, left: &Ty, right: &Ty) -> Ty {
        let int = |ty: &Ty| match self.category(ty) {
            Category::Integer { int, .. } => int,
            _ => None,
        };

        match (int(left), int(right)) {
            (None, None) => Ty::Exact,
            (Some(_), None) => left.clone(),
            (None, Some(_)) => right.clone(),
            (Some(left_int), Some(right_int)) => {
                let common = left_int.common(right_int);
                if common == left_int {
                    left.clone()
                } else if common == right_int {
                    right.clone()
                } else {
                    Ty::Int(common)
                }
            }
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one arm per pair of operand categories"
    )]
    fn arithmetic(
        &mut self,
        op: BinaryOp,
        left: Bound<S>,
        right: Bound<S>,
        span: Span,
    ) -> BindResult<S> {
        let left = self.operand(left)?;
        let right = self.operand(right)?;
        let (left_category, right_category) = (self.category(&left.ty), self.category(&right.ty));
        match (&left_category, &right_category) {
            (Category::Integer { .. }, Category::Integer { .. }) => self.node(
                Op::Arithmetic {
                    op,
                    left: Box::new(left),
                    right: Box::new(right),
                },
                Ty::Exact,
                span,
            ),
            (
                Category::Float(_) | Category::Integer { .. },
                Category::Float(_) | Category::Integer { .. },
            ) => {
                let format = [&left_category, &right_category]
                    .into_iter()
                    .filter_map(|category| match category {
                        Category::Float(format) => Some(*format),
                        _ => None,
                    })
                    .max()
                    .ok_or_else(|| Self::broken(span, "one operand is a float"))?;
                let left = self.float_operand(left, format)?;
                let right = self.float_operand(right, format)?;
                let op = match op {
                    BinaryOp::Add => FloatOperator::Add,
                    BinaryOp::Sub => FloatOperator::Sub,
                    BinaryOp::Mul => FloatOperator::Mul,
                    BinaryOp::Div => FloatOperator::Div,
                    _ => FloatOperator::Rem,
                };

                self.node(
                    Op::FloatArithmetic {
                        op,
                        left: Box::new(left),
                        right: Box::new(right),
                    },
                    Ty::Float(format),
                    span,
                )
            }

            (Category::Pointer(pointee), Category::Integer { .. })
                if matches!(op, BinaryOp::Add | BinaryOp::Sub) =>
            {
                let scale = self.element_size(&left, pointee.as_ref())?;
                let ty = left.ty.clone();
                self.node(
                    Op::Offset {
                        pointer: Box::new(left),
                        count: Box::new(right),
                        scale,
                        backward: op == BinaryOp::Sub,
                    },
                    ty,
                    span,
                )
            }
            (Category::Integer { .. }, Category::Pointer(pointee)) if op == BinaryOp::Add => {
                let scale = self.element_size(&right, pointee.as_ref())?;
                let ty = right.ty.clone();
                self.node(
                    Op::Offset {
                        pointer: Box::new(right),
                        count: Box::new(left),
                        scale,
                        backward: false,
                    },
                    ty,
                    span,
                )
            }

            (Category::Pointer(left_pointee), Category::Pointer(right_pointee))
                if op == BinaryOp::Sub =>
            {
                let scale = self.element_size(&left, left_pointee.as_ref())?;
                let right_scale = self.element_size(&right, right_pointee.as_ref())?;
                if scale != right_scale {
                    return Err(Self::error(
                        span,
                        ErrorKind::Type,
                        "the pointers point at types of different sizes",
                    ));
                }

                if scale == 0 {
                    return Err(Self::error(
                        span,
                        ErrorKind::Type,
                        "the pointers point at a zero-sized type, whose elements share one address",
                    ));
                }

                self.node(
                    Op::Difference {
                        left: Box::new(left),
                        right: Box::new(right),
                        scale,
                    },
                    Ty::Exact,
                    span,
                )
            }
            (Category::Bool, _) | (_, Category::Bool) => {
                let operand = if matches!(left_category, Category::Bool) {
                    &left
                } else {
                    &right
                };

                Err(Self::error(
                    operand.span,
                    ErrorKind::Type,
                    format!(
                        "`{}` is a truth value, not a number",
                        self.quote(operand.span)
                    ),
                )
                .with_hint("convert it with `as u8`"))
            }

            (category, _)
                if !matches!(
                    category,
                    Category::Integer { .. } | Category::Float(_) | Category::Pointer(_)
                ) =>
            {
                Err(self.type_error(&left, category, "is not a number"))
            }

            (_, category)
                if !matches!(
                    category,
                    Category::Integer { .. } | Category::Float(_) | Category::Pointer(_)
                ) =>
            {
                Err(self.type_error(&right, category, "is not a number"))
            }
            _ => Err(Self::error(
                span,
                ErrorKind::Type,
                format!(
                    "`{}` cannot be applied to `{}` and `{}`",
                    super::syntax::binary_text(op),
                    type_name(self.scope, &left.ty),
                    type_name(self.scope, &right.ty)
                ),
            )),
        }
    }

    /// The size of the elements a pointer steps over, zero for a zero-sized
    /// type, whose elements share one address.
    fn element_size(
        &self,
        pointer: &Bound<S>,
        target: Option<&Ty>,
    ) -> Result<u64, ExpressionError> {
        let Some(target) = target else {
            return Err(Self::error(
                pointer.span,
                ErrorKind::Type,
                format!(
                    "`{}` points at void, which has no size",
                    self.quote(pointer.span)
                ),
            )
            .with_hint("cast it to `u8*` to count bytes"));
        };

        size_of(self.scope, target).ok_or_else(|| {
            Self::error(
                pointer.span,
                ErrorKind::Type,
                format!(
                    "`{}` points at a type without a size",
                    self.quote(pointer.span)
                ),
            )
        })
    }

    fn float_operand(&mut self, node: Bound<S>, format: FloatFormat) -> BindResult<S> {
        let span = node.span;
        let conversion = match self.category(&node.ty) {
            Category::Float(own) if own == format => return Ok(node),
            Category::Float(_) => Conversion::FloatToFloat(format),
            _ => Conversion::IntegerToFloat(format),
        };

        self.node(
            Op::Convert {
                operand: Box::new(node),
                conversion,
            },
            Ty::Float(format),
            span,
        )
    }

    fn comparison(
        &mut self,
        op: BinaryOp,
        left: Bound<S>,
        right: Bound<S>,
        span: Span,
    ) -> BindResult<S> {
        let ordering = !matches!(op, BinaryOp::Eq | BinaryOp::Ne);
        let left = self.settle(left)?;
        let right = self.settle(right)?;
        if self.is_dynamic(&left) || self.is_dynamic(&right) {
            let left = self.dynamic_operand(left, true)?;
            let right = self.dynamic_operand(right, true)?;
            return self.node(
                Op::Compare {
                    op,
                    how: Comparison::Dynamic,
                    left: Box::new(left),
                    right: Box::new(right),
                },
                Ty::Bool,
                span,
            );
        }

        // Text compares with a string literal, equality only.
        if matches!(left.ty, Ty::Text) || matches!(right.ty, Ty::Text) {
            let (text, other) = if matches!(left.ty, Ty::Text) {
                (left, right)
            } else {
                (right, left)
            };

            if !ordering && matches!(other.ty, Ty::Text) {
                return self.node(
                    Op::Compare {
                        op,
                        how: Comparison::Texts,
                        left: Box::new(other),
                        right: Box::new(text),
                    },
                    Ty::Bool,
                    span,
                );
            }

            if ordering || !other.is_place() {
                return Err(Self::error(
                    span,
                    ErrorKind::Type,
                    "a string only compares, by `==` or `!=`, with text the program holds",
                ));
            }

            return self.node(
                Op::Compare {
                    op,
                    how: Comparison::Text,
                    left: Box::new(other),
                    right: Box::new(text),
                },
                Ty::Bool,
                span,
            );
        }

        let left = self.operand(left)?;
        let right = self.operand(right)?;
        let how = match (self.category(&left.ty), self.category(&right.ty)) {
            (Category::Integer { .. }, Category::Integer { .. }) => Comparison::Integers,
            (Category::Float(_), Category::Float(_)) => Comparison::Floats,
            (Category::Float(_), Category::Integer { .. }) => Comparison::FloatInteger,
            (Category::Integer { .. }, Category::Float(_)) => Comparison::IntegerFloat,
            (Category::Pointer(_) | Category::Null, Category::Pointer(_) | Category::Null) => {
                Comparison::Addresses
            }
            (Category::Pointer(_), Category::Integer { .. }) if Self::is_zero(&right) => {
                Comparison::Addresses
            }
            (Category::Integer { .. }, Category::Pointer(_)) if Self::is_zero(&left) => {
                Comparison::Addresses
            }
            (Category::Bool, Category::Bool) if !ordering => Comparison::Bools,
            (left_category, right_category) => {
                for (node, category) in [(&left, &left_category), (&right, &right_category)] {
                    if let Category::Opaque(reason) = category {
                        return Err(self.unsupported(node.span, reason));
                    }
                }

                return Err(Self::error(
                    span,
                    ErrorKind::Type,
                    format!(
                        "`{}` cannot compare `{}` with `{}`",
                        super::syntax::binary_text(op),
                        type_name(self.scope, &left.ty),
                        type_name(self.scope, &right.ty)
                    ),
                ));
            }
        };

        self.node(
            Op::Compare {
                op,
                how,
                left: Box::new(left),
                right: Box::new(right),
            },
            Ty::Bool,
            span,
        )
    }

    fn is_zero(node: &Bound<S>) -> bool {
        matches!(&node.op, Op::Constant(Constant::Integer(integer)) if integer.value().is_zero())
    }

    fn conditional(
        &mut self,
        condition: NodeId,
        then: NodeId,
        otherwise: NodeId,
        span: Span,
    ) -> BindResult<S> {
        let condition = self.bind(condition)?;
        let condition = self.truth(condition)?;
        let (then, otherwise) = self.operands(then, otherwise)?;
        let then = self.settle(then)?;
        let otherwise = self.settle(otherwise)?;
        if then.is_place() && otherwise.is_place() && then.ty == otherwise.ty {
            let ty = then.ty.clone();
            return self.node(
                Op::Choose {
                    condition: Box::new(condition),
                    then: Box::new(then),
                    otherwise: Box::new(otherwise),
                    places: true,
                },
                ty,
                span,
            );
        }

        let then = self.operand(then)?;
        let otherwise = self.operand(otherwise)?;
        let (then_category, otherwise_category) =
            (self.category(&then.ty), self.category(&otherwise.ty));
        let (then, otherwise, ty) = if then.ty == otherwise.ty {
            let ty = then.ty.clone();
            (then, otherwise, ty)
        } else {
            match (then_category, otherwise_category) {
                (Category::Integer { .. }, Category::Integer { .. }) => {
                    (then, otherwise, Ty::Exact)
                }

                (
                    Category::Float(_) | Category::Integer { .. },
                    Category::Float(_) | Category::Integer { .. },
                ) => {
                    let format = [&then.ty, &otherwise.ty]
                        .into_iter()
                        .filter_map(|ty| match ty {
                            Ty::Float(format) => Some(*format),
                            _ => match self.category(ty) {
                                Category::Float(format) => Some(format),
                                _ => None,
                            },
                        })
                        .max()
                        .ok_or_else(|| Self::broken(span, "one branch is a float"))?;
                    let then = self.float_operand(then, format)?;
                    let otherwise = self.float_operand(otherwise, format)?;
                    (then, otherwise, Ty::Float(format))
                }
                (Category::Pointer(_), Category::Null) => {
                    let ty = then.ty.clone();
                    (then, otherwise, ty)
                }
                (Category::Null, Category::Pointer(_)) => {
                    let ty = otherwise.ty.clone();
                    (then, otherwise, ty)
                }
                _ => {
                    return Err(Self::error(
                        span,
                        ErrorKind::Type,
                        format!(
                            "the branches have different types, `{}` and `{}`",
                            type_name(self.scope, &then.ty),
                            type_name(self.scope, &otherwise.ty)
                        ),
                    ));
                }
            }
        };

        self.node(
            Op::Choose {
                condition: Box::new(condition),
                then: Box::new(then),
                otherwise: Box::new(otherwise),
                places: false,
            },
            ty,
            span,
        )
    }

    // ---- Assignment ----

    /// `target = value`, or `target op= value`, which must be the whole
    /// expression.
    fn assignment(
        &mut self,
        op: Option<BinaryOp>,
        target: NodeId,
        value: NodeId,
        span: Span,
    ) -> BindResult<S> {
        let target = self.bind(target)?;
        if matches!(target.op, Op::Register(_)) {
            return self.register_assignment(op, target, value, span);
        }

        let target = self.settle(target)?;
        if matches!(target.ty, Ty::Dynamic) {
            return Err(Self::error(
                target.span,
                ErrorKind::Unsupported,
                format!(
                    "`{}` has a dynamic type, whose values microscope does not change",
                    self.quote(target.span)
                ),
            ));
        }

        if !target.is_place() || !matches!(target.ty, Ty::Program(_)) {
            return Err(Self::error(
                target.span,
                ErrorKind::NotAnLvalue,
                format!(
                    "`{}` is a computed value, which cannot be assigned",
                    self.quote(target.span)
                ),
            ));
        }

        let category = self.category(&target.ty);
        if !matches!(
            category,
            Category::Integer { .. } | Category::Float(_) | Category::Bool | Category::Pointer(_)
        ) {
            return Err(self.type_error(
                &target,
                &category,
                "cannot be assigned; only numbers, truth values, and pointers can",
            ));
        }

        let value = self.bind_beside(value, Some(&target))?;
        let value = match op {
            None => value,
            Some(op) => self.binary_bound(op, target.clone(), value, span)?,
        };

        let value = self.operand(value)?;
        let source = self.category(&value.ty);
        let compatible = match category {
            Category::Integer { .. } => {
                matches!(
                    source,
                    Category::Integer { .. } | Category::Float(_) | Category::Bool
                )
            }
            Category::Float(_) => matches!(source, Category::Integer { .. } | Category::Float(_)),
            Category::Bool => matches!(source, Category::Bool | Category::Integer { .. }),
            _ => matches!(
                source,
                Category::Pointer(_) | Category::Null | Category::Integer { .. }
            ),
        };

        if !compatible {
            return Err(Self::error(
                value.span,
                ErrorKind::Type,
                format!(
                    "`{}` cannot be assigned to `{}`",
                    type_name(self.scope, &value.ty),
                    type_name(self.scope, &target.ty)
                ),
            ));
        }

        let (ty, value_span) = (target.ty.clone(), value.span);
        let fitted = self.node(Op::Fit(Box::new(value)), ty.clone(), value_span)?;
        self.node(
            Op::Assign {
                target: Box::new(target),
                value: Box::new(fitted),
            },
            ty,
            span,
        )
    }

    /// `$register = value`: a number the register's width holds, or a
    /// pointer.
    fn register_assignment(
        &mut self,
        op: Option<BinaryOp>,
        target: Bound<S>,
        value: NodeId,
        span: Span,
    ) -> BindResult<S> {
        let value = self.bind(value)?;
        let value = match op {
            None => value,
            Some(op) => self.binary_bound(op, target.clone(), value, span)?,
        };

        let value = self.operand(value)?;
        if !matches!(
            self.category(&value.ty),
            Category::Integer { .. }
                | Category::Float(_)
                | Category::Bool
                | Category::Pointer(_)
                | Category::Null
        ) {
            return Err(Self::error(
                value.span,
                ErrorKind::Type,
                format!(
                    "`{}` cannot be assigned to a register; only numbers and pointers can",
                    type_name(self.scope, &value.ty)
                ),
            ));
        }

        let (ty, value_span) = (target.ty.clone(), value.span);
        let fitted = self.node(Op::Fit(Box::new(value)), ty.clone(), value_span)?;
        self.node(
            Op::Assign {
                target: Box::new(target),
                value: Box::new(fitted),
            },
            ty,
            span,
        )
    }

    // ---- Conversions ----

    /// A record in memory cast to one of its base classes: that base's part
    /// of it, as C++'s `static_cast` selects it. `None` when the target is
    /// not one of its bases.
    fn upcast(
        &mut self,
        operand: &Bound<S>,
        target: &Ty,
        target_category: &Category,
        span: Span,
    ) -> Result<Option<Bound<S>>, ExpressionError> {
        let (Ty::Program(from), Ty::Program(base), Category::Record) =
            (&operand.ty, target, target_category)
        else {
            return Ok(None);
        };

        if !operand.is_place() || !matches!(self.category(&operand.ty), Category::Record) {
            return Ok(None);
        }

        match self.scope.plan(*from, StepKind::Base(*base)) {
            Ok(planned) => {
                let ty = Self::reached(planned.result, span, || {
                    format!(
                        "`{}` has a type the debugger cannot compute with",
                        self.quote(span)
                    )
                })?;
                self.node(
                    Op::Step {
                        base: Box::new(operand.clone()),
                        step: planned.step,
                        indices: Vec::new(),
                        follows: false,
                    },
                    ty,
                    span,
                )
                .map(Some)
            }
            Err(refusal) if refusal.kind == ErrorKind::AmbiguousName => {
                Err(Self::refused(span, refusal))
            }
            Err(_) => Ok(None),
        }
    }

    fn convert(&mut self, operand: Bound<S>, target: Ty, span: Span) -> BindResult<S> {
        let operand = self.settle(operand)?;
        if operand.ty == target {
            let mut operand = operand;
            operand.span = span;
            return Ok(operand);
        }

        let target_category = self.category(&target);
        if let Some(base) = self.upcast(&operand, &target, &target_category, span)? {
            return Ok(base);
        }

        let operand = match target_category {
            Category::Pointer(_) | Category::Integer { .. } | Category::Bool => {
                self.operand(operand)?
            }
            _ => self.value(operand)?,
        };

        let source = self.category(&operand.ty);
        let conversion = match (&source, &target_category) {
            (_, Category::Bool) => {
                return self.truth(operand).map(|mut node| {
                    node.span = span;
                    node
                });
            }
            (Category::Integer { .. }, Category::Integer { int: Some(int), .. }) => {
                Conversion::Truncate(*int)
            }
            (Category::Float(_), Category::Integer { int: Some(int), .. }) => {
                Conversion::FloatToInteger(*int)
            }
            (Category::Bool, Category::Integer { int: Some(int), .. }) => {
                Conversion::BoolToInteger(*int)
            }
            (Category::Pointer(_) | Category::Null, Category::Integer { int: Some(int), .. }) => {
                Conversion::PointerToInteger(*int)
            }
            (Category::Integer { .. }, Category::Float(format)) => {
                Conversion::IntegerToFloat(*format)
            }
            (Category::Float(_), Category::Float(format)) => Conversion::FloatToFloat(*format),
            (Category::Integer { .. }, Category::Pointer(_)) => Conversion::IntegerToPointer,
            (Category::Pointer(_) | Category::Null, Category::Pointer(_)) => {
                Conversion::PointerToPointer
            }
            (category @ Category::Opaque(_), _) => {
                return Err(self.type_error(&operand, category, "cannot be converted"));
            }
            (_, Category::Opaque(reason)) => {
                return Err(Self::error(
                    span,
                    ErrorKind::Unsupported,
                    format!(
                        "`{}` cannot be computed with: {reason}",
                        type_name(self.scope, &target)
                    ),
                ));
            }
            (from, to) if aggregate(from) || aggregate(to) => {
                return Err(Self::error(
                    span,
                    ErrorKind::Type,
                    format!(
                        "`{}` cannot be converted to `{}` by value",
                        type_name(self.scope, &operand.ty),
                        type_name(self.scope, &target)
                    ),
                )
                .with_hint("convert a pointer to it instead"));
            }
            _ => {
                return Err(Self::error(
                    span,
                    ErrorKind::Type,
                    format!(
                        "`{}` cannot be converted to `{}`",
                        type_name(self.scope, &operand.ty),
                        type_name(self.scope, &target)
                    ),
                ));
            }
        };

        self.node(
            Op::Convert {
                operand: Box::new(operand),
                conversion,
            },
            target,
            span,
        )
    }

    // ---- Members, indices, lengths ----

    /// Whether a pointer only stands for a container a view presents,
    /// which nothing indexes as a pointer.
    fn represents(&self, ty: &Ty) -> bool {
        matches!(ty, Ty::Program(reference) if self.scope.stands_for_container(*reference))
    }

    fn member(
        &mut self,
        base: Bound<S>,
        field: &Field,
        field_span: Span,
        arrow: bool,
        span: Span,
    ) -> BindResult<S> {
        let base = self.settle(base)?;
        if self.is_dynamic(&base) {
            return self.dynamic_member(base, field, field_span, arrow, span);
        }

        let base = match self.category(&base.ty) {
            Category::Pointer(_) => self.deref(base, span)?,
            _ if arrow => {
                let category = self.category(&base.ty);
                return Err(self.type_error(&base, &category, "is not a pointer"));
            }
            _ => base,
        };

        let base = self.settle(base)?;
        let category = self.category(&base.ty);
        if !matches!(category, Category::Record) {
            let what = if matches!(category, Category::Pointer(_)) {
                "is a pointer to a pointer; `.` and `->` follow one pointer"
            } else {
                "has no members"
            };

            return Err(self.type_error(&base, &category, what));
        }

        let Ty::Program(from) = base.ty else {
            return Err(Self::broken(base.span, "records are program types"));
        };

        if !base.is_place() {
            return Err(Self::error(
                base.span,
                ErrorKind::Type,
                format!(
                    "`{}` is a computed value, whose members cannot be selected",
                    self.quote(base.span)
                ),
            ));
        }

        let names = match field {
            Field::Named(name) => vec![name.clone()],
            Field::Index(index) => vec![format!("__{index}"), index.to_string()],
        };

        let mut planned = Err(Refusal::new(ErrorKind::Type, String::new()));
        for name in &names {
            planned = self.scope.plan(from, StepKind::Member(name));
            if planned.is_ok() {
                break;
            }
        }

        let planned = planned.map_err(|refusal| Self::refused(field_span, refusal))?;
        let ty = Self::reached(planned.result, field_span, || {
            format!(
                "`{}` has a type the debugger cannot compute with",
                self.quote(span)
            )
        })?;
        self.node(
            Op::Step {
                base: Box::new(base),
                step: planned.step,
                indices: Vec::new(),
                follows: false,
            },
            ty,
            span,
        )
    }

    /// `a[i][j]...`: an array takes as many consecutive indices as it has
    /// dimensions, and a record any its view gives it.
    fn index(&mut self, id: NodeId) -> BindResult<S> {
        let mut chain = Vec::new();
        let mut current = id;
        while let NodeKind::Index { base, index } = self.tree().kind(current) {
            chain.push((*index, self.tree().span(current)));
            current = *base;
        }

        chain.reverse();
        let mut node = self.bind(current)?;
        let mut pending = chain.as_slice();
        while let [(first, span), ..] = pending {
            node = self.settle(node)?;
            let category = self.category(&node.ty);
            match category {
                // A value of a dynamic type takes a key or an index, as its
                // type when it runs says.
                Category::Dynamic if node.is_place() => {
                    node = self.dynamic_index(node, *first, *span)?;
                    pending = &pending[1..];
                }
                Category::Pointer(target) if !self.represents(&node.ty) => {
                    let pointer = self.value(node)?;
                    let scale = self.element_size(&pointer, target.as_ref())?;
                    let index = self.bind(*first)?;
                    let index = self.integer_value(index)?;
                    let ty = pointer.ty.clone();
                    let moved = self.node(
                        Op::Offset {
                            pointer: Box::new(pointer),
                            count: Box::new(index),
                            scale,
                            backward: false,
                        },
                        ty,
                        *span,
                    )?;
                    let target = target.ok_or_else(|| Self::broken(*span, "a sized pointee"))?;
                    node = self.deref_value(moved, &target, *span)?;
                    pending = &pending[1..];
                }

                // A record has no indexing of its own, nor has a value its
                // language represents as a pointer; a view that presents it
                // as a sequence may give it some, one index at a time.
                ref category @ (Category::Array { .. }
                | Category::RuntimeArray
                | Category::Slice(_)
                | Category::Record
                | Category::Pointer(_))
                    if node.is_place() =>
                {
                    let Ty::Program(from) = node.ty else {
                        return Err(Self::broken(*span, "indexed places are program types"));
                    };

                    let native = matches!(
                        category,
                        Category::Array { .. } | Category::RuntimeArray | Category::Slice(_)
                    );

                    // A value a view presents as a map is indexed by key.
                    if !native && let Ok(planned) = self.scope.plan(from, StepKind::Entry) {
                        node = self.entry(node, planned, *first, *span)?;
                        pending = &pending[1..];
                        continue;
                    }

                    let available = pending.len();
                    let planned = self
                        .scope
                        .plan(from, StepKind::Index { available })
                        .map_err(|refusal| {
                            if native {
                                Self::refused(*span, refusal)
                            } else {
                                self.type_error(&node, category, "cannot be indexed")
                            }
                        })?;
                    let consumed = if native {
                        planned.consumed.clamp(1, available)
                    } else {
                        1
                    };

                    let (taken, rest) = pending.split_at(consumed);
                    let mut indices = Vec::new();
                    for (index, _) in taken {
                        let index = self.bind(*index)?;
                        indices.push(self.integer_value(index)?);
                    }

                    let span = taken.last().map_or(*span, |(_, span)| *span);
                    let ty = Self::reached(planned.result, span, || {
                        "the element has a type the debugger cannot compute with".to_owned()
                    })?;
                    node = self.node(
                        Op::Step {
                            base: Box::new(node),
                            step: planned.step,
                            indices,
                            follows: !matches!(category, Category::Array { .. }),
                        },
                        ty,
                        span,
                    )?;
                    pending = rest;
                }
                category => return Err(self.type_error(&node, &category, "cannot be indexed")),
            }
        }

        Ok(node)
    }

    fn measured_as_text(&self, category: &Category, ty: &Ty) -> bool {
        match category {
            Category::Pointer(Some(pointee)) if is_character(self.scope, pointee) => true,
            Category::Pointer(_) => matches!(ty, Ty::Program(ty) if self.scope.has_view(*ty)),
            _ => true,
        }
    }

    fn length(&mut self, operand: Bound<S>, span: Span) -> BindResult<S> {
        let operand = self.settle(operand)?;
        let category = self.category(&operand.ty);
        let constant =
            |count: u64| Constant::Integer(Integer::Exact(Exact::from(u128::from(count))));
        match category {
            Category::Array { dimensions, .. } => {
                let count = dimensions.first().map_or(0, |dimension| dimension.count);
                self.node(Op::Constant(constant(count)), Ty::Exact, span)
            }
            Category::Slice(_) | Category::RuntimeArray if operand.is_place() => self.node(
                Op::Length {
                    operand: Box::new(operand),
                    how: Length::Slice,
                },
                Ty::Exact,
                span,
            ),
            // Text, or a pointer or record a view presents with a length,
            // such as a Go map.
            Category::Pointer(_) | Category::Record
                if operand.is_place() && self.measured_as_text(&category, &operand.ty) =>
            {
                self.node(
                    Op::Length {
                        operand: Box::new(operand),
                        how: Length::Text,
                    },
                    Ty::Exact,
                    span,
                )
            }
            Category::Text => self.node(
                Op::Length {
                    operand: Box::new(operand),
                    how: Length::Bytes,
                },
                Ty::Exact,
                span,
            ),
            Category::Dynamic if operand.is_place() => self.node(
                Op::Length {
                    operand: Box::new(operand),
                    how: Length::Dynamic,
                },
                Ty::Exact,
                span,
            ),
            category => Err(self.type_error(&operand, &category, "has no length")),
        }
    }

    /// `cap(x)`: how many elements `x` has room for. An array has room for
    /// its elements, a slice for what its descriptor records, and anything
    /// else for what the `capacity` field of its view says.
    fn capacity(&mut self, operand: Bound<S>, span: Span) -> BindResult<S> {
        let operand = self.settle(operand)?;
        let category = self.category(&operand.ty);
        match category {
            Category::Array { dimensions, .. } => {
                let count = dimensions.first().map_or(0, |dimension| dimension.count);
                self.node(
                    Op::Constant(Constant::Integer(Integer::Exact(Exact::from(u128::from(
                        count,
                    ))))),
                    Ty::Exact,
                    span,
                )
            }
            Category::Slice(_) if operand.is_place() => {
                if !self.records_capacity(&operand.ty) {
                    return Err(self.type_error(&operand, &category, "records no capacity"));
                }

                self.node(
                    Op::Capacity {
                        operand: Box::new(operand),
                        how: Capacity::Slice,
                    },
                    Ty::Exact,
                    span,
                )
            }

            // A record, or a pointer that stands for a container, may have
            // a view that gives it a capacity.
            Category::Record if operand.is_place() => self.node(
                Op::Capacity {
                    operand: Box::new(operand),
                    how: Capacity::Presented,
                },
                Ty::Exact,
                span,
            ),
            Category::Pointer(_) if operand.is_place() && self.represents(&operand.ty) => self
                .node(
                    Op::Capacity {
                        operand: Box::new(operand),
                        how: Capacity::Presented,
                    },
                    Ty::Exact,
                    span,
                ),
            category => Err(self.type_error(&operand, &category, "has no capacity")),
        }
    }

    /// Whether a slice type's descriptor records a capacity.
    fn records_capacity(&self, ty: &Ty) -> bool {
        let Ty::Program(reference) = ty else {
            return false;
        };

        representation(self.scope, *reference).is_ok_and(|(_, info)| {
            matches!(info.kind, TypeKind::Slice { words, .. } if words.capacity.is_some())
        })
    }

    /// Whether a slice type's elements are its language's text, as Rust's
    /// `str` and Zig's `[]const u8` are.
    fn is_text_slice(&self, ty: &Ty) -> bool {
        let Ty::Program(reference) = ty else {
            return false;
        };

        representation(self.scope, *reference)
            .is_ok_and(|(_, info)| matches!(info.kind, TypeKind::Slice { text: true, .. }))
    }

    /// Whether a type is an array, or a slice of anything but text.
    fn has_elements(&self, ty: &Ty) -> bool {
        match self.category(ty) {
            Category::Array { .. } | Category::RuntimeArray => true,
            Category::Slice(_) => !self.is_text_slice(ty),
            _ => false,
        }
    }

    /// An index or bound, as an integer.
    fn bound_index(&mut self, id: NodeId) -> BindResult<S> {
        let node = self.bind(id)?;
        self.integer_value(node)
    }

    /// `base[start:end]`: a range of an array's or slice's elements, which
    /// is only ever the whole expression, or the bytes of text between the
    /// bounds, as a string.
    fn slice(
        &mut self,
        id: NodeId,
        base: NodeId,
        start: Option<NodeId>,
        end: Option<NodeId>,
        span: Span,
    ) -> BindResult<S> {
        let base = self.bind(base)?;
        let base = self.settle(base)?;
        let start = start.map(|start| self.bound_index(start)).transpose()?;
        let end = end.map(|end| self.bound_index(end)).transpose()?;
        let (start, end) = (start.map(Box::new), end.map(Box::new));
        if base.is_place() && self.has_elements(&base.ty) {
            if id != self.tree().root() {
                return Err(Self::error(
                    span,
                    ErrorKind::Type,
                    "a slice of an array or slice is a range of its elements, which must be \
                     the whole expression",
                ));
            }

            let ty = base.ty.clone();
            return self.node(
                Op::Range {
                    base: Box::new(base),
                    start,
                    end,
                },
                ty,
                span,
            );
        }

        let category = self.category(&base.ty);
        let text = match &category {
            Category::Text | Category::Record | Category::Slice(_) => true,
            Category::Pointer(Some(pointee)) => {
                is_character(self.scope, pointee) && !self.represents(&base.ty)
            }
            _ => false,
        };

        if !text || !(base.is_place() || matches!(category, Category::Text)) {
            return Err(self.type_error(&base, &category, "is not an array, slice, or text"));
        }

        self.node(
            Op::TextSlice {
                base: Box::new(base),
                start,
                end,
            },
            Ty::Text,
            span,
        )
    }

    /// `m[key]`: the value a map holds for a key.
    fn entry(
        &mut self,
        map: Bound<S>,
        planned: super::target::Planned<S::Step>,
        key: NodeId,
        span: Span,
    ) -> BindResult<S> {
        let ty = Self::reached(planned.result, span, || {
            "the map's values have a type the debugger cannot compute with".to_owned()
        })?;
        let key = self.bind(key)?;
        let key = self.key(key)?;
        self.node(
            Op::Entry {
                base: Box::new(map),
                step: planned.step,
                key: Box::new(key),
            },
            ty,
            span,
        )
    }

    /// A map's key: a number, truth value, pointer, or string, or text the
    /// program holds.
    fn key(&mut self, node: Bound<S>) -> BindResult<S> {
        let node = self.value(node)?;
        match self.category(&node.ty) {
            Category::Integer { .. }
            | Category::Float(_)
            | Category::Bool
            | Category::Pointer(_)
            | Category::Null
            | Category::Text => Ok(node),
            Category::Record | Category::Slice(_) if node.is_place() => Ok(node),
            Category::Dynamic => self.dynamic_value(node),
            category => Err(self.type_error(
                &node,
                &category,
                "is no key: a key is a number, truth value, pointer, or string",
            )),
        }
    }

    // ---- Values of a dynamic type ----

    fn is_dynamic(&self, node: &Bound<S>) -> bool {
        matches!(self.category(&node.ty), Category::Dynamic)
    }

    /// The value of a node of a dynamic type, which the machine loads when
    /// it is a place.
    fn dynamic_value(&mut self, node: Bound<S>) -> BindResult<S> {
        if !node.is_place() {
            return Ok(node);
        }

        let span = node.span;
        self.node(Op::Load(Box::new(node)), Ty::Dynamic, span)
    }

    /// An operand of an operation on a value of a dynamic type: that value,
    /// or a number or truth value, or a string where `text` allows one.
    fn dynamic_operand(&mut self, node: Bound<S>, text: bool) -> BindResult<S> {
        let node = self.operand(node)?;
        match self.category(&node.ty) {
            Category::Dynamic => self.dynamic_value(node),
            Category::Integer { .. } | Category::Float(_) | Category::Bool => Ok(node),
            Category::Text if text => Ok(node),
            category => Err(self.type_error(
                &node,
                &category,
                if text {
                    "is not a number, truth value, or string"
                } else {
                    "is not a number"
                },
            )),
        }
    }

    /// Arithmetic or a bit operation of which an operand has a dynamic
    /// type, whose rules the operands' types when it runs choose.
    fn dynamic_arithmetic(
        &mut self,
        op: BinaryOp,
        left: Bound<S>,
        right: Bound<S>,
        span: Span,
    ) -> BindResult<S> {
        let left = self.dynamic_operand(left, false)?;
        let right = self.dynamic_operand(right, false)?;
        self.node(
            Op::DynamicArithmetic {
                op,
                left: Box::new(left),
                right: Box::new(right),
            },
            Ty::Dynamic,
            span,
        )
    }

    /// `x[key]` of a value of a dynamic type: the item its value has for
    /// the key or index when it runs.
    fn dynamic_index(&mut self, base: Bound<S>, key: NodeId, span: Span) -> BindResult<S> {
        let step = self
            .scope
            .plan_dynamic(StepKind::Entry)
            .map_err(|refusal| Self::refused(span, refusal))?;
        let key = self.bind(key)?;
        let key = self.key(key)?;
        self.node(
            Op::Entry {
                base: Box::new(base),
                step,
                key: Box::new(key),
            },
            Ty::Dynamic,
            span,
        )
    }

    /// `x.name` of a value of a dynamic type: the attribute its value has
    /// when it runs. An attribute belongs to a value, not to its type, so
    /// one a value lacks is refused only then.
    fn dynamic_member(
        &mut self,
        base: Bound<S>,
        field: &Field,
        field_span: Span,
        arrow: bool,
        span: Span,
    ) -> BindResult<S> {
        if arrow {
            return Err(Self::error(
                base.span,
                ErrorKind::Type,
                format!(
                    "`{}` has a dynamic type, which is no pointer; `.` reaches its attributes",
                    self.quote(base.span)
                ),
            ));
        }

        let Field::Named(name) = field else {
            return Err(Self::error(
                field_span,
                ErrorKind::Type,
                format!(
                    "`{}` has attributes by name, not by number",
                    self.quote(base.span)
                ),
            ));
        };

        if !base.is_place() {
            return Err(Self::error(
                base.span,
                ErrorKind::Type,
                format!(
                    "`{}` is a computed value, whose members cannot be selected",
                    self.quote(base.span)
                ),
            ));
        }

        let step = self
            .scope
            .plan_dynamic(StepKind::Member(name))
            .map_err(|refusal| Self::refused(field_span, refusal))?;
        self.node(
            Op::Step {
                base: Box::new(base),
                step,
                indices: Vec::new(),
                follows: true,
            },
            Ty::Dynamic,
            span,
        )
    }
}

/// Whether `$name` reads the call that stopped the thread.
fn call_value(name: &str) -> bool {
    matches!(
        name,
        "arg0" | "arg1" | "arg2" | "arg3" | "arg4" | "arg5" | "ret" | "path"
    )
}
