//! Prints every answer a program's debug information gives.
//!
//! The answers are in one canonical text form, so that two loaders'
//! answers can be compared with `diff`. Identifiers are an implementation's own numbering, so the dump
//! names every function, instance, symbol, file, and type by what it is,
//! never by its number, and prints a list in its own order only where the
//! order is itself an answer.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use regex::{Captures, Regex};

use crate::debug_info::{DebugInfo, VariableContext, VariableRuntime, VariableRuntimeError};
use crate::inspection::InspectionBudget;
use crate::unwind::{DwarfRegister, MemoryReader, RegisterFile};
use crate::{
    AddressRange, CodeInstanceKind, ImageAddress, LineNumber, ModuleImage, TypeKind, TypeNode,
    TypeReference, VariableQuery, VariableUnavailableReason, VirtualAddress,
};

/// The parts of a dump, each printed under its own heading.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, clap::ValueEnum)]
pub enum Section {
    Image,
    Symbols,
    Lines,
    Functions,
    Globals,
    Types,
    Addresses,
    Breakpoints,
    Names,
    Variables,
    Unwind,
}

impl Section {
    pub const ALL: [Self; 11] = [
        Self::Image,
        Self::Symbols,
        Self::Lines,
        Self::Functions,
        Self::Globals,
        Self::Types,
        Self::Addresses,
        Self::Breakpoints,
        Self::Names,
        Self::Variables,
        Self::Unwind,
    ];
}

/// What to dump.
#[derive(Debug, Clone)]
pub struct Options {
    pub sections: BTreeSet<Section>,

    /// How many addresses within each code instance to inspect variables
    /// at, besides its entry and its last instruction.
    pub variable_addresses: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            sections: Section::ALL.into_iter().collect(),
            variable_addresses: 3,
        }
    }
}

/// Loads `path` as the debugger loads a module and dumps its answers.
pub fn dump(path: &Path, options: &Options, out: &mut dyn Write) -> anyhow::Result<()> {
    let info = crate::debug_info::load_module(
        path,
        crate::ModuleImageId::new(0),
        &crate::debug_info::DebugFileSearch::default(),
    )?;
    dump_info(&info, options, out)
}

/// Dumps the answers of debug information already loaded.
pub(crate) fn dump_info(
    info: &crate::debug_info::DebugInfo,
    options: &Options,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let names = Names::new(&info.image)?;
    let mut dump = Dumper {
        info: Some(info),
        image: &info.image,
        names: &names,
        out,
    };

    for section in &options.sections {
        match section {
            Section::Image => dump.image()?,
            Section::Symbols => dump.symbols()?,
            Section::Lines => dump.lines()?,
            Section::Functions => dump.functions()?,
            Section::Globals => dump.globals()?,
            Section::Types => dump.types()?,
            Section::Addresses => dump.addresses()?,
            Section::Breakpoints => dump.breakpoints()?,
            Section::Names => dump.names()?,
            Section::Variables => dump.variables(options.variable_addresses)?,
            Section::Unwind => dump.unwind()?,
        }
    }

    Ok(())
}

/// Stable names for an image's numbered entities.
struct Names {
    types: Vec<String>,
    functions: Vec<String>,
    instances: Vec<String>,
    symbols: Vec<String>,
    files: Vec<String>,
    globals: Vec<String>,
    sections: Vec<String>,

    /// Line sequences, numbered by their first row.
    sequences: BTreeMap<u64, usize>,
    pattern: Regex,
}

impl Names {
    #[expect(clippy::too_many_lines, reason = "one name for each kind of entity")]
    fn new(image: &ModuleImage) -> anyhow::Result<Self> {
        let files = image
            .source_files()
            .iter()
            .map(|file| file.path.display().to_string())
            .collect::<Vec<_>>();
        let location = |location: &Option<crate::SourceLocation>| {
            location.as_ref().map_or_else(
                || "?".to_owned(),
                |location| {
                    format!(
                        "{}:{}",
                        files.get(location.file.index()).map_or("?", String::as_str),
                        location.line
                    )
                },
            )
        };

        let functions = image
            .functions()
            .map(|function| {
                format!(
                    "{}|{}|{}",
                    function.name(),
                    function.linkage_name().unwrap_or(""),
                    location(&function.declaration())
                )
            })
            .collect::<Vec<_>>();
        let instances = image
            .code_instances()
            .map(|instance| {
                format!(
                    "{}@{}{}",
                    functions
                        .get(instance.function().index())
                        .map_or("?", String::as_str),
                    instance.ranges().next().map_or_else(
                        || "none".to_owned(),
                        |range| format!("{:#x}", range.start.get())
                    ),
                    match instance.kind() {
                        CodeInstanceKind::OutOfLine => "",
                        CodeInstanceKind::Inline { .. } => "/inline",
                    }
                )
            })
            .collect();
        let symbols = image
            .symbols()
            .map(|symbol| format!("{}@{:#x}", symbol.name(), symbol.address().get()))
            .collect();
        let globals = image
            .globals()
            .map(|global| {
                format!(
                    "{}|{}",
                    global.qualified_name,
                    global.linkage_name.as_deref().unwrap_or("")
                )
            })
            .collect();
        let sections = image
            .sections()
            .iter()
            .map(|section| format!("{}@{:#x}", section.name, section.range.start.get()))
            .collect();
        let key_index = Regex::new(r"#\d+")?;
        let nodes = image.types().collect::<Vec<_>>();
        let type_keys = crate::type_identity::TypeIndex::build(
            nodes.first().map(|node| node.reference().image),
            nodes.len(),
            |index| match nodes[index] {
                TypeNode::Resolved(info) => Some(info),
                TypeNode::Malformed { .. } => None,
            },
        );
        let types = nodes
            .iter()
            .map(|node| match node {
                TypeNode::Resolved(info) => {
                    let key = type_keys
                        .key(info.reference)
                        .map_or_else(String::new, |key| {
                            key_index.replace_all(key, "#").into_owned()
                        });
                    format!(
                        "{}:{}:{}:{key}",
                        info.name,
                        kind_tag(&info.kind),
                        info.byte_size
                            .map_or_else(|| "?".to_owned(), |size| size.to_string())
                    )
                }
                TypeNode::Malformed { description, .. } => format!("malformed:{description}"),
            })
            .collect();
        let mut sequences = BTreeMap::new();

        for row in image.statement_rows() {
            let next = sequences.len();
            sequences
                .entry(u64::from(row.sequence.get()))
                .or_insert(next);
        }

        Ok(Self {
            types,
            functions,
            instances,
            symbols,
            files,
            globals,
            sections,
            sequences,
            pattern: Regex::new(
                r"TypeReference \{ image: ModuleImageId\(\d+\), id: TypeId\((\d+)\) \}|(TypeId|FunctionId|CodeInstanceId|SymbolId|SourceFileId|GlobalVariableId|SectionId|LineSequenceId|CallSiteId)\((\d+)\)|\b(id|symbol|section): [A-Za-z]+Id\(\d+\), ",
            )?,
        })
    }

    /// Text with every identifier replaced by what it names.
    fn canon(&self, text: &str) -> String {
        let name = |table: &[String], index: &str| {
            index
                .parse::<usize>()
                .ok()
                .and_then(|index| table.get(index))
                .map_or_else(|| format!("<bad {index}>"), Clone::clone)
        };

        self.pattern
            .replace_all(text, |captures: &Captures<'_>| {
                if let Some(id) = captures.get(1) {
                    return format!("T[{}]", name(&self.types, id.as_str()));
                }

                if captures.get(4).is_some() {
                    // An entity's own number, which its name replaces.
                    return String::new();
                }

                let index = &captures[3];
                match &captures[2] {
                    "TypeId" => format!("T[{}]", name(&self.types, index)),
                    "FunctionId" => format!("F[{}]", name(&self.functions, index)),
                    "CodeInstanceId" => format!("I[{}]", name(&self.instances, index)),
                    "SymbolId" => format!("S[{}]", name(&self.symbols, index)),
                    "SourceFileId" => format!("P[{}]", name(&self.files, index)),
                    "GlobalVariableId" => format!("G[{}]", name(&self.globals, index)),
                    "SectionId" => format!("X[{}]", name(&self.sections, index)),
                    "LineSequenceId" => format!(
                        "Q[{}]",
                        index
                            .parse::<u64>()
                            .ok()
                            .and_then(|sequence| self.sequences.get(&sequence))
                            .map_or_else(|| "?".to_owned(), ToString::to_string)
                    ),
                    _ => "CallSite".to_owned(),
                }
            })
            .into_owned()
    }

    fn debug(&self, value: &impl std::fmt::Debug) -> String {
        self.canon(&format!("{value:?}"))
    }

    fn ty(&self, reference: TypeReference) -> &str {
        self.types
            .get(reference.id.index())
            .map_or("<bad type>", String::as_str)
    }
}

/// The variant of a type's kind, without its fields.
const fn kind_tag(kind: &TypeKind) -> &'static str {
    match kind {
        TypeKind::Base(_) => "base",
        TypeKind::Enumeration { .. } => "enum",
        TypeKind::Pointer { .. } => "pointer",
        TypeKind::Reference { .. } => "reference",
        TypeKind::Array { .. } => "array",
        TypeKind::RuntimeArray { .. } => "runtime array",
        TypeKind::Slice { .. } => "slice",
        TypeKind::Record { .. } => "record",
        TypeKind::Union { .. } => "union",
        TypeKind::Variant { .. } => "variant",
        TypeKind::Modified { .. } => "modified",
        TypeKind::Named { .. } => "named",
        TypeKind::Unspecified => "unspecified",
        TypeKind::Function => "function",
        TypeKind::Signature { .. } => "signature",
        TypeKind::Opaque { .. } => "opaque",
    }
}

struct Dumper<'a> {
    /// The loaded module, which the variables and unwind sections read.
    info: Option<&'a DebugInfo>,
    image: &'a ModuleImage,
    names: &'a Names,
    out: &'a mut dyn Write,
}

const FIRST_LINE: LineNumber = LineNumber::new(1).expect("line one is nonzero");
const LAST_LINE: LineNumber = LineNumber::new(u64::MAX).expect("the largest line is nonzero");

impl<'a> Dumper<'a> {
    fn loaded(&self) -> std::io::Result<&'a DebugInfo> {
        self.info.ok_or_else(|| {
            std::io::Error::other(
                "only a loaded module's sections read its variables and unwinding",
            )
        })
    }

    fn heading(&mut self, name: &str) -> std::io::Result<()> {
        writeln!(self.out, "## {name}")
    }

    fn line(&mut self, text: &str) -> std::io::Result<()> {
        writeln!(self.out, "{text}")
    }

    /// Prints lines whose order is no answer, sorted.
    fn sorted(&mut self, mut lines: Vec<String>) -> std::io::Result<()> {
        lines.sort_unstable();
        for line in lines {
            writeln!(self.out, "{line}")?;
        }

        Ok(())
    }

    fn image(&mut self) -> std::io::Result<()> {
        let image = self.image;
        let names = self.names;
        self.heading("image")?;
        self.line(&format!("target {:?}", image.target()))?;
        self.line(&format!("range {:?}", image.address_range()))?;
        self.line(&format!(
            "thread_local_storage {}",
            image.has_thread_local_storage()
        ))?;
        self.line(&names.debug(&image.symbol_sources()))?;
        self.line(&format!(
            "debug_file {:?}",
            image.separate_debug_file().map(|file| match file {
                crate::DebugFile::Used(path) => format!("used {}", path.display()),
                crate::DebugFile::Unusable { path, reason } =>
                    format!("unusable {}: {reason}", path.display()),
            })
        ))?;
        self.line(&format!(
            "debug_information {:?}",
            image.debug_information()
        ))?;
        for producer in image.producers() {
            self.line(&format!("producer {producer}"))?;
        }

        for error in image.view_errors() {
            self.line(&format!("view error {error:?}"))?;
        }

        self.heading("sections")?;
        for section in image.sections() {
            self.line(&names.debug(section))?;
        }

        self.heading("constants")?;
        for (name, value) in image.constants_for_dump() {
            self.line(&format!("{name} = {value:?}"))?;
        }

        self.heading("thread locals")?;
        for (name, place) in image.thread_locals_for_dump() {
            self.line(&format!("{name} = {place:?}"))?;
        }

        self.heading("vtables")?;
        for (address, ty) in image.vtables_for_dump() {
            self.line(&format!(
                "{:#x} {} class {:?}",
                address.get(),
                names.ty(ty),
                image.vtable_class(address)
            ))?;
        }

        self.heading("go runtime types")?;
        for offset in image.go_runtime_type_offsets_for_dump() {
            let ty = image.go_runtime_type(offset);
            self.line(&format!(
                "{offset:#x} {}",
                ty.map_or("none", |ty| names.ty(ty))
            ))?;
        }

        Ok(())
    }

    fn symbols(&mut self) -> std::io::Result<()> {
        let image = self.image;
        let names = self.names;
        self.heading("symbols")?;
        for symbol in image.symbols() {
            self.line(&names.debug(&symbol))?;
        }

        self.heading("got slots")?;
        for slot in image.got_slots() {
            self.line(&names.debug(slot))?;
        }

        Ok(())
    }

    fn lines(&mut self) -> std::io::Result<()> {
        let image = self.image;
        let names = self.names;
        self.heading("source files")?;
        self.sorted(names.files.clone())?;
        self.heading("statement rows")?;
        for row in image.statement_rows() {
            self.line(&names.debug(&row))?;
        }

        self.heading("line entries")?;
        for entry in image.line_entries() {
            self.line(&names.debug(&entry))?;
        }

        Ok(())
    }

    fn functions(&mut self) -> std::io::Result<()> {
        let image = self.image;
        let names = self.names;
        self.heading("functions")?;
        let mut lines = Vec::new();

        for function in image.functions() {
            let mut text = names.debug(&function);
            let _ = write!(text, " package {:?}", image.function_package(function.id()));
            let mut instances = image
                .instances_for_function(function.id())
                .map(|instance| names.instances[instance.id().index()].clone())
                .collect::<Vec<_>>();
            instances.sort_unstable();
            for instance in instances {
                let _ = write!(text, "\n    {instance}");
            }

            lines.push(text);
        }

        self.sorted(lines)?;
        self.heading("code instances")?;
        let lines = image
            .code_instances()
            .map(|instance| self.instance(instance))
            .collect();
        self.sorted(lines)
    }

    fn instance(&self, instance: crate::CodeInstance<'_>) -> String {
        let names = self.names;
        let mut text = format!(
            "{} {}",
            names.instances[instance.id().index()],
            names.debug(&instance)
        );
        let entries = self
            .image
            .recommended_entries_for_instance(instance.id())
            .collect::<Vec<_>>();
        let _ = write!(text, " entries {entries:?}");

        if let Some(points) = self.image.resume_points(instance.id()) {
            let _ = write!(text, " resume {}", names.debug(&points));
        }

        text
    }

    fn globals(&mut self) -> std::io::Result<()> {
        let image = self.image;
        let names = self.names;
        self.heading("globals")?;
        for global in image.globals() {
            self.line(&names.debug(&global))?;
        }

        Ok(())
    }

    fn types(&mut self) -> std::io::Result<()> {
        let image = self.image;
        let names = self.names;
        self.heading("types")?;
        let mut lines = BTreeSet::new();

        for node in image.types() {
            let text = match node {
                TypeNode::Resolved(info) => format!(
                    "{} = {} {} {}",
                    names.ty(info.reference),
                    names.debug(&info.kind),
                    names.debug(&info.identity),
                    image
                        .coroutine(info.reference.id)
                        .map_or_else(String::new, |coroutine| {
                            let mut functions = image
                                .coroutine_functions(info.reference.id)
                                .iter()
                                .map(|function| names.functions[function.id().index()].clone())
                                .collect::<Vec<_>>();
                            functions.sort_unstable();
                            format!("coroutine {} run by {functions:?}", names.debug(&coroutine))
                        })
                ),
                TypeNode::Malformed { reference, .. } => names.ty(*reference).to_owned(),
            };

            lines.insert(text);
        }

        for line in lines {
            self.line(&line)?;
        }

        self.heading("type queries")?;
        let mut queries = BTreeSet::new();
        let mut bases = BTreeSet::new();
        let mut instances = BTreeSet::new();

        for node in image.types() {
            if let TypeNode::Resolved(info) = node {
                queries.insert(info.name.to_string());
                if let Some(identity) = &info.identity {
                    bases.insert(identity.base.to_string());
                    instances.insert((
                        identity.language,
                        identity
                            .path
                            .iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>(),
                        identity.base.to_string(),
                    ));
                }
            }
        }

        let set = |references: Vec<TypeReference>| {
            references
                .into_iter()
                .map(|reference| names.ty(reference).to_owned())
                .collect::<BTreeSet<_>>()
        };

        for name in queries {
            let matched = set(image.types_named(&name));
            self.line(&format!("named {name:?}: {matched:?}"))?;
        }

        for base in bases {
            let with = set(image.types_with_base(&base));
            self.line(&format!("base {base:?}: {with:?}"))?;
        }

        for (language, path, base) in instances {
            let path = path.iter().map(String::as_str).collect::<Vec<_>>();
            let found = set(image.type_instances(language, &path, &base));
            self.line(&format!(
                "instances {language:?} {path:?} {base:?}: {found:?}"
            ))?;
        }

        Ok(())
    }

    /// Every address an answer may change at, and points inside each
    /// range: row and range boundaries, their last bytes, and midpoints.
    fn addresses_of_interest(&self) -> BTreeSet<u64> {
        let image = self.image;
        let mut addresses = BTreeSet::new();
        let mut range = |range: AddressRange<ImageAddress>| {
            let (start, end) = (range.start.get(), range.end.get());
            addresses.insert(start);
            addresses.insert(end);
            if end > start {
                addresses.insert(end - 1);
                addresses.insert(start + (end - start) / 2);
            }
        };

        for row in image.statement_rows() {
            range(AddressRange {
                start: row.address,
                end: row.address,
            });
        }

        for entry in image.line_entries() {
            range(entry.range);
        }

        for instance in image.code_instances() {
            for each in instance.ranges() {
                range(each);
            }
        }

        for symbol in image.symbols() {
            if let Some(extent) = symbol.extent() {
                range(extent.range);
            }

            if let Some(storage) = symbol.storage() {
                range(storage);
            }
        }

        for section in image.sections() {
            range(section.range);
        }

        addresses
    }

    fn addresses(&mut self) -> std::io::Result<()> {
        let image = self.image;
        let names = self.names;
        self.heading("instruction starts")?;
        for (address, evidence) in image.instruction_starts(image.address_range()) {
            self.line(&format!("{:#x} {evidence:?}", address.get()))?;
        }

        self.heading("addresses")?;
        let mut previous = String::new();
        let mut runtime = Synthetic;

        for address in self.addresses_of_interest() {
            let at = ImageAddress::new(address);

            // A synthetic image, which only tests dump, has no call sites.
            let call_site = self.info.map_or_else(
                || "-".to_owned(),
                |info| {
                    names.debug(
                        &info
                            .variables
                            .call_site(at, &mut runtime, &mut InspectionBudget::default())
                            .map_err(|error| runtime_error(&error)),
                    )
                },
            );
            let location = image.locate(at);
            let description = image.describe(at);
            let symbol = |symbol: Option<&crate::SymbolLocation>| {
                symbol.map_or_else(
                    || "-".to_owned(),
                    |symbol| {
                        format!(
                            "{}/{:?}/{:?}",
                            names.symbols[symbol.symbol.index()],
                            symbol.kind,
                            symbol.provenance
                        )
                    },
                )
            };

            let chain = |chain: &crate::InlineChain| {
                chain
                    .instances
                    .iter()
                    .map(|instance| names.instances[instance.index()].as_str())
                    .collect::<Vec<_>>()
                    .join(" > ")
            };

            // Offsets follow from the address and what contains it, so the
            // answer stays the same from one address to the next until what
            // contains it changes.
            let text = format!(
                "fn {} | physical {} | inline {} | source {} | symbol {} | section {} | data {} | role {:?} | resume {} | boundaries {} | call {}",
                location
                    .function
                    .as_ref()
                    .map_or("-", |function| names.functions[function.id.index()]
                        .as_str()),
                location
                    .physical_instance
                    .map_or("-", |instance| names.instances[instance.index()].as_str()),
                match &location.inline_frames {
                    crate::InlineFrameLookup::None => "-".to_owned(),
                    crate::InlineFrameLookup::Unique(unique) => chain(unique),
                    crate::InlineFrameLookup::Ambiguous(chains) => format!(
                        "ambiguous [{}]",
                        chains.iter().map(chain).collect::<Vec<_>>().join("; ")
                    ),
                },
                location.source.as_ref().map_or_else(
                    || "-".to_owned(),
                    |source| format!(
                        "{}:{}:{}",
                        names.files[source.file.index()],
                        source.line,
                        source.column.map_or(0, crate::ColumnNumber::get)
                    )
                ),
                symbol(location.symbol.as_ref()),
                description
                    .section
                    .as_ref()
                    .map_or("-", |section| names.sections[section.section.index()]
                        .as_str()),
                symbol(description.symbol.as_ref()),
                image.code_role(at),
                image.is_resume_code(at),
                names.debug(&image.control_boundaries_at(at).collect::<Vec<_>>()),
                call_site,
            );

            if text != previous {
                self.line(&format!("{address:#x} {text}"))?;
                previous = text;
            }
        }

        Ok(())
    }

    fn breakpoints(&mut self) -> std::io::Result<()> {
        let image = self.image;
        let names = self.names;
        self.heading("breakpoints")?;
        let mut lines = BTreeMap::<usize, BTreeSet<u64>>::new();

        for row in image.statement_rows() {
            if let Some(location) = &row.location {
                let lines = lines.entry(location.file.index()).or_default();
                let line = location.line.get();
                lines.extend([line.saturating_sub(1).max(1), line, line + 1]);
            }
        }

        let mut output = Vec::new();

        for file in image.source_files() {
            let path = &names.files[file.id.index()];
            let all = image
                .breakpoint_lines(file.id, FIRST_LINE..=LAST_LINE)
                .map(LineNumber::get)
                .collect::<Vec<_>>();
            let mut text = format!(
                "{path} lines {all:?} keeps {}",
                image.keeps_line_breakpoints(file.id)
            );

            for line in lines.get(&file.id.index()).into_iter().flatten() {
                let Some(line) = LineNumber::new(*line) else {
                    return Err(std::io::Error::other("a breakpoint line is zero"));
                };

                let mut addresses = image
                    .statement_addresses(file.id, line)
                    .map(ImageAddress::get)
                    .collect::<Vec<_>>();
                addresses.sort_unstable();
                let _ = write!(
                    text,
                    "\n    {line}: {:?} {:x?} nearest {:?}",
                    image.breakpoint_line(file.id, line).map(LineNumber::get),
                    addresses,
                    image.nearest_statement_lines(file.id, line),
                );
            }

            output.push(text);
        }

        self.sorted(output)
    }

    #[expect(
        clippy::too_many_lines,
        reason = "every kind of name, one after another"
    )]
    fn names(&mut self) -> std::io::Result<()> {
        let image = self.image;
        let names = self.names;
        self.heading("function names")?;
        let mut function_names = BTreeSet::new();
        for function in image.functions() {
            function_names.insert(function.name().to_string());
            if let Some(linkage) = &function.linkage_name() {
                function_names.insert(linkage.to_string());
            }

            if let Some(package) = image.function_package(function.id()) {
                if let Some(local) = function.name().strip_prefix(&format!("{package}.")) {
                    function_names.insert(local.to_owned());
                }

                if let Some((_, last)) = function.name().rsplit_once('.') {
                    function_names.insert(last.to_owned());
                }
            }
        }

        let mut lines = Vec::new();

        for name in &function_names {
            let single = image
                .function_named(name)
                .map(|function| names.functions[function.id().index()].clone())
                .map_err(|error| error.to_string());
            let mut all = image
                .functions_named(name)
                .map(|function| names.functions[function.id().index()].clone())
                .collect::<Vec<_>>();
            all.sort_unstable();
            let located = image
                .functions_located(name, None)
                .map(|found| {
                    let mut found = found
                        .iter()
                        .map(|function| names.functions[function.id().index()].clone())
                        .collect::<Vec<_>>();
                    found.sort_unstable();
                    found
                })
                .map_err(|error| error.to_string());
            lines.push(format!(
                "{name:?}: {single:?} all {all:?} located {located:?}"
            ));
        }

        self.sorted(lines)?;

        self.heading("symbol names")?;
        let mut symbol_names = BTreeSet::new();
        for symbol in image.symbols() {
            symbol_names.insert(symbol.name().to_string());
            symbol_names.insert(symbol.unversioned_name().to_owned());
            if let Some(demangled) = symbol.demangled_name() {
                symbol_names.insert(crate::demangle::last_part(&demangled).to_owned());
            }
        }

        for slot in image.got_slots() {
            if let crate::GotTarget::Import(name) = &slot.target {
                symbol_names.insert(name.to_string());
            }
        }

        let mut lines = Vec::new();

        for name in &symbol_names {
            let single = image
                .symbol_named(name)
                .map(|symbol| names.symbols[symbol.id().index()].clone())
                .map_err(|error| error.to_string());
            let mut exact = image
                .symbols_named(name)
                .map(|symbol| names.symbols[symbol.id().index()].clone())
                .collect::<Vec<_>>();
            exact.sort_unstable();
            let mut answering = image
                .symbols_answering(name)
                .map(|symbol| names.symbols[symbol.id().index()].clone())
                .collect::<Vec<_>>();
            answering.sort_unstable();
            lines.push(format!(
                "{name:?}: {single:?} exact {exact:?} answering {answering:?} imported {}",
                image.imports_function(name)
            ));
        }

        self.sorted(lines)?;

        self.heading("global names")?;
        let mut selectors = BTreeSet::new();

        for global in image.globals() {
            selectors.insert(global.name.to_string());
            selectors.insert(global.qualified_name.to_string());
            if let Some(linkage) = &global.linkage_name {
                selectors.insert(linkage.to_string());
            }
        }

        for selector in selectors {
            let found = image
                .global_named(&selector)
                .map(|global| names.globals[global.id.index()].clone())
                .map_err(|error| names.canon(&error.to_string()));
            self.line(&format!("{selector:?}: {found:?}"))?;
        }

        self.heading("source file names")?;
        let mut lines = Vec::new();

        for file in image.source_files() {
            for query in [
                Some(file.path.as_path()),
                file.path.file_name().map(Path::new),
            ]
            .into_iter()
            .flatten()
            {
                let found = image
                    .source_file_matching(query)
                    .map(|found| names.files[found.id.index()].clone())
                    .map_err(|error| error.to_string());
                lines.push(format!("{}: {found:?}", query.display()));
            }
        }

        lines.dedup();
        self.sorted(lines)
    }

    fn variables(&mut self, per_instance: usize) -> std::io::Result<()> {
        let image = self.image;
        let names = self.names;
        let variables = &self.loaded()?.variables;
        self.heading("variables")?;
        let context = |address| VariableContext {
            stop_id: crate::StopId::new(1),
            context: crate::ThreadId::new(1).into(),
            frame: crate::StackFrameId::new(0),
            module: crate::ModuleId::new(0),
            image: crate::ModuleImageId::new(0),
            address: Some(address),
        };

        let mut lines = Vec::new();
        let statements = image.statement_rows().collect::<Vec<_>>();

        for instance in image.code_instances() {
            let selected = match instance.kind() {
                CodeInstanceKind::OutOfLine => None,
                CodeInstanceKind::Inline { .. } => Some(instance.id()),
            };

            let mut addresses = BTreeSet::new();
            if let Some(entry) = instance.breakpoint_entry() {
                addresses.insert(entry.address.get());
            }

            for range in instance.ranges() {
                if range.start < range.end {
                    addresses.insert(range.start.get());
                    addresses.insert(range.end.get() - 1);
                }
            }

            let rows = instance.ranges().next().map_or(&[][..], |range| {
                let rows = statements.as_slice();
                let first = rows.partition_point(|row| row.address < range.start);
                let rest = &rows[first.min(rows.len())..];
                let count = rest.partition_point(|row| row.address < range.end);
                &rest[..count]
            });
            let mut statement_addresses = rows
                .iter()
                .filter(|row| row.flags.is_statement())
                .map(|row| row.address.get())
                .collect::<Vec<_>>();
            statement_addresses.dedup();
            addresses.extend(statement_addresses.into_iter().take(per_instance));
            let mut text = names.instances[instance.id().index()].clone();
            let mut previous = String::new();

            for address in addresses {
                let at = ImageAddress::new(address);
                let mut runtime = Synthetic;
                let found = variables
                    .inspect(
                        at,
                        selected,
                        &VariableQuery::All,
                        context(at),
                        &mut runtime,
                        &mut InspectionBudget::default(),
                    )
                    .map_err(|error| error.to_string());
                let found = names.debug(&found);
                if found != previous {
                    let _ = write!(text, "\n    {address:#x} {found}");
                    previous = found;
                }
            }

            lines.push(text);
        }

        self.sorted(lines)?;

        self.heading("global values")?;
        for global in image.globals() {
            let mut runtime = Synthetic;
            let found = variables
                .inspect_global(
                    global.id,
                    None,
                    VariableContext {
                        address: None,
                        ..context(ImageAddress::new(0))
                    },
                    &mut runtime,
                    &mut InspectionBudget::default(),
                )
                .map_err(|error| error.to_string());
            self.line(&format!(
                "{} {}",
                names.globals[global.id.index()],
                names.debug(&found)
            ))?;
        }

        Ok(())
    }

    fn unwind(&mut self) -> std::io::Result<()> {
        let image = self.image;
        self.heading("unwind")?;
        let mut addresses = BTreeSet::new();

        for instance in image.code_instances() {
            if matches!(instance.kind(), CodeInstanceKind::OutOfLine) {
                for range in instance.ranges() {
                    if range.start < range.end {
                        addresses.extend([
                            range.start.get(),
                            range.start.get() + 1,
                            range.end.get() - 1,
                        ]);
                    }
                }
            }
        }

        for symbol in image.symbols() {
            if let Some(extent) = symbol.extent() {
                addresses.extend([extent.range.start.get(), extent.range.end.get() - 1]);
            }
        }

        let registers = RegisterFile::new((0..17).map(|register| {
            (
                DwarfRegister::new(register),
                0x7fff_0000 + u64::from(register) * 0x100,
            )
        }));
        let mut previous = String::new();

        for address in addresses {
            let at = ImageAddress::new(address);
            let mut memory = Synthetic;
            let cfa = self.loaded()?.unwind.cfa(at, &registers, &mut memory);
            let step = self
                .loaded()?
                .unwind
                .unwind(at, &registers, &mut memory)
                .map(|step| (step.registers, step.cfa, step.signal_frame));
            let text = format!("{cfa:?} {step:?}");
            if text != previous {
                self.line(&format!("{address:#x} {text}"))?;
                previous = text;
            }
        }

        Ok(())
    }
}

fn runtime_error(error: &VariableRuntimeError) -> String {
    match error {
        VariableRuntimeError::Unavailable(reason) => format!("unavailable {reason:?}"),
        VariableRuntimeError::Malformed(reason) => format!("malformed {reason}"),
        VariableRuntimeError::Fatal(reason) => format!("fatal {reason}"),
    }
}

/// A stopped thread no process backs: registers, memory, and addresses
/// that are fixed functions of their names, so that every load sees the
/// same program state.
struct Synthetic;

/// Where the synthetic process maps its image.
const BIAS: u64 = 0x5555_0000_0000;

const fn pattern(address: u64) -> u8 {
    // Small byte values keep lengths and counts read from memory small.
    ((address.wrapping_mul(0x9e37_79b9) >> 13) & 0x1f).to_le_bytes()[0]
}

impl MemoryReader for Synthetic {
    fn read_u64(&mut self, address: VirtualAddress) -> Option<u64> {
        let mut bytes = [0; 8];
        for (offset, byte) in bytes.iter_mut().enumerate() {
            *byte = pattern(address.get().wrapping_add(offset as u64));
        }

        Some(u64::from_le_bytes(bytes))
    }
}

impl VariableRuntime for Synthetic {
    fn register(
        &mut self,
        register: DwarfRegister,
    ) -> Result<crate::debug_info::VariableRegister, VariableRuntimeError> {
        let register = register.number();

        // General registers are words; the rest, vector registers.
        let bits: u16 = if register < 17 { 64 } else { 128 };
        let value = 0x7fff_0000_u64 + u64::from(register) * 0x100;
        let mut contents = value.to_le_bytes().to_vec();
        contents.resize(usize::from(bits / 8), 0);
        Ok(crate::debug_info::VariableRegister {
            descriptor: crate::RegisterDescriptor {
                id: crate::RegisterId::new(u32::from(register)),
                name: format!("r{register}").into(),
                bits,
                role: None,
            },
            bytes: contents.into(),
        })
    }

    fn call_frame_cfa(&self) -> Result<VirtualAddress, VariableRuntimeError> {
        Ok(VirtualAddress::new(0x7ffe_0000))
    }

    fn tls_address(&mut self, offset: u64) -> Result<VirtualAddress, VariableUnavailableReason> {
        Ok(VirtualAddress::new(0x6000_0000 + offset))
    }

    fn relocate(&self, address: ImageAddress) -> Result<VirtualAddress, Arc<str>> {
        Ok(VirtualAddress::new(address.get() + BIAS))
    }

    fn image_address(&self, address: VirtualAddress) -> Option<ImageAddress> {
        address.get().checked_sub(BIAS).map(ImageAddress::new)
    }

    fn read_memory(
        &mut self,
        address: VirtualAddress,
        size: usize,
    ) -> Result<Arc<[u8]>, VariableRuntimeError> {
        Ok((0..size)
            .map(|offset| pattern(address.get().wrapping_add(offset as u64)))
            .collect())
    }

    fn entry_value(
        &mut self,
        _: crate::debug_info::EntryParameter,
        _: &mut InspectionBudget,
    ) -> Result<u64, VariableRuntimeError> {
        Err(VariableRuntimeError::Malformed(
            "the synthetic process has no caller".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::image::lines::{Files, LineTables, Row};
    use crate::model::ModuleMetadata;
    use crate::{AddressRange, ImageAddress, ModuleImage, SourceFileId};

    /// The line sections' dump of an image with these rows, each line's
    /// range running to the next row.
    fn dumped(rows: &[Row]) -> String {
        let mut files = Files::default();
        files.intern(PathBuf::from("/src/a.c"));
        let mut lines = LineTables::default();
        lines.begin_sequence().unwrap();

        for (index, row) in rows.iter().enumerate() {
            let at = lines.push_row(row).unwrap();
            if let Some(next) = rows.get(index + 1)
                && row.location().is_some()
                && row.address < next.address
            {
                let range = AddressRange {
                    start: ImageAddress::new(row.address),
                    end: ImageAddress::new(next.address),
                };

                lines.push_range(range, at, row.statement).unwrap();
            }
        }

        let image = ModuleImage::new(
            PathBuf::from("/a"),
            crate::TargetDescription::X86_64,
            AddressRange {
                start: ImageAddress::new(0),
                end: ImageAddress::new(0x1000),
            },
            ModuleMetadata {
                files,
                lines,
                ..ModuleMetadata::default()
            },
        );
        let names = super::Names::new(&image).unwrap();
        let mut out = Vec::new();
        let mut dump = super::Dumper {
            info: None,
            image: &image,
            names: &names,
            out: &mut out,
        };

        dump.lines().unwrap();
        dump.addresses().unwrap();
        dump.breakpoints().unwrap();
        String::from_utf8(out).unwrap()
    }

    type Change = (&'static str, fn(&mut Row));

    /// Dumps are compared to find what changed, so a part of a row the
    /// dump left out could change unseen: changing any part of any row
    /// changes it.
    #[test]
    fn the_dump_shows_every_part_of_every_row() {
        let row = |address, line| Row {
            address,
            file: Some(SourceFileId::new(0)),
            line,
            statement: true,
            ..Row::default()
        };

        let rows = [
            row(0x10, 3),
            row(0x14, 4),
            row(0x18, 4),
            Row {
                end_sequence: true,
                ..row(0x20, 4)
            },
        ];
        let base = dumped(&rows);
        let changes: [Change; 10] = [
            ("address", |row| row.address += 1),
            ("file", |row| row.file = None),
            ("line", |row| row.line += 7),
            ("column", |row| row.column = 9),
            ("operation index", |row| row.operation_index = 1),
            ("discriminator", |row| row.discriminator = 2),
            ("isa", |row| row.isa = 3),
            ("statement", |row| row.statement = !row.statement),
            ("prologue end", |row| row.prologue_end = true),
            ("epilogue begin", |row| row.epilogue_begin = true),
        ];

        for (name, change) in changes {
            let mut different = rows;
            change(&mut different[1]);
            assert_ne!(dumped(&different), base, "the dump omits a row's {name}");
        }
    }
}
