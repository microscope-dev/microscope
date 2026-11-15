use std::path::{Path, PathBuf};
use std::sync::Arc;

use object::{Object as _, ObjectSymbol as _};
use serde::Deserialize;

use super::facts::{Facts, ProgramFacts};
use super::loader::Image;
use super::markers::{self, Marker};
use crate::debug_info::{self, DebugInfo};

#[must_use]
pub fn directory() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

#[must_use]
pub fn built() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("build/golden")
}

const SIMULATED_ROOT: &str = "/microscope/tests/golden";

#[derive(Debug, thiserror::Error)]
pub enum CorpusError {
    #[error("{path}: {error}")]
    Io {
        path: PathBuf,
        error: std::io::Error,
    },

    #[error("{path}: {error}")]
    Manifest {
        path: PathBuf,
        error: serde_json::Error,
    },

    #[error("{path}: {error}")]
    Load { path: PathBuf, error: String },

    #[error("{path} is missing; build the golden programs with `just build-test-programs`")]
    NotBuilt { path: PathBuf },
}

impl CorpusError {
    fn load(path: &Path, error: impl Into<String>) -> Self {
        Self::Load {
            path: path.to_owned(),
            error: error.into(),
        }
    }
}

#[derive(Deserialize)]
struct Manifest {
    program: String,
    variants: Vec<ManifestVariant>,
    runs: Vec<Run>,
}

#[derive(Deserialize)]
struct ManifestVariant {
    name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Run {
    pub arguments: Vec<String>,
    pub exit_code: i32,
    pub output: String,
}

pub struct Variant {
    pub name: String,

    pub file: PathBuf,

    pub path: Arc<str>,

    pub inode: u64,
    pub data: Arc<[u8]>,
    pub image: Arc<Image>,

    pub functions: Vec<String>,

    pub facts: Facts,

    pub globals: Vec<(String, u64, u64)>,
    debug_info: DebugInfo,
}

impl Variant {
    #[must_use]
    pub fn debug_info(&self) -> DebugInfo {
        DebugInfo {
            image: Arc::clone(&self.debug_info.image),
            unwind: Arc::clone(&self.debug_info.unwind),
            variables: Arc::clone(&self.debug_info.variables),
        }
    }
}

pub struct Program {
    pub name: String,
    pub variants: Vec<Variant>,
    pub runs: Vec<Run>,

    pub functions: Vec<String>,

    pub source: PathBuf,

    pub source_lines: u64,

    pub markers: Vec<Marker>,

    pub views: Option<Arc<str>>,
}

pub struct Corpus {
    pub programs: Vec<Program>,
}

impl Corpus {
    pub fn load() -> Result<Self, CorpusError> {
        let root = directory();
        let built = built();
        let io = |path: &Path| {
            let path = path.to_owned();
            move |error| CorpusError::Io { path, error }
        };

        let read_built = |path: &Path| match std::fs::read(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Err(CorpusError::NotBuilt {
                    path: path.to_owned(),
                })
            }
            result => result.map_err(io(path)),
        };

        let mut manifests = std::fs::read_dir(&root)
            .map_err(io(&root))?
            .filter_map(|entry| Some(entry.ok()?.path().join("manifest.json")))
            .filter(|path| path.is_file())
            .collect::<Vec<_>>();
        manifests.sort();

        let mut programs = Vec::new();
        let mut next_inode = 1;

        for path in manifests {
            let text = std::fs::read_to_string(&path).map_err(io(&path))?;
            let manifest: Manifest =
                serde_json::from_str(&text).map_err(|error| CorpusError::Manifest {
                    path: path.clone(),
                    error,
                })?;
            let facts_path = built.join(&manifest.program).join("facts.json");
            let facts: ProgramFacts =
                serde_json::from_slice(&read_built(&facts_path)?).map_err(|error| {
                    CorpusError::Manifest {
                        path: facts_path.clone(),
                        error,
                    }
                })?;
            let mut facts = facts.variants.into_iter();
            let mut variants = Vec::new();

            for entry in &manifest.variants {
                let name = format!("{}-{}", manifest.program, entry.name);
                let file = built.join(&manifest.program).join(&name);
                let data: Arc<[u8]> = read_built(&file)?.into();
                let simulated = format!("{SIMULATED_ROOT}/{}/{name}", manifest.program);
                let load = |error: String| CorpusError::Load {
                    path: file.clone(),
                    error,
                };

                let image = Image::new(&simulated, next_inode, &data)
                    .map_err(|error| load(error.to_string()))?;
                let debug_info = debug_info::load_bytes(Path::new(&simulated), &data)
                    .map_err(|error| load(error.to_string()))?;
                let functions = defined_functions(&data).map_err(load)?;
                let globals = watchable_objects(&data).map_err(load)?;

                let variant_facts = facts
                    .next()
                    .filter(|facts| facts.name == entry.name)
                    .ok_or_else(|| {
                        CorpusError::load(&facts_path, format!("no facts for {name}"))
                    })?;
                let variant_facts = Facts::new(variant_facts)
                    .map_err(|error| CorpusError::load(&facts_path, error))?;

                variants.push(Variant {
                    name,
                    file,
                    path: Arc::from(simulated),
                    inode: next_inode,
                    data,
                    image: Arc::new(image),
                    functions,
                    facts: variant_facts,
                    globals,
                    debug_info,
                });
                next_inode += 1;
            }

            let functions = variants
                .iter()
                .flat_map(|variant| variant.functions.iter().cloned())
                .collect::<std::collections::BTreeSet<_>>();

            let source_file = root
                .join(&manifest.program)
                .join(format!("{}.c", manifest.program));
            let source = std::fs::read_to_string(&source_file).map_err(io(&source_file))?;
            let source_lines = source.lines().count() as u64;
            let markers =
                markers::parse(&source).map_err(|error| CorpusError::load(&source_file, error))?;

            programs.push(Program {
                views: views_of(&root, &manifest.program)?,
                source: PathBuf::from(format!("{SIMULATED_ROOT}/{0}/{0}.c", manifest.program)),
                source_lines,
                markers,
                functions: functions.into_iter().collect(),
                name: manifest.program,
                variants,
                runs: manifest.runs,
            });
        }

        Ok(Self { programs })
    }
}

fn views_of(root: &Path, name: &str) -> Result<Option<Arc<str>>, CorpusError> {
    let path = root.join(name).join(format!("{name}.views"));
    match std::fs::read_to_string(&path) {
        Ok(text) => Ok(Some(Arc::from(text))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(CorpusError::Io { path, error }),
    }
}

fn watchable_objects(data: &[u8]) -> Result<Vec<(String, u64, u64)>, String> {
    let file = object::File::parse(data).map_err(|error| error.to_string())?;
    let mut objects = file
        .symbols()
        .filter(|symbol| symbol.kind() == object::SymbolKind::Data)
        .filter(|symbol| (1..=4096).contains(&symbol.size()))
        .filter_map(|symbol| {
            let name = symbol.name().ok()?;
            (!name.starts_with('_')).then(|| (name.to_owned(), symbol.address(), symbol.size()))
        })
        .collect::<Vec<_>>();
    objects.sort();
    objects.dedup();
    Ok(objects)
}

fn defined_functions(data: &[u8]) -> Result<Vec<String>, String> {
    let file = object::File::parse(data).map_err(|error| error.to_string())?;
    let mut functions = file
        .symbols()
        .filter(|symbol| symbol.kind() == object::SymbolKind::Text)
        .filter_map(|symbol| symbol.name().ok())
        .filter(|name| !name.starts_with('_'))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    functions.sort();
    functions.dedup();
    Ok(functions)
}
