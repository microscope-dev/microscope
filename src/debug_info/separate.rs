use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use object::{Object, ObjectSection};

use crate::DebugFileOptions;

const SYSTEM_DEBUG_DIRECTORY: &str = "/usr/lib/debug";

const DEFAULT_DEBUGINFOD_TIMEOUT: Duration = Duration::from_secs(90);

const DOWNLOAD_LIMIT: u64 = 4 << 30;

#[derive(Debug, Clone, Default)]
pub struct DebugFileSearch {
    directories: Vec<PathBuf>,
    servers: Vec<String>,
    cache: Option<PathBuf>,
    timeout: Option<Duration>,
}

pub struct DebugFile {
    pub path: PathBuf,
    pub data: Vec<u8>,
}

#[derive(Default)]
pub struct Search {
    pub found: Option<DebugFile>,

    pub rejected: Option<(DebugFile, String)>,
}

pub enum Supplementary {
    None,
    Found(DebugFile),

    Missing(String),
}

impl DebugFileSearch {
    pub fn new(options: &DebugFileOptions) -> Self {
        let machine = std::env::var_os("NEXTEST_RUN_ID").is_none();
        let mut directories = options.directories.clone();
        if machine {
            if let Some(listed) = std::env::var_os("NIX_DEBUG_INFO_DIRS") {
                directories.extend(std::env::split_paths(&listed));
            }

            directories.push(PathBuf::from(SYSTEM_DEBUG_DIRECTORY));
        }

        let mut seen = std::collections::BTreeSet::new();
        directories.retain(|directory| {
            !directory.as_os_str().is_empty() && seen.insert(directory.clone())
        });
        let servers = if options.debuginfod {
            options.debuginfod_urls.clone().unwrap_or_else(|| {
                std::env::var("DEBUGINFOD_URLS")
                    .ok()
                    .filter(|_| machine)
                    .map(|urls| urls.split_whitespace().map(str::to_owned).collect())
                    .unwrap_or_default()
            })
        } else {
            Vec::new()
        };

        let timeout = std::env::var("DEBUGINFOD_TIMEOUT")
            .ok()
            .and_then(|seconds| seconds.trim().parse().ok())
            .map_or(DEFAULT_DEBUGINFOD_TIMEOUT, Duration::from_secs);

        Self {
            directories,
            servers,
            cache: options
                .debuginfod_cache
                .clone()
                .or_else(|| default_cache().filter(|_| machine)),
            timeout: Some(timeout),
        }
    }

    pub fn find(&self, path: &Path, object: &object::File<'_>) -> Search {
        if has_dwarf(object) {
            return Search::default();
        }

        let build_id = object.build_id().ok().flatten().filter(|id| id.len() > 1);
        let describes = |data: &[u8]| mismatch(object, data, build_id, None).is_none();
        let rejected = std::cell::RefCell::new(None);

        let consider = |candidate: &Path, checksum: Option<u32>| {
            let data = crate::image::backing::read_named(candidate).ok()?;
            let file = DebugFile {
                path: candidate.to_path_buf(),
                data,
            };

            match mismatch(object, &file.data, build_id, checksum) {
                None => Some(file),
                Some(reason) => {
                    rejected.borrow_mut().get_or_insert((file, reason));
                    None
                }
            }
        };

        let found = self.find_named(path, object, build_id, consider);
        let found = found.or_else(|| build_id.and_then(|id| self.download(id, describes)));

        Search {
            rejected: found.is_none().then(|| rejected.take()).flatten(),
            found,
        }
    }

    pub fn with_build_id(&self, build_id: &[u8]) -> Option<DebugFile> {
        if build_id.len() < 2 {
            return None;
        }

        let describes = |data: &[u8]| {
            object::File::parse(data)
                .ok()
                .and_then(|file| file.build_id().ok().flatten().map(<[u8]>::to_vec))
                .as_deref()
                == Some(build_id)
        };

        self.by_build_id(build_id, describes)
            .or_else(|| self.download(build_id, describes))
    }

    fn find_named(
        &self,
        path: &Path,
        object: &object::File<'_>,
        build_id: Option<&[u8]>,
        consider: impl Fn(&Path, Option<u32>) -> Option<DebugFile>,
    ) -> Option<DebugFile> {
        if let Some(id) = build_id {
            let hex = hex(id);
            let found = self.directories.iter().find_map(|directory| {
                consider(
                    &directory
                        .join(".build-id")
                        .join(&hex[..2])
                        .join(format!("{}.debug", &hex[2..])),
                    None,
                )
            });
            if found.is_some() {
                return found;
            }
        }

        if let Ok(Some((name, checksum))) = object.gnu_debuglink() {
            let name = Path::new(OsStr::from_bytes(name));

            if name.file_name() == Some(name.as_os_str())
                && let Some(directory) = path.parent()
            {
                let mut candidates =
                    vec![directory.join(name), directory.join(".debug").join(name)];
                let relative = directory.strip_prefix("/").unwrap_or(directory);

                candidates.extend(
                    self.directories
                        .iter()
                        .map(|root| root.join(relative).join(name)),
                );
                for candidate in candidates {
                    if candidate == path {
                        continue;
                    }

                    if let Some(file) = consider(&candidate, Some(checksum)) {
                        return Some(file);
                    }
                }
            }
        }

        None
    }

    pub fn supplementary(&self, path: &Path, object: &object::File<'_>) -> Supplementary {
        let link = match supplementary_link(object) {
            Ok(Some(link)) => link,
            Ok(None) => return Supplementary::None,
            Err(reason) => return Supplementary::Missing(reason),
        };

        let name = Path::new(OsStr::from_bytes(&link.name));
        let build_id = link.id.as_slice();
        let named = if link.standard {
            format!(
                "its supplementary file {} (checksum {})",
                name.display(),
                hex(build_id)
            )
        } else {
            format!(
                "its dwz supplementary file {} (build-id {})",
                name.display(),
                hex(build_id)
            )
        };

        if build_id.len() < 2 {
            return Supplementary::Missing(format!("{named} has no identifier to check"));
        }

        let describes = |data: &[u8]| supplementary_file_of(object, data, &link);
        let named_path = if name.as_os_str().is_empty() {
            None
        } else if name.is_absolute() {
            Some(name.to_path_buf())
        } else {
            fs::canonicalize(path)
                .ok()
                .and_then(|path| Some(path.parent()?.join(name)))
        };

        let found = named_path
            .and_then(|path| read_if(&path, describes))
            .or_else(|| self.by_build_id(build_id, describes))
            .or_else(|| {
                let within = name.to_str()?.split_once("/.dwz/")?.1;
                self.directories
                    .iter()
                    .find_map(|directory| read_if(&directory.join(".dwz").join(within), describes))
            })
            .or_else(|| self.download(build_id, describes));

        found.map_or_else(
            || Supplementary::Missing(format!("{named} was not found")),
            Supplementary::Found,
        )
    }

    fn by_build_id(
        &self,
        build_id: &[u8],
        describes: impl Fn(&[u8]) -> bool + Copy,
    ) -> Option<DebugFile> {
        let hex = hex(build_id);
        self.directories.iter().find_map(|directory| {
            let candidate = directory
                .join(".build-id")
                .join(&hex[..2])
                .join(format!("{}.debug", &hex[2..]));
            read_if(&candidate, describes)
        })
    }

    fn download(
        &self,
        build_id: &[u8],
        describes: impl Fn(&[u8]) -> bool + Copy,
    ) -> Option<DebugFile> {
        if self.servers.is_empty() {
            return None;
        }

        let hex = hex(build_id);
        let cached = self
            .cache
            .as_ref()
            .map(|cache| cache.join(&hex).join("debuginfo"));
        if let Some(file) = cached
            .as_deref()
            .and_then(|cached| read_if(cached, describes))
        {
            return Some(file);
        }

        let agent = ureq::Agent::config_builder()
            .timeout_global(self.timeout)
            .build()
            .new_agent();

        for server in &self.servers {
            let url = format!("{}/buildid/{hex}/debuginfo", server.trim_end_matches('/'));
            let Ok(mut response) = agent.get(&url).call() else {
                continue;
            };

            let Ok(data) = response
                .body_mut()
                .with_config()
                .limit(DOWNLOAD_LIMIT)
                .read_to_vec()
            else {
                continue;
            };

            if !describes(&data) {
                continue;
            }

            let path = cached
                .as_deref()
                .filter(|cached| keep(cached, &data))
                .map_or_else(|| PathBuf::from(&url), Path::to_path_buf);

            return Some(DebugFile { path, data });
        }

        None
    }
}

fn default_cache() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("DEBUGINFOD_CACHE_PATH") {
        return Some(PathBuf::from(path));
    }

    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|base| base.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))?;

    Some(base.join("debuginfod_client"))
}

fn keep(path: &Path, data: &[u8]) -> bool {
    let Some(directory) = path.parent() else {
        return false;
    };

    let partial = directory.join(format!(".debuginfo.{}.partial", std::process::id()));
    let kept = fs::create_dir_all(directory)
        .and_then(|()| fs::write(&partial, data))
        .and_then(|()| fs::rename(&partial, path));
    if kept.is_err() {
        let _ = fs::remove_file(&partial);
    }

    kept.is_ok()
}

fn read_if(path: &Path, describes: impl Fn(&[u8]) -> bool) -> Option<DebugFile> {
    let data = crate::image::backing::read_named(path).ok()?;
    describes(&data).then(|| DebugFile {
        path: path.to_path_buf(),
        data,
    })
}

fn mismatch(
    object: &object::File<'_>,
    data: &[u8],
    build_id: Option<&[u8]>,
    checksum: Option<u32>,
) -> Option<String> {
    if let Some(checksum) = checksum {
        let actual = crc32(data);
        if actual != checksum {
            return Some(format!(
                "its CRC-32 is {actual:#010x}, not the {checksum:#010x} the module's debug link \
                 records: it is from another build, or has changed since"
            ));
        }
    }

    let Ok(debug) = object::File::parse(data) else {
        return Some("it is not an ELF file".to_owned());
    };

    if debug.architecture() != object.architecture() {
        return Some("it describes another architecture's code".to_owned());
    }

    let actual = debug.build_id().ok().flatten();

    match build_id {
        Some(id) if checksum.is_none() && actual != Some(id) => Some(format!(
            "its build-id is {}, not the module's {}, so it is from another build",
            actual.map_or_else(|| "missing".to_owned(), hex),
            hex(id)
        )),
        _ => None,
    }
}

struct SupplementaryLink {
    name: Vec<u8>,
    id: Vec<u8>,

    standard: bool,
}

fn supplementary_link(object: &object::File<'_>) -> Result<Option<SupplementaryLink>, String> {
    if let Some((name, id)) = object
        .gnu_debugaltlink()
        .map_err(|error| error.to_string())?
    {
        return Ok(Some(SupplementaryLink {
            name: name.to_vec(),
            id: id.to_vec(),
            standard: false,
        }));
    }

    Ok(debug_sup(object)?
        .filter(|sup| !sup.supplementary)
        .map(|sup| SupplementaryLink {
            name: sup.name,
            id: sup.checksum,
            standard: true,
        }))
}

struct DebugSup {
    supplementary: bool,

    name: Vec<u8>,

    checksum: Vec<u8>,
}

fn debug_sup(object: &object::File<'_>) -> Result<Option<DebugSup>, String> {
    use gimli::Reader as _;

    let Some(section) = object
        .sections()
        .find(|section| section.name_bytes() == Ok(b".debug_sup"))
    else {
        return Ok(None);
    };

    let malformed = |error: &dyn std::fmt::Display| format!("its .debug_sup is malformed: {error}");
    let data = super::compressed::read_one(Some(section), super::compressed::MOST_PER_FILE)
        .map_err(|error| malformed(&error))?;
    let endian = if object.is_little_endian() {
        gimli::RunTimeEndian::Little
    } else {
        gimli::RunTimeEndian::Big
    };

    let mut reader = gimli::EndianSlice::new(&data, endian);
    let read = |reader: &mut gimli::EndianSlice<'_, gimli::RunTimeEndian>| {
        let version = reader.read_u16()?;
        let supplementary = reader.read_u8()?;
        let name = reader.read_null_terminated_slice()?;
        let length = reader.read_uleb128()?;
        let checksum =
            reader.split(usize::try_from(length).map_err(|_| gimli::Error::BadUnsignedLeb128)?)?;
        Ok::<_, gimli::Error>((
            version,
            supplementary,
            name.slice().to_vec(),
            checksum.slice().to_vec(),
        ))
    };

    let (version, supplementary, name, checksum) =
        read(&mut reader).map_err(|error| malformed(&error))?;
    if version != 5 {
        return Err(format!(
            "its .debug_sup has version {version}, which microscope does not read"
        ));
    }

    Ok(Some(DebugSup {
        supplementary: supplementary != 0,
        name,
        checksum,
    }))
}

fn supplementary_file_of(object: &object::File<'_>, data: &[u8], link: &SupplementaryLink) -> bool {
    let Ok(candidate) = object::File::parse(data) else {
        return false;
    };

    if candidate.architecture() != object.architecture() {
        return false;
    }

    let build_id = candidate.build_id().ok().flatten();
    if !link.standard {
        return build_id == Some(link.id.as_slice());
    }

    build_id == Some(link.id.as_slice())
        || debug_sup(&candidate)
            .ok()
            .flatten()
            .is_some_and(|sup| sup.supplementary && sup.checksum == link.id)
}

pub fn has_dwarf(object: &object::File<'_>) -> bool {
    object
        .section_by_name(".debug_info")
        .and_then(|section| section.data().ok())
        .is_some_and(|data| !data.is_empty())
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}

fn crc32(data: &[u8]) -> u32 {
    crc32fast::hash(data)
}
