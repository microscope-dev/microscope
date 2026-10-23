//! Views: how a value of a type a view matches is presented as the thing
//! it stands for, such as a `std::string` as its text or a `Vec` as its
//! elements, while the stored value stays one step away
//! (`docs/views.md`).
//!
//! This module is pure, as the evaluator is: it reaches a program only
//! through the evaluator's traits, which the debugger implements, and never
//! performs I/O, reads clocks, or starts threads. A view is parsed once,
//! bound against each concrete type it matches before anything runs, and
//! then run at stops, charging every read to the inspection's budget.

pub mod bind;
pub mod embedded;
pub mod format;
#[cfg(any(test, feature = "fuzzing"))]
pub mod fuzz;
pub mod kernel;
pub mod pattern;
pub mod run;
pub mod scan;
pub mod summary;
pub mod syntax;

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};

use crate::eval::target::Scope;
use crate::eval::types::TypeSource;
use crate::{TypeInfo, TypeReference, ViewName};

use bind::{BoundView, Rejection, SelfType};
use syntax::View;

/// The view files built into microscope, in the order they are tried.
const BUILT_IN: [(&str, &str); 18] = [
    (
        "libstdc++.views",
        include_str!("../../views/libstdc++.views"),
    ),
    ("libc++.views", include_str!("../../views/libc++.views")),
    ("rust-std.views", include_str!("../../views/rust-std.views")),
    ("tokio.views", include_str!("../../views/tokio.views")),
    ("zig-std.views", include_str!("../../views/zig-std.views")),
    (
        "odin-core.views",
        include_str!("../../views/odin-core.views"),
    ),
    ("d-std.views", include_str!("../../views/d-std.views")),
    (
        "nim-system.views",
        include_str!("../../views/nim-system.views"),
    ),
    ("gnat.views", include_str!("../../views/gnat.views")),
    (
        "go-runtime.views",
        include_str!("../../views/go-runtime.views"),
    ),
    ("go-time.views", include_str!("../../views/go-time.views")),
    ("go-sync.views", include_str!("../../views/go-sync.views")),
    ("go-text.views", include_str!("../../views/go-text.views")),
    (
        "go-errors.views",
        include_str!("../../views/go-errors.views"),
    ),
    (
        "go-containers.views",
        include_str!("../../views/go-containers.views"),
    ),
    ("glibc.views", include_str!("../../views/glibc.views")),
    (
        "python-std.views",
        include_str!("../../views/python-std.views"),
    ),
    (
        "javascript-std.views",
        include_str!("../../views/javascript-std.views"),
    ),
];

/// The kernels built into microscope: each one's name, the source it is built
/// from, and its module, which `just build-test-programs` checks is what
/// that source builds.
const BUILT_IN_KERNELS: [(&str, &str, &[u8]); 1] = [(
    "rust-btree",
    "views/kernels/rust-btree.zig",
    include_bytes!("../../views/kernels/rust-btree.wasm"),
)];

/// The renderers built into microscope, which any `visualize` may call: each
/// one's name and its JavaScript (`docs/visualizers.md`). They use only
/// the API every renderer has.
const BUILT_IN_RENDERERS: [(&str, &str); 11] = [
    (
        "line-plot",
        include_str!("../../views/visualizers/line-plot.js"),
    ),
    (
        "bar-chart",
        include_str!("../../views/visualizers/bar-chart.js"),
    ),
    (
        "scatter-plot",
        include_str!("../../views/visualizers/scatter-plot.js"),
    ),
    (
        "histogram",
        include_str!("../../views/visualizers/histogram.js"),
    ),
    (
        "box-plot",
        include_str!("../../views/visualizers/box-plot.js"),
    ),
    (
        "donut-chart",
        include_str!("../../views/visualizers/donut-chart.js"),
    ),
    (
        "heatmap",
        include_str!("../../views/visualizers/heatmap.js"),
    ),
    (
        "flame-graph",
        include_str!("../../views/visualizers/flame-graph.js"),
    ),
    ("bitmap", include_str!("../../views/visualizers/bitmap.js")),
    ("bits", include_str!("../../views/visualizers/bits.js")),
    ("mesh", include_str!("../../views/visualizers/mesh.js")),
];

/// The largest renderer read, in bytes.
pub const MAX_RENDERER_BYTES: usize = 256 * 1024;

/// Views from one source, such as the files loaded for a session, a
/// module's embedded views, or the built-in ones, in the order they are
/// tried, and the kernels they may call. Immutable once made; loading
/// views makes a new set.
#[derive(Debug)]
pub struct ViewSet {
    views: Vec<Arc<View>>,

    /// Each base name's views, in order.
    by_base: BTreeMap<String, Vec<usize>>,

    /// Each kernel, by name, with where it was loaded from.
    kernels: BTreeMap<String, (Arc<str>, Arc<kernel::Kernel>)>,

    /// Each renderer, by name.
    renderers: BTreeMap<String, Arc<crate::Renderer>>,
    errors: Vec<syntax::Error>,
}

impl ViewSet {
    /// Parses files, highest precedence first.
    #[must_use]
    pub fn new<'a>(files: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        let mut views = Vec::new();
        let mut errors = Vec::new();

        for (source, text) in files {
            let file = syntax::parse(source, text);
            views.extend(file.views);
            errors.extend(file.errors);
        }

        let mut by_base = BTreeMap::<String, Vec<usize>>::new();

        for (index, view) in views.iter().enumerate() {
            by_base
                .entry(view.pattern.base.clone())
                .or_default()
                .push(index);
        }

        Self {
            views,
            by_base,
            kernels: BTreeMap::new(),
            renderers: BTreeMap::new(),
            errors,
        }
    }

    /// Adds renderers loaded from `origin`, each a name and its
    /// JavaScript. A renderer too large, or whose name an earlier one has,
    /// is an error of `origin`.
    pub fn add_renderers<'a>(
        &mut self,
        origin: &str,
        renderers: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) {
        for (name, source) in renderers {
            let error = |message: String| syntax::Error {
                source: Arc::from(origin),
                line: 0,
                column: 0,
                message: format!("renderer `{name}`: {message}"),
            };

            if !syntax::is_renderer_name(name) {
                self.errors.push(error(syntax::RENDERER_NAME.to_owned()));
                continue;
            }

            if source.len() > MAX_RENDERER_BYTES {
                self.errors.push(error(format!(
                    "a renderer may be at most {MAX_RENDERER_BYTES} bytes long"
                )));
                continue;
            }

            if self.renderers.contains_key(name) {
                self.errors
                    .push(error("an earlier renderer has the same name".to_owned()));
                continue;
            }

            self.renderers.insert(
                name.to_owned(),
                Arc::new(crate::Renderer::new(name, origin, source)),
            );
        }
    }

    /// The renderer named `name`, if the set has one.
    #[must_use]
    pub fn renderer(&self, name: &str) -> Option<&Arc<crate::Renderer>> {
        self.renderers.get(name)
    }

    /// The set's renderers, by name.
    pub fn renderers(&self) -> impl Iterator<Item = &Arc<crate::Renderer>> + '_ {
        self.renderers.values()
    }

    /// Adds kernels loaded from `origin`, each a name, its source or a link
    /// to it, and its module. A kernel that cannot load, or whose name an
    /// earlier one has, is an error of `origin`.
    pub fn add_kernels<'a>(
        &mut self,
        origin: &str,
        kernels: impl IntoIterator<Item = (&'a str, &'a str, &'a [u8])>,
    ) {
        for (name, source, wasm) in kernels {
            let error = |message: String| syntax::Error {
                source: Arc::from(origin),
                line: 0,
                column: 0,
                message: format!("kernel `{name}`: {message}"),
            };

            if !syntax::is_kernel_name(name) {
                self.errors.push(error(syntax::KERNEL_NAME.to_owned()));
                continue;
            }

            if self.kernels.contains_key(name) {
                self.errors
                    .push(error("an earlier kernel has the same name".to_owned()));
                continue;
            }

            match kernel::Kernel::new(name, source, wasm) {
                Ok(kernel) => {
                    self.kernels
                        .insert(name.to_owned(), (Arc::from(origin), Arc::new(kernel)));
                }
                Err(message) => self.errors.push(error(message)),
            }
        }
    }

    /// The kernel named `name`, if the set has one.
    #[must_use]
    pub fn kernel(&self, name: &str) -> Option<Arc<kernel::Kernel>> {
        self.kernels.get(name).map(|(_, kernel)| Arc::clone(kernel))
    }

    /// The set's kernels, with where each was loaded from and what it is
    /// built from.
    pub fn kernels(&self) -> impl Iterator<Item = crate::KernelSource> + '_ {
        self.kernels
            .values()
            .map(|(origin, kernel)| crate::KernelSource {
                name: Arc::clone(kernel.name()),
                origin: Arc::clone(origin),
                source: Arc::from(kernel.source()),
            })
    }

    /// The built-in views alone, with the built-in kernels.
    #[must_use]
    pub fn built_in() -> Arc<Self> {
        static BUILT_IN_SET: OnceLock<Arc<ViewSet>> = OnceLock::new();
        Arc::clone(BUILT_IN_SET.get_or_init(|| {
            let mut set = Self::new(BUILT_IN);
            set.add_kernels("built-in", BUILT_IN_KERNELS);
            set.add_renderers("built-in", BUILT_IN_RENDERERS);
            Arc::new(set)
        }))
    }

    /// A set of no views.
    #[must_use]
    pub fn empty() -> Arc<Self> {
        static EMPTY: OnceLock<Arc<ViewSet>> = OnceLock::new();
        Arc::clone(EMPTY.get_or_init(|| Arc::new(Self::new([]))))
    }

    /// The views, in the order they are tried.
    #[must_use]
    pub fn views(&self) -> &[Arc<View>] {
        &self.views
    }

    /// What kept parts of the files out.
    #[must_use]
    pub fn errors(&self) -> &[syntax::Error] {
        &self.errors
    }

    /// Whether a view of the set names a type of this identity's base, or
    /// its Go kind.
    #[must_use]
    pub fn names(&self, identity: &crate::TypeIdentity) -> bool {
        self.by_base.contains_key(identity.base.as_ref())
            || pattern::go_kind_word(identity).is_some_and(|kind| self.by_base.contains_key(kind))
    }
}

/// A view a type was matched against, and whether it bound.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub name: Arc<ViewName>,

    /// `None` when it bound.
    pub rejection: Option<Rejection>,
}

/// Which view presents a type: the first candidate that binds, with why
/// each candidate before it did not, and the `extend`s that add to it.
/// Views after it are not tried; `extend`s are.
#[derive(Debug, Clone)]
pub struct Choice<St> {
    pub bound: Option<Arc<BoundView<St>>>,
    pub candidates: Vec<Candidate>,
}

impl<St> Default for Choice<St> {
    fn default() -> Self {
        Self {
            bound: None,
            candidates: Vec::new(),
        }
    }
}

/// How a view is named in results and diagnostics.
#[must_use]
pub fn name_of(view: &View) -> Arc<ViewName> {
    Arc::new(ViewName {
        source: Arc::clone(&view.source),
        line: view.line,
        header: Arc::clone(&view.header),
        extend: view.extend,
    })
}

/// Whether `ty`, through its typedefs and qualifiers, is a pointer that
/// only stands for a container its language gives a kind of its own, which
/// a pattern names by that kind: a Go map or channel. Expressions index,
/// measure, and size such a value through its view, never as a pointer.
#[must_use]
pub fn stands_for_container(types: &dyn TypeSource, mut ty: TypeReference) -> bool {
    for _ in 0..64 {
        let Some(info) = types.type_info(ty) else {
            return false;
        };

        if info
            .identity
            .as_deref()
            .and_then(pattern::go_kind_word)
            .is_some_and(|kind| matches!(kind, "map" | "chan"))
        {
            return true;
        }

        match info.kind {
            crate::TypeKind::Named {
                target: Some(target),
                ..
            }
            | crate::TypeKind::Modified { target, .. } => ty = target,
            _ => return false,
        }
    }

    false
}

/// Chooses the view for values of `ty`: the first, in the set's order,
/// whose pattern names the type and which binds against it in `scope`. A
/// typedef's own identity is tried before what it stands for, as Go's map
/// types are typedefs of a pointer.
#[cfg(any(test, feature = "fuzzing"))]
pub fn choose<S: Scope>(views: &ViewSet, ty: TypeReference, scope: &S) -> Choice<S::Step> {
    choose_among(&[views], ty, scope)
}

/// As [`choose`], among the views of several sets, each set's before the
/// next's.
pub fn choose_among<S: Scope>(sets: &[&ViewSet], ty: TypeReference, scope: &S) -> Choice<S::Step> {
    let mut choice = Choice::default();

    for (ty, info) in wrappers(scope, ty) {
        let Some(identity) = info.identity.as_deref() else {
            continue;
        };

        if let Some(bound) =
            choose_identity(sets, identity, SelfType::Program(ty), scope, &mut choice)
        {
            choice.bound = Some(Arc::new(bound));
            return choice;
        }
    }

    choice
}

/// Chooses the view for an interpreter's objects of a class whose lineage
/// is `lineage`: the class's identity, then each class it derives from, in
/// the order the language looks attributes up in. The first identity a
/// view of the language binds for presents the objects, as an enum's
/// members are presented by the view of the enum class they derive from.
/// The view binds once for every object of the class, since `self` is a
/// value of a dynamic type.
pub fn choose_dynamic<S: Scope>(
    sets: &[&ViewSet],
    lineage: &[crate::TypeIdentity],
    scope: &S,
) -> Choice<S::Step> {
    let mut choice = Choice::default();

    for identity in lineage {
        if let Some(bound) = choose_identity(sets, identity, SelfType::Dynamic, scope, &mut choice)
        {
            choice.bound = Some(Arc::new(bound));
            return choice;
        }
    }

    choice
}

/// Binds the views whose patterns name `identity`, in the sets' order,
/// against `self_type`, adding each one tried to `choice`'s candidates:
/// the first that binds, with every `extend` that adds to it, or `None`.
fn choose_identity<S: Scope>(
    sets: &[&ViewSet],
    identity: &crate::TypeIdentity,
    self_type: SelfType,
    scope: &S,
    choice: &mut Choice<S::Step>,
) -> Option<BoundView<S::Step>> {
    let mut candidates = Vec::new();

    for views in sets {
        // A Go type is also named by its kind, as every map is by `map`.
        let mut found = views
            .by_base
            .get(identity.base.as_ref())
            .into_iter()
            .flatten()
            .chain(
                pattern::go_kind_word(identity)
                    .and_then(|kind| views.by_base.get(kind))
                    .into_iter()
                    .flatten(),
            )
            .copied()
            .collect::<Vec<_>>();
        found.sort_unstable();
        found.dedup();
        candidates.extend(found.into_iter().map(|index| (&views.views[index], *views)));
    }

    let mut base = None;
    let mut extensions = Vec::new();

    for (view, set) in candidates {
        // Views after the one that binds are not tried; `extend`s are.
        if base.is_some() && !view.extend {
            continue;
        }

        if !pattern::language_matches(view.language, identity.language) {
            continue;
        }

        let Some(captures) = pattern::matches(&view.pattern, identity, scope) else {
            continue;
        };

        match bind::bind(view, set, self_type, &captures, scope) {
            Ok(bound) if view.extend => extensions.push((view, set, captures, bound)),
            Ok(bound) => {
                choice.candidates.push(Candidate {
                    name: name_of(view),
                    rejection: None,
                });
                base = Some(bound);
            }
            Err(rejection) => choice.candidates.push(Candidate {
                name: name_of(view),
                rejection: Some(rejection),
            }),
        }
    }

    // `extend`s with no view to add to add to the value's members.
    if base.is_none()
        && let Some((view, set, captures, _)) = extensions.first()
    {
        let members = Arc::new(View {
            extend: false,
            statements: Vec::new(),
            ..(***view).clone()
        });
        match bind::bind(&members, set, self_type, captures, scope) {
            Ok(bound) => base = Some(bound),
            Err(rejection) => choice.candidates.push(Candidate {
                name: name_of(view),
                rejection: Some(rejection),
            }),
        }
    }

    let mut base = base?;

    for (view, _, _, extension) in extensions {
        let rejection = bind::check_extension(&base, &extension, self_type, scope).err();
        if rejection.is_none() {
            base.extensions.push(Arc::new(extension));
        }

        choice.candidates.push(Candidate {
            name: name_of(view),
            rejection,
        });
    }

    Some(base)
}

/// A type and the types its typedefs and qualifiers stand for, outermost
/// first, ending at its representation.
fn wrappers(types: &dyn TypeSource, ty: TypeReference) -> Vec<(TypeReference, &TypeInfo)> {
    let mut chain = Vec::new();
    let mut current = ty;

    while chain.len() < 64 {
        let Some(info) = types.type_info(current) else {
            break;
        };

        let next = match &info.kind {
            crate::TypeKind::Modified { target, .. }
            | crate::TypeKind::Named {
                target: Some(target),
                ..
            } => Some(*target),
            _ => None,
        };

        chain.push((current, info));
        match next {
            Some(next) => current = next,
            None => break,
        }
    }

    chain
}
