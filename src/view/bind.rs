//! Binding a view against one concrete type, before anything runs: every
//! member path resolves, every type resolves through the type index, and
//! every expression type-checks. An `or` alternative that does not bind is
//! skipped; a view any part of which does not bind is rejected with its
//! reason, and the next candidate is tried.

use std::sync::Arc;

use crate::eval::bind::{Mode, bind as bind_expression, bind_condition, bind_value};
use crate::eval::error::ErrorKind;
use crate::eval::ir::Program;
use crate::eval::number::Exact;
use crate::eval::syntax::Expression;
use crate::eval::syntax::ast::{BinaryOp, NodeKind};
use crate::eval::target::{
    Lookup, Planned, Refusal, Register, Scope, StepKind, TypeLookup, TypeQuery,
};

use crate::eval::types::{Category, Ty, TypeSource, category, is_character, representation};
use crate::{BaseTypeEncoding, TypeArgument, TypeInfo, TypeKind, TypeReference};

use super::pattern::{Captured, Captures};
use super::syntax::{
    ArgumentPattern, Clause, Count, DynamicType, Expr, Format, Generator, InputValue, Item,
    Pattern, Piece, Shape, Statement, TypeExpr, View, Visualize,
};

/// Something a view's expression names, which its machine reaches at a
/// stop.
#[derive(Debug, Clone)]
pub enum ViewObject<St> {
    /// `self`, the value presented.
    This,

    /// A member of `self`, by its planned step.
    Member(St),

    /// A `let`, by position.
    Let(usize),

    /// A generator's variable, by nesting depth.
    Variable(usize),

    /// A global of the value's module, by a step that reaches it from
    /// anywhere.
    Global(St),
}

/// A program bound in a view's scope.
pub type ViewProgram<St> = Program<ViewObject<St>, St>;

/// A `check`, with its sides when it is a comparison, for saying why it
/// failed.
#[derive(Debug, Clone)]
pub struct BoundCheck<St> {
    pub text: String,
    pub program: ViewProgram<St>,
    pub sides: Option<[(String, ViewProgram<St>); 2]>,
}

/// A `field`.
#[derive(Debug, Clone)]
pub struct BoundField<St> {
    pub name: Arc<str>,
    pub program: ViewProgram<St>,
}

/// How a `format` writes a value, with the enumeration it names resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundFormat {
    Hex,
    Char,
    Bytes,
    Utf8,
    Utf16,
    Flags(TypeReference),
    Enum(TypeReference),
    Duration(super::syntax::TimeUnit),
    Time(super::syntax::TimeUnit),
}

/// A piece of a `summary`.
#[derive(Debug, Clone)]
pub enum BoundPiece<St> {
    Literal(String),

    /// A value's summary, or the value written in `format`.
    Hole {
        program: ViewProgram<St>,
        format: Option<BoundFormat>,
    },
}

/// Where a `text` shape's characters are, and how many bytes wide each
/// is: one, or two or four for UTF-16 and UTF-32.
#[derive(Debug, Clone)]
pub enum TextSource<St> {
    /// A pointer to characters.
    Pointer {
        program: ViewProgram<St>,
        width: usize,
    },

    /// An array or slice of characters, with the step to its first.
    Elements {
        program: ViewProgram<St>,
        first: St,
        width: usize,
    },
}

impl<St> TextSource<St> {
    /// How many bytes wide each character is.
    pub const fn width(&self) -> usize {
        match self {
            Self::Pointer { width, .. } | Self::Elements { width, .. } => *width,
        }
    }
}

/// A generator, bound: its programs see the variables of the clauses
/// around it, and a link's program also its node, at the clause's own
/// position.
#[derive(Debug, Clone)]
pub enum BoundGenerator<St> {
    Range(ViewProgram<St>),
    List {
        head: ViewProgram<St>,
        next: ViewProgram<St>,
    },
    Inorder {
        root: ViewProgram<St>,
        left: ViewProgram<St>,
        right: ViewProgram<St>,
    },

    /// A kernel's items, each as many words as the clause has variables.
    Kernel {
        kernel: Arc<super::kernel::Kernel>,
        arguments: Vec<ViewProgram<St>>,
        words: usize,
    },
}

/// What follows a clause's generator, bound.
#[derive(Debug, Clone)]
pub enum BoundItem<St> {
    Filter(ViewProgram<St>),

    /// A value computed once for each of the clause's values, at the next
    /// position.
    Let(ViewProgram<St>),
}

/// A clause, bound. Its variable is at one position, and its `let`s at the
/// positions after it.
#[derive(Debug, Clone)]
pub struct BoundClause<St> {
    pub generator: BoundGenerator<St>,
    pub items: Vec<BoundItem<St>>,
}

impl<St> BoundClause<St> {
    /// How many positions the clause's variables and `let`s take.
    pub fn width(&self) -> usize {
        let variables = match &self.generator {
            BoundGenerator::Kernel { words, .. } => *words,
            _ => 1,
        };

        variables
            + self
                .items
                .iter()
                .filter(|item| matches!(item, BoundItem::Let(_)))
                .count()
    }
}

/// The generators of a sequence or map, and its declared count.
#[derive(Debug, Clone)]
pub struct BoundScan<St> {
    /// `None` when the view leaves the count to the generators.
    pub count: Option<ViewProgram<St>>,
    pub clauses: Vec<BoundClause<St>>,
}

impl<St> BoundScan<St> {
    /// Whether element `k` is reached directly: one `range` and no filter.
    pub fn random_access(&self) -> bool {
        matches!(
            self.clauses.as_slice(),
            [BoundClause {
                generator: BoundGenerator::Range(_),
                items,
            }] if items.iter().all(|item| matches!(item, BoundItem::Let(_)))
        )
    }
}

/// A shape, bound.
#[derive(Debug, Clone)]
pub enum BoundShape<St> {
    Text {
        source: TextSource<St>,
        length: Option<ViewProgram<St>>,
    },
    Value(ViewProgram<St>),
    Empty(Vec<BoundPiece<St>>),
    Sequence {
        scan: BoundScan<St>,
        element: ViewProgram<St>,
    },
    Map {
        scan: BoundScan<St>,
        key: ViewProgram<St>,
        value: ViewProgram<St>,
    },
    If {
        condition: ViewProgram<St>,
        then: Box<Self>,
        otherwise: Box<Self>,
    },

    /// A record whose members are its children, before the view's fields.
    Record(Vec<BoundField<St>>),

    /// An interpreter's object as the debugger presents it without a view,
    /// lending its children, as a view of one that does not `show` presents
    /// it.
    Stored,

    /// What a pointer points to, as a type that may be chosen at run time.
    Dynamic {
        pointer: ViewProgram<St>,
        ty: BoundDynamic<St>,
    },

    /// A `match` whose arms name no value it has: a problem saying the
    /// value.
    Unmatched {
        text: String,
        program: ViewProgram<St>,
    },
}

/// The type a `dynamic` shape presents its pointer's target as.
#[derive(Debug, Clone)]
pub enum BoundDynamic<St> {
    Fixed(TypeReference),

    /// A type's arguments, of which `index` chooses one; a value argument
    /// is `None`.
    Argument {
        types: Arc<[Option<TypeReference>]>,
        index: ViewProgram<St>,
    },

    /// The one type among `candidates` that `pattern` names when its
    /// captures `known` does not name are the type arguments of the
    /// function whose code `code` addresses.
    Function {
        code: ViewProgram<St>,
        name: Arc<str>,

        /// Whether `name` is a parameter's, which names its argument.
        argument: bool,
        pattern: Arc<Pattern>,
        known: Captures,
        candidates: Arc<[TypeReference]>,
    },
}

impl<St> BoundShape<St> {
    fn any_branch(&self, test: fn(&Self) -> bool) -> bool {
        match self {
            Self::If {
                then, otherwise, ..
            } => then.any_branch(test) || otherwise.any_branch(test),
            shape => test(shape),
        }
    }

    /// Whether any branch presents text.
    pub fn has_text(&self) -> bool {
        self.any_branch(|shape| matches!(shape, Self::Text { .. }))
    }

    /// Whether any branch presents a sequence or map, whose elements or
    /// entries are children.
    pub fn has_elements(&self) -> bool {
        self.any_branch(|shape| matches!(shape, Self::Sequence { .. } | Self::Map { .. }))
    }
}

/// A `visualize`, bound: the renderer it calls and its inputs, or why it
/// does not bind, which leaves the rest of its view as it is.
#[derive(Debug, Clone)]
pub struct BoundVisualizer<St> {
    pub name: Arc<str>,
    pub line: u32,
    pub bound: Result<BoundDrawing<St>, Rejection>,
}

/// What a `visualize` that binds draws with.
#[derive(Debug, Clone)]
pub struct BoundDrawing<St> {
    pub renderer: Arc<crate::Renderer>,
    pub inputs: Vec<BoundInput<St>>,
}

/// An input of a `visualize`, bound.
#[derive(Debug, Clone)]
pub struct BoundInput<St> {
    pub name: Arc<str>,
    pub value: BoundInputValue<St>,

    /// The part of the presented value the input is, as a drawing's
    /// `select` names one: `""` for `self`, else members and indices.
    pub path: Option<Arc<str>>,
}

/// What an input hands the renderer, bound.
#[derive(Debug, Clone)]
pub enum BoundInputValue<St> {
    Value(ViewProgram<St>),

    /// `bytes(PTR, LEN)`: an address and a count of bytes.
    Bytes {
        pointer: ViewProgram<St>,
        length: ViewProgram<St>,
    },

    /// A string, as written.
    Text(Arc<str>),
}

/// A view bound against one concrete type.
#[derive(Debug, Clone)]
pub struct BoundView<St> {
    pub view: Arc<View>,

    /// Each `let`, computed at most once per presentation.
    pub lets: Vec<ViewProgram<St>>,
    pub checks: Vec<BoundCheck<St>>,
    pub fields: Vec<BoundField<St>>,
    pub summary: Option<Vec<BoundPiece<St>>>,
    pub shape: BoundShape<St>,

    /// The names of members and fields left out of the children, with
    /// the lines that hide them.
    pub hidden: Vec<(Arc<str>, u32)>,

    /// How members and fields are written, by name, with the lines that
    /// say so; a later one wins.
    pub formats: Vec<(Arc<str>, BoundFormat, u32)>,

    /// The bytes of `self`, which `format self as utf8` writes as the
    /// summary of a shape that does not present another value.
    pub self_text: Option<TextSource<St>>,

    /// The `extend`s that add to this view, each bound in a scope of its
    /// own, in the order they are tried.
    pub extensions: Vec<Arc<Self>>,

    /// Its `visualize` statements, in order, each bound or not.
    pub visualizers: Vec<BoundVisualizer<St>>,
}

impl<St> BoundView<St> {
    /// Every named child the view may show, by name: each record member of
    /// any branch of its shape, then its fields.
    pub fn named_programs(&self) -> Vec<(Arc<str>, &ViewProgram<St>)> {
        fn record_members<'b, St>(
            shape: &'b BoundShape<St>,
            out: &mut Vec<(Arc<str>, &'b ViewProgram<St>)>,
        ) {
            match shape {
                BoundShape::Record(members) => out.extend(
                    members
                        .iter()
                        .map(|member| (Arc::clone(&member.name), &member.program)),
                ),
                BoundShape::If {
                    then, otherwise, ..
                } => {
                    record_members(then, out);
                    record_members(otherwise, out);
                }
                _ => {}
            }
        }

        let mut named = Vec::new();
        record_members(&self.shape, &mut named);
        named.extend(
            self.fields
                .iter()
                .map(|field| (Arc::clone(&field.name), &field.program)),
        );
        named
    }
}

/// Why a view does not bind: the part that failed, and how.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    pub line: u32,

    /// The part of the view that failed, as written.
    pub part: String,
    pub reason: String,
}

impl std::fmt::Display for Rejection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "line {}: `{}`: {}",
            self.line, self.part, self.reason
        )
    }
}

/// What a view presents: values of one of the program's types, or an
/// interpreter's objects, whose types only their values know.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelfType {
    Program(TypeReference),

    /// An object of an interpreted language, as a Python view presents:
    /// `self` is a value of a dynamic type, and its attributes are written
    /// `self.name`, as the language writes them.
    Dynamic,
}

impl SelfType {
    /// The program type, for a view of one.
    #[must_use]
    pub const fn program(self) -> Option<TypeReference> {
        match self {
            Self::Program(reference) => Some(reference),
            Self::Dynamic => None,
        }
    }

    const fn ty(self) -> Ty {
        match self {
            Self::Program(reference) => Ty::Program(reference),
            Self::Dynamic => Ty::Dynamic,
        }
    }
}

/// The scope a view's expressions bind in: its generators' variables, its
/// `let`s, the values its pattern captured, `self`, and `self`'s members.
/// Types are its captures and `type`s, then the program's. Nothing the
/// frame names is visible, so a view means the same at every stop.
pub struct ViewScope<'a, S: Scope> {
    base: &'a S,
    self_type: SelfType,
    captures: &'a Captures,

    /// The set the view came from, whose kernels it calls before the
    /// built-in ones.
    set: &'a super::ViewSet,
    types: Vec<(String, TypeReference)>,
    lets: Vec<(String, Ty, bool)>,

    /// The generators' variables and clauses' `let`s in scope, by
    /// position, with their types and whether each is a place.
    variables: Vec<(String, Ty, bool)>,
}

impl<'a, S: Scope> ViewScope<'a, S> {
    pub const fn new(
        base: &'a S,
        self_type: SelfType,
        captures: &'a Captures,
        set: &'a super::ViewSet,
    ) -> Self {
        Self {
            base,
            self_type,
            captures,
            set,
            types: Vec::new(),
            lets: Vec::new(),
            variables: Vec::new(),
        }
    }

    fn self_name(&self) -> String {
        self.self_type
            .program()
            .and_then(|reference| self.base.type_info(reference))
            .map_or_else(|| "self".to_owned(), |info| info.name.to_string())
    }
}

impl<S: Scope> TypeSource for ViewScope<'_, S> {
    fn type_info(&self, ty: TypeReference) -> Option<&TypeInfo> {
        self.base.type_info(ty)
    }

    fn pointer_size(&self) -> u8 {
        self.base.pointer_size()
    }

    fn byte_order(&self) -> crate::ByteOrder {
        self.base.byte_order()
    }

    fn c_base_type(&self, ty: crate::CBaseType) -> Option<crate::BaseType> {
        self.base.c_base_type(ty)
    }

    fn same_type(&self, left: TypeReference, right: TypeReference) -> bool {
        self.base.same_type(left, right)
    }
}

impl<S: Scope> Scope for ViewScope<'_, S> {
    type Object = ViewObject<S::Step>;
    type Step = S::Step;

    fn lookup(&self, name: &str, outermost: bool) -> Result<Lookup<Self::Object>, Refusal> {
        if outermost {
            return Ok(Lookup::NotFound);
        }

        let named = |names: &[(String, Ty, bool)], object: fn(usize) -> Self::Object| {
            let index = names.iter().rposition(|(named, ..)| named == name)?;
            let (_, ty, place) = &names[index];
            Some(match (place, ty) {
                (true, Ty::Program(reference)) => Lookup::Object {
                    object: object(index),
                    ty: Ok(*reference),
                },
                _ => Lookup::Bound {
                    object: object(index),
                    ty: ty.clone(),
                },
            })
        };

        if let Some(found) = named(&self.variables, ViewObject::Variable)
            .or_else(|| named(&self.lets, ViewObject::Let))
        {
            return Ok(found);
        }

        match self.captures.iter().find(|(captured, _)| captured == name) {
            Some((_, Captured::Value(value))) => {
                return Ok(Lookup::Constant(Exact::from(*value)));
            }
            Some((_, Captured::Type(_))) => return Ok(Lookup::NotFound),
            None => {}
        }

        let self_type = match self.self_type {
            SelfType::Program(reference) => reference,
            SelfType::Dynamic if name == "self" => return Ok(Lookup::Dynamic(ViewObject::This)),
            // An object's attributes are named through `self`, so that
            // what an object happens to hold never shadows a name.
            SelfType::Dynamic => return Ok(Lookup::NotFound),
        };

        if name == "self" {
            return Ok(Lookup::Object {
                object: ViewObject::This,
                ty: Ok(self_type),
            });
        }

        match self.base.plan(self_type, StepKind::Member(name)) {
            Ok(Planned { step, result, .. }) => Ok(Lookup::Object {
                object: ViewObject::Member(step),
                ty: result.ok_or_else(|| {
                    Arc::from(format!(
                        "the member `{name}` has a type the debugger cannot compute with"
                    ))
                }),
            }),
            Err(refusal) if refusal.kind == ErrorKind::Type => Ok(Lookup::NotFound),
            Err(refusal) => Err(refusal),
        }
    }

    fn lookup_type(&self, query: &TypeQuery) -> TypeLookup {
        if query.tag.is_none() {
            if let Some((_, reference)) = self
                .types
                .iter()
                .rev()
                .find(|(name, _)| *name == query.name)
            {
                return TypeLookup::Found(*reference);
            }

            if let Some((_, Captured::Type(reference))) = self
                .captures
                .iter()
                .find(|(captured, _)| *captured == query.name)
            {
                return TypeLookup::Found(*reference);
            }
        }

        self.base.lookup_type(query)
    }

    fn plan(
        &self,
        from: TypeReference,
        step: StepKind<'_>,
    ) -> Result<Planned<Self::Step>, Refusal> {
        self.base.plan(from, step)
    }

    fn register(&self, _name: &str) -> Option<Register> {
        None
    }

    fn plan_dynamic(&self, step: StepKind<'_>) -> Result<Self::Step, Refusal> {
        self.base.plan_dynamic(step)
    }

    fn types_with_base(&self, base: &str) -> Vec<TypeReference> {
        self.base.types_with_base(base)
    }

    fn stands_for_container(&self, ty: TypeReference) -> bool {
        self.base.stands_for_container(ty)
    }

    fn global(&self, name: &str) -> Result<Lookup<Self::Object>, Refusal> {
        Ok(match self.base.global_step(name)? {
            Some((step, ty)) => Lookup::Object {
                object: ViewObject::Global(step),
                ty: Ok(ty),
            },
            None => Lookup::NotFound,
        })
    }
}

/// Binds `view`, from `set`, against what it presents, `self_type`, whose
/// identity its pattern matched, capturing `captures`.
#[expect(
    clippy::too_many_lines,
    reason = "each statement binds in its own arm, in the order the view writes them"
)]
pub fn bind<S: Scope>(
    view: &Arc<View>,
    set: &super::ViewSet,
    self_type: SelfType,
    captures: &Captures,
    base: &S,
) -> Result<BoundView<S::Step>, Rejection> {
    let mut scope = ViewScope::new(base, self_type, captures, set);
    let mut lets = Vec::new();

    // `let`s and `type`s, in order: each sees those before it.
    for statement in &view.statements {
        match statement {
            Statement::Let {
                name,
                alternatives,
                line,
            } => {
                let program = first_alternative(alternatives, |alternative| {
                    bind_value(&alternative.expression, &scope)
                        .map_err(|error| explain(&scope, alternative, &error))
                })
                .map_err(|reason| Rejection {
                    line: *line,
                    part: format!("let {name}"),
                    reason,
                })?;
                let ty = program.result().clone();
                let place = program.is_place() && matches!(ty, Ty::Program(_));
                scope.lets.push((name.clone(), ty, place));
                lets.push(program);
            }

            Statement::Type {
                name,
                alternatives,
                line,
            } => {
                let reference = first_alternative(alternatives, |alternative| {
                    resolve_type(alternative, &scope)
                })
                .map_err(|reason| Rejection {
                    line: *line,
                    part: format!("type {name}"),
                    reason,
                })?;
                scope.types.push((name.clone(), reference));
            }
            _ => {}
        }
    }

    let mut checks = Vec::new();
    let mut fields = Vec::new();
    let mut summary = None;
    let mut shape = None;
    let mut hidden = Vec::new();
    let mut formats = Vec::new();
    let mut visualizers = Vec::new();
    for statement in &view.statements {
        match statement {
            Statement::Check(expr) => {
                for conjunct in conjuncts(expr) {
                    checks.push(bind_check(&conjunct, &scope)?);
                }
            }
            Statement::Field { name, value } => fields.push(BoundField {
                name: name.as_str().into(),
                program: bind_part(value, &scope, Mode::Read)?,
            }),
            Statement::Summary(pieces) => summary = Some(bind_pieces(pieces, &scope)?),
            Statement::Show(shown) => shape = Some(bind_shape(shown, &mut scope)?),
            Statement::Hide { names, line } => {
                hidden.extend(
                    names
                        .iter()
                        .map(|name| (Arc::<str>::from(name.as_str()), *line)),
                );
            }

            Statement::Format {
                names,
                format,
                line,
            } => {
                let format = bind_format(format, *line, &scope)?;
                formats.extend(
                    names
                        .iter()
                        .map(|name| (Arc::<str>::from(name.as_str()), format, *line)),
                );
            }
            Statement::Visualize(visualize) => {
                visualizers.push(bind_visualizer(visualize, set, &scope));
            }
            Statement::Let { .. } | Statement::Type { .. } => {}
        }
    }

    // A view that does not `show` presents the value's members, and its
    // bases as members named by their types.
    let shape = match shape {
        Some(shape) => shape,
        None if view.extend => BoundShape::Record(Vec::new()),
        None => members(&mut scope, view.line)?,
    };

    let self_text = match formats
        .iter()
        .rev()
        .find(|(name, ..)| name.as_ref() == "self")
    {
        Some((_, BoundFormat::Utf8, line))
            if suits(BoundFormat::Utf8, &scope.self_type.ty(), &scope).is_ok() =>
        {
            Some(bind_text_source(&synthetic("self", *line)?, &scope)?)
        }
        _ => None,
    };

    let bound = BoundView {
        view: Arc::clone(view),
        lets,
        checks,
        fields,
        summary,
        shape,
        hidden,
        formats,
        self_text,
        extensions: Vec::new(),
        visualizers,
    };

    if !view.extend {
        let self_ty = presented(&bound.shape, scope.self_type);
        check_against(&bound, &bound.named_programs(), &self_ty, &scope)?;
    }

    Ok(bound)
}

/// Binds a `visualize`: its renderer, from the view's own set or else the
/// built-in ones, and each input, as a field binds.
fn bind_visualizer<S: Scope>(
    visualize: &Visualize,
    set: &super::ViewSet,
    scope: &ViewScope<'_, S>,
) -> BoundVisualizer<S::Step> {
    let bound = (|| {
        let renderer = set
            .renderer(&visualize.name)
            .cloned()
            .or_else(|| {
                super::ViewSet::built_in()
                    .renderer(&visualize.name)
                    .cloned()
            })
            .ok_or_else(|| Rejection {
                line: visualize.line,
                part: format!("visualize \"{}\"", visualize.name),
                reason: "no renderer has that name".to_owned(),
            })?;
        let mut inputs = Vec::new();
        for input in &visualize.inputs {
            let mut path = None;
            let value = match &input.value {
                InputValue::Value(alternatives) => {
                    let (program, part) = first_alternative(alternatives, |alternative| {
                        bind_part(alternative, scope, Mode::Read)
                            .map(|program| {
                                let part = input_path(alternative, &program);
                                (program, part)
                            })
                            .map_err(|rejection| rejection.reason)
                    })
                    .map_err(|reason| Rejection {
                        line: input.line,
                        part: input.name.clone(),
                        reason,
                    })?;
                    path = part;
                    BoundInputValue::Value(program)
                }
                InputValue::Bytes { pointer, length } => BoundInputValue::Bytes {
                    pointer: bind_category(
                        pointer,
                        scope,
                        |category| {
                            matches!(category, Category::Pointer(_) | Category::Integer { .. })
                        },
                        "`bytes` reads at a pointer or an address",
                    )?,
                    length: bind_integer(length, scope)?,
                },
                InputValue::Text(text) => BoundInputValue::Text(text.as_str().into()),
            };

            inputs.push(BoundInput {
                name: input.name.as_str().into(),
                value,
                path,
            });
        }

        Ok(BoundDrawing { renderer, inputs })
    })();
    BoundVisualizer {
        name: visualize.name.as_str().into(),
        line: visualize.line,
        bound,
    }
}

/// The part of the presented value an input's expression names, when it
/// is one: written as members and literal indices of `self`, and held in
/// its storage rather than reached through a pointer.
fn input_path<St>(expr: &Expr, program: &ViewProgram<St>) -> Option<Arc<str>> {
    if !matches!(
        program.root_object(),
        Some(ViewObject::This | ViewObject::Member(_))
    ) {
        return None;
    }

    let text = expr.text();
    let path = match text.strip_prefix("self") {
        Some(rest) if rest.is_empty() || rest.starts_with('[') => rest,
        Some(rest) if rest.starts_with('.') => &rest[1..],
        _ => text,
    };

    is_part_path(path).then(|| path.into())
}

/// Whether `path` is member names and literal indices, as `a.b[2].c`.
fn is_part_path(path: &str) -> bool {
    let mut rest = path;
    let mut first = true;
    while !rest.is_empty() {
        if let Some(index) = rest.strip_prefix('[') {
            let Some(end) = index.find(']') else {
                return false;
            };

            if end == 0 || !index[..end].bytes().all(|byte| byte.is_ascii_digit()) {
                return false;
            }

            rest = &index[end + 1..];
        } else {
            if !first {
                let Some(after) = rest.strip_prefix('.') else {
                    return false;
                };

                rest = after;
            }

            let end = rest
                .find(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
                .unwrap_or(rest.len());
            if end == 0 || rest.as_bytes()[0].is_ascii_digit() {
                return false;
            }

            rest = &rest[end..];
        }

        first = false;
    }

    true
}

/// The first alternative that binds, or every alternative's reason.
fn first_alternative<A, T>(
    alternatives: &[A],
    mut bind: impl FnMut(&A) -> Result<T, String>,
) -> Result<T, String> {
    let mut reasons = Vec::new();
    for alternative in alternatives {
        match bind(alternative) {
            Ok(bound) => return Ok(bound),
            Err(reason) => reasons.push(reason),
        }
    }

    Err(reasons.join("; or "))
}

/// What a view that does not `show` presents: a record's members, and its
/// bases as members named by their types, or any other value as itself.
fn members<S: Scope>(
    scope: &mut ViewScope<'_, S>,
    line: u32,
) -> Result<BoundShape<S::Step>, Rejection> {
    let SelfType::Program(self_type) = scope.self_type else {
        return Ok(BoundShape::Stored);
    };

    let record = representation(scope, self_type)
        .ok()
        .and_then(|(_, info)| match &info.kind {
            TypeKind::Record { members, bases, .. } => {
                Some((Arc::clone(members), Arc::clone(bases)))
            }
            _ => None,
        });
    let Some((members, bases)) = record else {
        return Ok(BoundShape::Value(bind_part(
            &synthetic("self", line)?,
            scope,
            Mode::Read,
        )?));
    };

    let mut fields = Vec::new();
    for (index, base) in bases.iter().enumerate() {
        let name = scope
            .type_info(base.type_ref)
            .map_or_else(|| format!("<base {index}>"), |info| info.name.to_string());
        let alias = format!("__base{index}");
        scope.types.push((alias.clone(), base.type_ref));
        fields.push(BoundField {
            name: name.into(),
            program: bind_part(
                &synthetic(&format!("({alias})self"), line)?,
                scope,
                Mode::Read,
            )?,
        });
    }

    for member in members.iter().filter(|member| !member.artificial) {
        let Some(name) = member.name.as_deref() else {
            continue;
        };

        fields.push(BoundField {
            name: name.into(),
            program: bind_part(
                &synthetic(&format!("self.`{name}`"), line)?,
                scope,
                Mode::Read,
            )?,
        });
    }

    Ok(BoundShape::Record(fields))
}

/// An expression the binder writes for a view.
fn synthetic(text: &str, line: u32) -> Result<Expr, Rejection> {
    Expression::parse_view(text)
        .map(|expression| Expr { expression, line })
        .map_err(|error| Rejection {
            line,
            part: text.to_owned(),
            reason: error.to_string(),
        })
}

/// A `format`, with the enumeration it names resolved.
fn bind_format<S: Scope>(
    format: &Format,
    line: u32,
    scope: &ViewScope<'_, S>,
) -> Result<BoundFormat, Rejection> {
    let enumeration = |ty: &TypeExpr| {
        let reference = resolve_type(ty, scope).map_err(|reason| Rejection {
            line,
            part: "format".to_owned(),
            reason,
        })?;
        match representation(scope, reference) {
            Ok((_, info)) if matches!(info.kind, TypeKind::Enumeration { .. }) => Ok(reference),
            _ => Err(Rejection {
                line,
                part: "format".to_owned(),
                reason: "`flags` and `enum` name an enumeration".to_owned(),
            }),
        }
    };

    Ok(match format {
        Format::Hex => BoundFormat::Hex,
        Format::Char => BoundFormat::Char,
        Format::Bytes => BoundFormat::Bytes,
        Format::Utf8 => BoundFormat::Utf8,
        Format::Utf16 => BoundFormat::Utf16,
        Format::Flags(ty) => BoundFormat::Flags(enumeration(ty)?),
        Format::Enum(ty) => BoundFormat::Enum(enumeration(ty)?),
        Format::Duration(unit) => BoundFormat::Duration(*unit),
        Format::Time(unit) => BoundFormat::Time(*unit),
    })
}

/// Every name an `extend` hides or formats is one of the members or
/// fields of the view it extends, or one of its own fields, or `self`, and
/// every format suits what it writes.
pub fn check_extension<St>(
    base: &BoundView<St>,
    extension: &BoundView<St>,
    self_type: SelfType,
    types: &dyn TypeSource,
) -> Result<(), Rejection> {
    let mut named = base.named_programs();
    named.extend(extension.named_programs());
    check_against(extension, &named, &presented(&base.shape, self_type), types)
}

/// What a view's `self` format writes: the value a `value` shape presents,
/// or else the value itself.
fn presented<St>(shape: &BoundShape<St>, self_type: SelfType) -> Ty {
    match shape {
        BoundShape::Value(program) => program.result().clone(),
        _ => self_type.ty(),
    }
}

/// Every name `bound` hides or formats is in `named`, or is `self`, and
/// every format suits what it writes.
fn check_against<St>(
    bound: &BoundView<St>,
    named: &[(Arc<str>, &ViewProgram<St>)],
    self_ty: &Ty,
    types: &dyn TypeSource,
) -> Result<(), Rejection> {
    for (name, line) in &bound.hidden {
        let line = *line;
        if !named.iter().any(|(candidate, _)| candidate == name) {
            return Err(Rejection {
                line,
                part: format!("hide {name}"),
                reason: format!("`{name}` is neither a member nor a field the view shows"),
            });
        }
    }

    for (name, format, line) in &bound.formats {
        let line = *line;
        let result = if name.as_ref() == "self" {
            Some(self_ty.clone())
        } else {
            named
                .iter()
                .find(|(candidate, _)| candidate == name)
                .map(|(_, program)| program.result().clone())
        };

        let Some(ty) = result else {
            return Err(Rejection {
                line,
                part: format!("format {name}"),
                reason: format!("`{name}` is neither a member nor a field the view shows"),
            });
        };

        if let Err(reason) = suits(*format, &ty, types) {
            return Err(Rejection {
                line,
                part: format!("format {name}"),
                reason,
            });
        }
    }

    Ok(())
}

/// Whether a format can write a value of `ty`.
fn suits(format: BoundFormat, ty: &Ty, types: &dyn TypeSource) -> Result<(), String> {
    let category = category(types, ty);
    let ok = match format {
        BoundFormat::Hex
        | BoundFormat::Char
        | BoundFormat::Flags(_)
        | BoundFormat::Enum(_)
        | BoundFormat::Duration(_)
        | BoundFormat::Time(_) => matches!(category, Category::Integer { .. }),
        BoundFormat::Bytes => matches!(ty, Ty::Program(_)),
        // An element may be named, as C's `uint8_t` names `unsigned char`.
        BoundFormat::Utf8 => match category {
            Category::Array { element, .. } | Category::Slice(element) => {
                representation(types, element).is_ok_and(|(_, info)| {
                    matches!(&info.kind, TypeKind::Base(base) if base.byte_size == 1
                        && !matches!(base.encoding, BaseTypeEncoding::Boolean | BaseTypeEncoding::Floating))
                })
            }
            _ => false,
        },
        BoundFormat::Utf16 => match category {
            Category::Array { element, .. } => representation(types, element)
                .is_ok_and(|(_, info)| info.byte_size == Some(2)),
            _ => false,
        },
    };

    if ok {
        return Ok(());
    }

    let what = match format {
        BoundFormat::Bytes => "a value in memory",
        BoundFormat::Utf8 => "an array or slice of bytes",
        BoundFormat::Utf16 => "an array of 16-bit units",
        _ => "an integer",
    };

    Err(format!("the format writes {what}"))
}

/// An error binding `expr`, said in terms of the view.
fn explain<S: Scope>(
    scope: &ViewScope<'_, S>,
    expr: &Expr,
    error: &crate::ExpressionError,
) -> String {
    let at = error.span.text(expr.expression.text());
    if error.kind == ErrorKind::UnknownName {
        return format!(
            "`{at}` is neither a member of `{}` nor a name the view declares",
            scope.self_name()
        );
    }

    if at.is_empty() || at == expr.text() {
        error.message.clone()
    } else {
        format!("`{at}`: {}", error.message)
    }
}

fn rejection<S: Scope>(
    scope: &ViewScope<'_, S>,
    expr: &Expr,
    error: &crate::ExpressionError,
) -> Rejection {
    Rejection {
        line: expr.line,
        part: expr.text().to_owned(),
        reason: explain(scope, expr, error),
    }
}

fn bind_part<S: Scope>(
    expr: &Expr,
    scope: &ViewScope<'_, S>,
    mode: Mode,
) -> Result<ViewProgram<S::Step>, Rejection> {
    bind_expression(&expr.expression, scope, mode).map_err(|error| rejection(scope, expr, &error))
}

fn bind_value_part<S: Scope>(
    expr: &Expr,
    scope: &ViewScope<'_, S>,
) -> Result<ViewProgram<S::Step>, Rejection> {
    bind_value(&expr.expression, scope).map_err(|error| rejection(scope, expr, &error))
}

fn bind_condition_part<S: Scope>(
    expr: &Expr,
    scope: &ViewScope<'_, S>,
) -> Result<ViewProgram<S::Step>, Rejection> {
    bind_condition(&expr.expression, scope).map_err(|error| rejection(scope, expr, &error))
}

/// Binds an expression whose value's category `accepts`, or says `reason`.
fn bind_category<S: Scope>(
    expr: &Expr,
    scope: &ViewScope<'_, S>,
    accepts: fn(&Category) -> bool,
    reason: &str,
) -> Result<ViewProgram<S::Step>, Rejection> {
    let program = bind_value_part(expr, scope)?;
    if accepts(&category(scope, program.result())) {
        Ok(program)
    } else {
        Err(Rejection {
            line: expr.line,
            part: expr.text().to_owned(),
            reason: reason.to_owned(),
        })
    }
}

fn bind_integer<S: Scope>(
    expr: &Expr,
    scope: &ViewScope<'_, S>,
) -> Result<ViewProgram<S::Step>, Rejection> {
    bind_category(
        expr,
        scope,
        |category| matches!(category, Category::Integer { .. }),
        "is not an integer",
    )
}

const fn is_pointer(category: &Category) -> bool {
    matches!(category, Category::Pointer(_))
}

const NODES_ARE_POINTERS: &str = "a linked structure's nodes are pointers";

/// A check's conjuncts, each its own check, so a failure names the one
/// that does not hold.
fn conjuncts(expr: &Expr) -> Vec<Expr> {
    fn split(
        tree: &crate::eval::syntax::Tree,
        id: crate::eval::syntax::ast::NodeId,
        text: &str,
        out: &mut Vec<String>,
    ) {
        match tree.kind(id) {
            NodeKind::Binary {
                op: BinaryOp::And,
                left,
                right,
            } => {
                split(tree, *left, text, out);
                split(tree, *right, text, out);
            }
            _ => out.push(tree.span(id).text(text).trim().to_owned()),
        }
    }

    let Some(tree) = expr.expression.tree() else {
        return vec![expr.clone()];
    };

    let mut texts = Vec::new();
    split(tree, tree.root(), expr.expression.text(), &mut texts);
    let parsed = texts
        .iter()
        .map(|text| Expression::parse_view(text))
        .collect::<Result<Vec<_>, _>>();
    match parsed {
        Ok(expressions) if expressions.len() > 1 => expressions
            .into_iter()
            .map(|expression| Expr {
                expression,
                line: expr.line,
            })
            .collect(),
        _ => vec![expr.clone()],
    }
}

fn bind_check<S: Scope>(
    expr: &Expr,
    scope: &ViewScope<'_, S>,
) -> Result<BoundCheck<S::Step>, Rejection> {
    let program = bind_condition_part(expr, scope)?;

    // A comparison's sides say why it failed.
    let sides = expr.expression.tree().and_then(|tree| {
        let NodeKind::Binary { op, left, right } = tree.kind(tree.root()) else {
            return None;
        };

        if !matches!(
            op,
            BinaryOp::Eq | BinaryOp::Ne | BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge
        ) {
            return None;
        }

        let side = |id| {
            let text = tree.span(id).text(expr.expression.text()).trim().to_owned();
            let expression = Expression::parse_view(&text).ok()?;
            let program = bind_value(&expression, scope).ok()?;
            Some((text, program))
        };

        Some([side(*left)?, side(*right)?])
    });
    Ok(BoundCheck {
        text: expr.text().to_owned(),
        program,
        sides,
    })
}

fn bind_shape<S: Scope>(
    shape: &Shape,
    scope: &mut ViewScope<'_, S>,
) -> Result<BoundShape<S::Step>, Rejection> {
    Ok(match shape {
        Shape::Text { pointer, length } => BoundShape::Text {
            source: bind_text_source(pointer, scope)?,
            length: length
                .as_ref()
                .map(|length| bind_integer(length, scope))
                .transpose()?,
        },
        Shape::Value(value) => BoundShape::Value(bind_part(value, scope, Mode::Read)?),
        Shape::Empty(pieces) => BoundShape::Empty(bind_pieces(pieces, scope)?),
        Shape::Sequence {
            count,
            clauses,
            element,
        } => {
            let scan = bind_scan(count, clauses, scope)?;
            let element = bind_part(element, scope, Mode::Read);
            scope.variables.clear();
            BoundShape::Sequence {
                scan,
                element: element?,
            }
        }

        Shape::Map {
            count,
            clauses,
            key,
            value,
        } => {
            let scan = bind_scan(count, clauses, scope)?;
            let bound = bind_part(key, scope, Mode::Read)
                .and_then(|key| Ok((key, bind_part(value, scope, Mode::Read)?)));
            scope.variables.clear();
            let (key, value) = bound?;
            BoundShape::Map { scan, key, value }
        }

        Shape::If {
            condition,
            then,
            otherwise,
        } => BoundShape::If {
            condition: bind_condition_part(condition, scope)?,
            then: Box::new(bind_shape(then, scope)?),
            otherwise: Box::new(bind_shape(otherwise, scope)?),
        },
        Shape::Record(members) => BoundShape::Record(
            members
                .iter()
                .map(|(name, value)| {
                    Ok(BoundField {
                        name: name.as_str().into(),
                        program: bind_part(value, scope, Mode::Read)?,
                    })
                })
                .collect::<Result<_, Rejection>>()?,
        ),
        Shape::Dynamic { pointer, ty } => BoundShape::Dynamic {
            pointer: bind_category(pointer, scope, is_pointer, "is not a pointer")?,
            ty: bind_dynamic_type(ty, pointer.line, scope)?,
        },
        Shape::Unmatched(expr) => BoundShape::Unmatched {
            text: expr.text().to_owned(),
            program: bind_value_part(expr, scope)?,
        },
    })
}

/// The type of a `dynamic` shape: one type, or the arguments of a type
/// for its index to choose among.
fn bind_dynamic_type<S: Scope>(
    ty: &DynamicType,
    line: u32,
    scope: &ViewScope<'_, S>,
) -> Result<BoundDynamic<S::Step>, Rejection> {
    let rejected = |reason: String| Rejection {
        line,
        part: "dynamic".to_owned(),
        reason,
    };

    match ty {
        DynamicType::Fixed(ty) => resolve_type(ty, scope)
            .map(BoundDynamic::Fixed)
            .map_err(rejected),
        DynamicType::Argument { of, index } => {
            let of = resolve_type(of, scope).map_err(rejected)?;
            let info = representation(scope, of)
                .map_err(|reason| rejected(reason.to_string()))?
                .1;
            let types = info
                .identity
                .as_ref()
                .map(|identity| {
                    identity
                        .arguments
                        .iter()
                        .map(|argument| match argument {
                            TypeArgument::Type(reference) => Some(*reference),
                            _ => None,
                        })
                        .collect()
                })
                .unwrap_or_default();
            Ok(BoundDynamic::Argument {
                types,
                index: bind_integer(index, scope)?,
            })
        }
        DynamicType::Function { ty, code } => {
            let TypeExpr::Named { name, pointers: 0 } = ty else {
                return Err(rejected(
                    "a type a function's arguments complete is named with its arguments".to_owned(),
                ));
            };

            let pattern = super::syntax::type_pattern(name)
                .map_err(|reason| rejected(format!("`{name}`: {reason}")))?;

            // A parameter's name alone is the argument itself.
            let argument = is_capture_name(name);
            let candidates = if argument {
                Vec::new()
            } else {
                candidates(&pattern, scope)
            };

            if candidates.is_empty() && !argument {
                return Err(rejected(format!("no type is `{name}`")));
            }

            Ok(BoundDynamic::Function {
                code: bind_category(
                    code,
                    scope,
                    |category| matches!(category, Category::Pointer(_) | Category::Integer { .. }),
                    "is not the address of code",
                )?,
                name: name.as_str().into(),
                argument,
                pattern: Arc::new(pattern),
                known: known_types(scope),
                candidates: candidates.into(),
            })
        }
    }
}

/// Binds a count and its clauses, leaving the clauses' variables in scope
/// for the element, or the key and value, that follows; the caller clears
/// them.
fn bind_scan<S: Scope>(
    count: &Count,
    clauses: &[Clause],
    scope: &mut ViewScope<'_, S>,
) -> Result<BoundScan<S::Step>, Rejection> {
    let count = match count {
        Count::Known(count) => Some(bind_integer(count, scope)?),
        Count::Unknown => None,
    };

    let mut bound = Vec::new();
    for clause in clauses {
        match bind_clause(clause, scope) {
            Ok(clause) => bound.push(clause),
            Err(rejection) => {
                scope.variables.clear();
                return Err(rejection);
            }
        }
    }

    Ok(BoundScan {
        count,
        clauses: bound,
    })
}

/// Binds one clause, then puts its variable in scope.
fn bind_clause<S: Scope>(
    clause: &Clause,
    scope: &mut ViewScope<'_, S>,
) -> Result<BoundClause<S::Step>, Rejection> {
    let before = scope.variables.len();
    let (generator, ty) = match &clause.generator {
        Generator::Range(length) => (
            BoundGenerator::Range(bind_integer(length, scope)?),
            Ty::Exact,
        ),
        Generator::List { head, next } => {
            let (head, ty) = bind_node(head, scope)?;
            let next = bind_link(next, &ty, scope)?;
            (BoundGenerator::List { head, next }, ty)
        }
        Generator::Inorder { root, left, right } => {
            let (root, ty) = bind_node(root, scope)?;
            let left = bind_link(left, &ty, scope)?;
            let right = bind_link(right, &ty, scope)?;
            (BoundGenerator::Inorder { root, left, right }, ty)
        }

        Generator::Kernel {
            name,
            line,
            arguments,
        } => {
            let kernel = scope
                .set
                .kernel(name)
                .or_else(|| super::ViewSet::built_in().kernel(name))
                .ok_or_else(|| Rejection {
                    line: *line,
                    part: format!("kernel(\"{name}\")"),
                    reason: "no kernel has that name".to_owned(),
                })?;

            // Each is a 64-bit word to the kernel.
            let arguments = arguments
                .iter()
                .map(|argument| {
                    bind_category(
                        argument,
                        scope,
                        |category| {
                            matches!(
                                category,
                                Category::Integer { .. } | Category::Pointer(_) | Category::Bool
                            )
                        },
                        "a kernel's arguments are integers, pointers, and truth values",
                    )
                })
                .collect::<Result<_, _>>()?;
            (
                BoundGenerator::Kernel {
                    kernel,
                    arguments,
                    words: clause.variables.len(),
                },
                Ty::Exact,
            )
        }
    };

    for variable in &clause.variables {
        scope.variables.push((variable.clone(), ty.clone(), false));
    }

    let mut items = Vec::new();
    for item in &clause.items {
        items.push(match item {
            Item::Filter(filter) => BoundItem::Filter(bind_condition_part(filter, scope)?),
            Item::Let { name, value } => {
                let program = bind_value_part(value, scope)?;
                let ty = program.result().clone();
                let place = program.is_place() && matches!(ty, Ty::Program(_));
                scope.variables.push((name.clone(), ty, place));
                BoundItem::Let(program)
            }
        });
    }

    let clause = BoundClause { generator, items };

    // The scanner steps between clauses' values by this width.
    debug_assert_eq!(
        scope.variables.len() - before,
        clause.width(),
        "a clause puts other than its width of variables in scope"
    );
    Ok(clause)
}

/// A linked structure's first node: a pointer, whose type every node has.
fn bind_node<S: Scope>(
    expr: &Expr,
    scope: &ViewScope<'_, S>,
) -> Result<(ViewProgram<S::Step>, Ty), Rejection> {
    let program = bind_category(expr, scope, is_pointer, NODES_ARE_POINTERS)?;
    let ty = program.result().clone();
    Ok((program, ty))
}

/// `P => EXPR`, with `P` a node of type `ty`; the next node is a pointer.
fn bind_link<S: Scope>(
    link: &super::syntax::Link,
    ty: &Ty,
    scope: &mut ViewScope<'_, S>,
) -> Result<ViewProgram<S::Step>, Rejection> {
    scope
        .variables
        .push((link.parameter.clone(), ty.clone(), false));
    let program = bind_category(&link.expression, scope, is_pointer, NODES_ARE_POINTERS);
    scope.variables.pop();
    program
}

/// How many bytes wide a type's values are as text: one for a character
/// or a byte, and two or four for a character type that wide, whose
/// values are UTF-16 or UTF-32 units.
fn text_unit_width<S: Scope>(scope: &ViewScope<'_, S>, ty: &Ty) -> Option<usize> {
    match ty {
        Ty::Int(int) => (int.width() == 8).then_some(1),
        Ty::C(_) => is_character(scope, ty).then_some(1),
        Ty::Program(reference) => match representation(scope, *reference) {
            Ok((
                _,
                TypeInfo {
                    kind: TypeKind::Base(base),
                    ..
                },
            )) => match (base.byte_size, base.encoding) {
                (
                    _,
                    BaseTypeEncoding::Boolean
                    | BaseTypeEncoding::Floating
                    | BaseTypeEncoding::ComplexFloating,
                ) => None,
                (1, _) => Some(1),
                (
                    width @ (2 | 4),
                    BaseTypeEncoding::SignedCharacter | BaseTypeEncoding::UnsignedCharacter,
                ) => usize::try_from(width).ok(),
                _ => None,
            },
            _ => None,
        },
        _ => None,
    }
}

fn bind_text_source<S: Scope>(
    pointer: &Expr,
    scope: &ViewScope<'_, S>,
) -> Result<TextSource<S::Step>, Rejection> {
    let program = bind_value_part(pointer, scope)?;
    let refuse = || Rejection {
        line: pointer.line,
        part: pointer.text().to_owned(),
        reason: "`text` takes a pointer to characters, or an array or slice of them".to_owned(),
    };

    match category(scope, program.result()) {
        Category::Pointer(Some(target)) => {
            let width = text_unit_width(scope, &target).ok_or_else(refuse)?;
            Ok(TextSource::Pointer { program, width })
        }
        Category::Array { element, .. } | Category::Slice(element) if program.is_place() => {
            let width = text_unit_width(scope, &Ty::Program(element)).ok_or_else(refuse)?;
            let Ty::Program(from) = program.result() else {
                return Err(refuse());
            };

            let planned = scope
                .plan(*from, StepKind::Index { available: 1 })
                .map_err(|refusal| Rejection {
                    line: pointer.line,
                    part: pointer.text().to_owned(),
                    reason: refusal.message,
                })?;
            if planned.consumed != 1 {
                return Err(refuse());
            }

            Ok(TextSource::Elements {
                program,
                first: planned.step,
                width,
            })
        }
        _ => Err(refuse()),
    }
}

/// The type a `type` statement's alternative names.
fn resolve_type<S: Scope>(
    expr: &TypeExpr,
    scope: &ViewScope<'_, S>,
) -> Result<TypeReference, String> {
    match expr {
        TypeExpr::Named { name, pointers } => {
            if *pointers != 0 {
                return Err(format!(
                    "`{name}*`: a `type` names a type; write the pointer where it is used"
                ));
            }

            match scope.lookup_type(&TypeQuery {
                name: name.clone(),
                tag: None,
            }) {
                TypeLookup::Found(reference) => Ok(reference),
                TypeLookup::Ambiguous(candidates) => Err(format!(
                    "`{name}` names several types: {}",
                    candidates.join(", ")
                )),
                TypeLookup::NotFound if name.contains(['<', '(', '[']) => construct(name, scope),
                TypeLookup::NotFound => Err(format!("no type is named `{name}`")),
            }
        }
        TypeExpr::Nested { of, name } => {
            let of = resolve_type(of, scope)?;
            let outer = scope
                .type_info(of)
                .map(|info| Arc::clone(&info.name))
                .ok_or_else(|| "the type is malformed".to_owned())?;
            for separator in [".", "::"] {
                let full = format!("{outer}{separator}{name}");
                match scope.lookup_type(&TypeQuery {
                    name: full.clone(),
                    tag: None,
                }) {
                    TypeLookup::Found(reference) => return Ok(reference),
                    TypeLookup::Ambiguous(candidates) => {
                        return Err(format!(
                            "`{full}` names several types: {}",
                            candidates.join(", ")
                        ));
                    }
                    TypeLookup::NotFound => {}
                }
            }

            Err(format!("`{outer}` declares no type `{name}`"))
        }
        TypeExpr::TypeOf(expr) => {
            let program = bind_expression(&expr.expression, scope, Mode::Read)
                .map_err(|error| format!("`{}`: {}", expr.text(), explain(scope, expr, &error)))?;
            match program.result() {
                Ty::Program(reference) => Ok(*reference),
                _ => Err(format!("`{}` has no type of the program's", expr.text())),
            }
        }
        TypeExpr::Arg { of, index } => {
            let of = resolve_type(of, scope)?;
            let info = representation(scope, of)
                .map_err(|reason| reason.to_string())?
                .1;
            let argument = info
                .identity
                .as_ref()
                .and_then(|identity| identity.arguments.get(*index as usize).cloned());
            match argument {
                Some(TypeArgument::Type(reference)) => Ok(reference),
                Some(_) => Err(format!("argument {index} of `{}` is not a type", info.name)),
                None => Err(format!("`{}` has no argument {index}", info.name)),
            }
        }
    }
}

/// The one type whose identity a name with arguments spells, the
/// arguments being types the program defines, arguments the view's
/// pattern captured, or types the view names: `std::_Rb_tree_node<Value>`.
/// It is found through the type index by comparing identities, never by
/// spelling the type's name.
fn construct<S: Scope>(name: &str, scope: &ViewScope<'_, S>) -> Result<TypeReference, String> {
    let pattern =
        super::syntax::type_pattern(name).map_err(|reason| format!("`{name}`: {reason}"))?;
    let known = known_types(scope);
    if let Some(unknown) = unknown_capture(&pattern, &known) {
        return Err(format!(
            "`{name}`: `{unknown}` is neither an argument the pattern captured nor a type the view names"
        ));
    }

    let mut found = Vec::<TypeReference>::new();
    for candidate in candidates(&pattern, scope) {
        let Some(identity) = scope
            .type_info(candidate)
            .and_then(|info| info.identity.as_deref())
        else {
            continue;
        };

        if super::pattern::matches_with(&pattern, identity, scope, known.clone()).is_some()
            && !found.iter().any(|other| scope.same_type(*other, candidate))
        {
            found.push(candidate);
        }
    }

    match found.as_slice() {
        [reference] => Ok(*reference),
        [] => Err(format!("no type is `{name}`")),
        _ => Err(format!(
            "`{name}` names several types: {}",
            found
                .iter()
                .filter_map(|reference| scope.type_info(*reference))
                .map(|info| info.name.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// The types a view knows by name: the arguments its pattern captured and
/// the types its `type` statements name, which shadow them.
fn known_types<S: Scope>(scope: &ViewScope<'_, S>) -> Captures {
    let mut known = scope.captures.clone();
    for (type_name, reference) in &scope.types {
        known.retain(|(captured, _)| captured != type_name);
        known.push((type_name.clone(), Captured::Type(*reference)));
    }

    known
}

/// The program's types a pattern may name: those of its base name, in the
/// language of the type the view presents.
fn candidates<S: Scope>(pattern: &Pattern, scope: &ViewScope<'_, S>) -> Vec<TypeReference> {
    let language = scope
        .self_type
        .program()
        .and_then(|reference| scope.type_info(reference))
        .and_then(|info| info.identity.as_ref().map(|identity| identity.language));
    scope
        .types_with_base(&pattern.base)
        .into_iter()
        .filter(|candidate| {
            scope
                .type_info(*candidate)
                .and_then(|info| info.identity.as_deref())
                .is_some_and(|identity| {
                    language.is_none_or(|language| identity.language == language)
                })
        })
        .collect()
}

/// A summary's text and holes, bound.
fn bind_pieces<S: Scope>(
    pieces: &[Piece],
    scope: &ViewScope<'_, S>,
) -> Result<Vec<BoundPiece<S::Step>>, Rejection> {
    pieces
        .iter()
        .map(|piece| {
            Ok(match piece {
                Piece::Literal(text) => BoundPiece::Literal(text.clone()),
                Piece::Hole { value, format } => BoundPiece::Hole {
                    program: bind_part(value, scope, Mode::Read)?,
                    format: format
                        .as_ref()
                        .map(|format| bind_format(format, value.line, scope))
                        .transpose()?,
                },
            })
        })
        .collect()
}

/// Whether a type's name is a capture's: one capitalized word.
fn is_capture_name(name: &str) -> bool {
    name.chars().next().is_some_and(char::is_uppercase)
        && name
            .chars()
            .all(|character| character.is_alphanumeric() || character == '_')
}

/// A capitalized name in a pattern that nothing the view knows names.
fn unknown_capture(pattern: &Pattern, known: &Captures) -> Option<String> {
    pattern
        .arguments
        .iter()
        .flatten()
        .find_map(|argument| match argument {
            ArgumentPattern::Capture(name) if !known.iter().any(|(known, _)| known == name) => {
                Some(name.clone())
            }
            ArgumentPattern::Type(inner) => unknown_capture(inner, known),
            _ => None,
        })
}
