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

const BUILT_IN_KERNELS: [(&str, &str, &[u8]); 1] = [(
    "rust-btree",
    "views/kernels/rust-btree.zig",
    include_bytes!("../../views/kernels/rust-btree.wasm"),
)];

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

pub const MAX_RENDERER_BYTES: usize = 256 * 1024;

#[derive(Debug)]
pub struct ViewSet {
    views: Vec<Arc<View>>,

    by_base: BTreeMap<String, Vec<usize>>,

    kernels: BTreeMap<String, (Arc<str>, Arc<kernel::Kernel>)>,

    renderers: BTreeMap<String, Arc<crate::Renderer>>,
    errors: Vec<syntax::Error>,
}

impl ViewSet {
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

    #[must_use]
    pub fn renderer(&self, name: &str) -> Option<&Arc<crate::Renderer>> {
        self.renderers.get(name)
    }

    pub fn renderers(&self) -> impl Iterator<Item = &Arc<crate::Renderer>> + '_ {
        self.renderers.values()
    }

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

    #[must_use]
    pub fn kernel(&self, name: &str) -> Option<Arc<kernel::Kernel>> {
        self.kernels.get(name).map(|(_, kernel)| Arc::clone(kernel))
    }

    pub fn kernels(&self) -> impl Iterator<Item = crate::KernelSource> + '_ {
        self.kernels
            .values()
            .map(|(origin, kernel)| crate::KernelSource {
                name: Arc::clone(kernel.name()),
                origin: Arc::clone(origin),
                source: Arc::from(kernel.source()),
            })
    }

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

    #[must_use]
    pub fn empty() -> Arc<Self> {
        static EMPTY: OnceLock<Arc<ViewSet>> = OnceLock::new();
        Arc::clone(EMPTY.get_or_init(|| Arc::new(Self::new([]))))
    }

    #[must_use]
    pub fn views(&self) -> &[Arc<View>] {
        &self.views
    }

    #[must_use]
    pub fn errors(&self) -> &[syntax::Error] {
        &self.errors
    }

    #[must_use]
    pub fn names(&self, identity: &crate::TypeIdentity) -> bool {
        self.by_base.contains_key(identity.base.as_ref())
            || pattern::go_kind_word(identity).is_some_and(|kind| self.by_base.contains_key(kind))
    }
}

#[derive(Debug, Clone)]
pub struct Candidate {
    pub name: Arc<ViewName>,

    pub rejection: Option<Rejection>,
}

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

#[must_use]
pub fn name_of(view: &View) -> Arc<ViewName> {
    Arc::new(ViewName {
        source: Arc::clone(&view.source),
        line: view.line,
        header: Arc::clone(&view.header),
        extend: view.extend,
    })
}

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

#[cfg(any(test, feature = "fuzzing"))]
pub fn choose<S: Scope>(views: &ViewSet, ty: TypeReference, scope: &S) -> Choice<S::Step> {
    choose_among(&[views], ty, scope)
}

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

fn choose_identity<S: Scope>(
    sets: &[&ViewSet],
    identity: &crate::TypeIdentity,
    self_type: SelfType,
    scope: &S,
    choice: &mut Choice<S::Step>,
) -> Option<BoundView<S::Step>> {
    let mut candidates = Vec::new();

    for views in sets {
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
        #[expect(
            clippy::indexing_slicing,
            reason = "`ViewSet::new` makes `by_base` from the indexes of `views`"
        )]
        let found = found.into_iter().map(|index| (&views.views[index], *views));
        candidates.extend(found);
    }

    let mut base = None;
    let mut extensions = Vec::new();

    for (view, set) in candidates {
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
