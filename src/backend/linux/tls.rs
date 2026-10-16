//! Thread-local storage addresses, found the way the inferior's C library
//! lays its threads out.
//!
//! Debug information locates a TLS variable by its offset in its module's
//! TLS block, of which each thread has its own copy. Which copy a thread
//! uses is recorded in C library structures. glibc describes the layout of
//! its structures in the inferior itself ([`glibc`]); musl's are fixed ABI
//! ([`musl`]).
//!
//! On x86-64 both C libraries keep a thread's control block at its thread
//! pointer, and the psABI requires the block's first word to point to itself.
//! The second word of both points at the thread's dynamic thread vector
//! (DTV), which holds the address of each module's block.

use std::collections::BTreeMap;
use std::io::IoSliceMut;
use std::num::NonZeroU64;
use std::sync::Arc;

use nix::libc;
use nix::sys::ptrace;
use nix::sys::uio::{RemoteIoVec, process_vm_readv};
use nix::unistd::Pid;
use object::read::elf::ProgramHeader as _;
use object::{Object, ObjectSymbol, SymbolKind};

use super::{mapped_module_load_bias, module_mappings};
use crate::{ImageAddress, LoadBias, VirtualAddress};

mod glibc;
mod musl;

/// The prefix of the file name of musl's dynamic loader, which is also its
/// C library: `ld-musl-x86_64.so.1` on x86-64.
const MUSL_LOADER_PREFIX: &[u8] = b"ld-musl-";

/// The musl function that lays out a new thread's TLS, which a statically
/// linked musl program always contains.
const MUSL_COPY_TLS: &str = "__copy_tls";

/// The glibc function that lays out the TLS of a statically linked program,
/// which such a program always contains.
const GLIBC_SETUP_TLS: &str = "__libc_setup_tls";

/// Where the thread control block keeps its DTV's address, after its own.
const DTV_POINTER_OFFSET: u64 = 8;

/// The C library whose structures locate a program's TLS blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::backend) enum CLibrary {
    /// Dynamically linked glibc, or any C library not recognized as another,
    /// whose lookups then fail for want of glibc's descriptors.
    Glibc,

    /// glibc linked into the executable.
    StaticGlibc,
    Musl,
}

impl CLibrary {
    /// Recognizes the C library from the executable: a dynamically linked
    /// musl program names musl's loader as its interpreter, and a statically
    /// linked program contains its C library's TLS layout function.
    pub(in crate::backend) fn of_executable(data: &[u8]) -> Self {
        let Ok(object) = object::File::parse(data) else {
            return Self::Glibc;
        };

        let interpreter = match &object {
            object::File::Elf64(file) => {
                let endian = file.endian();
                file.elf_program_headers()
                    .iter()
                    .find_map(|segment| segment.interpreter(endian, data).ok().flatten())
            }
            _ => return Self::Glibc,
        };

        let musl_loader = |path: &[u8]| {
            path.rsplit(|&byte| byte == b'/')
                .next()
                .is_some_and(|name| name.starts_with(MUSL_LOADER_PREFIX))
        };

        match interpreter {
            Some(path) if musl_loader(path) => Self::Musl,
            None if object.symbol_by_name(MUSL_COPY_TLS).is_some() => Self::Musl,
            None if object.symbol_by_name(GLIBC_SETUP_TLS).is_some() => Self::StaticGlibc,
            _ => Self::Glibc,
        }
    }

    /// Names each module's TLS block, by load bias, as this C library does.
    /// `link_maps` are the loader's modules in its list order, with their
    /// load biases; `has_tls` says which loaded modules have TLS, and lacks
    /// those the debugger could not load.
    pub(super) fn tls_modules(
        self,
        link_maps: &[(LoadBias, VirtualAddress)],
        main_load_bias: LoadBias,
        has_tls: &BTreeMap<LoadBias, bool>,
    ) -> BTreeMap<LoadBias, TlsModule> {
        match self {
            Self::Glibc => link_maps
                .iter()
                .map(|&(load_bias, link_map)| (load_bias, TlsModule::Glibc { link_map }))
                .collect(),
            // glibc numbers the executable module 1. A library the program
            // loads stays unidentified: it binds to a second copy of glibc's
            // loader, mapped with it, whose TLS state the debugger does not
            // read.
            Self::StaticGlibc => has_tls
                .get(&main_load_bias)
                .copied()
                .unwrap_or(false)
                .then_some((main_load_bias, TlsModule::StaticGlibc))
                .into_iter()
                .collect(),
            Self::Musl => {
                // musl numbers the modules with TLS from one, in load order,
                // and never unloads one. The executable heads the list, so it
                // is first even before the loader publishes the list, and is
                // the only module of a statically linked program.
                let order = if link_maps.is_empty() {
                    vec![main_load_bias]
                } else {
                    link_maps.iter().map(|&(load_bias, _)| load_bias).collect()
                };

                let mut modules = BTreeMap::new();
                let mut id = NonZeroU64::MIN;
                for load_bias in order {
                    // Whether a module the debugger could not load has TLS is
                    // unknown, and so is every later module's number.
                    match has_tls.get(&load_bias) {
                        None => break,
                        Some(false) => {}
                        Some(true) => {
                            modules.insert(load_bias, TlsModule::Musl { id });
                            let Some(next) = id.checked_add(1) else {
                                break;
                            };

                            id = next;
                        }
                    }
                }

                modules
            }
        }
    }
}

/// How the C library identifies one module's TLS block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TlsModule {
    /// glibc finds a block through the module's loader `link_map`.
    Glibc { link_map: VirtualAddress },

    /// The executable of a statically linked glibc program, whose block is
    /// in every thread's DTV slot 1.
    StaticGlibc,

    /// musl indexes each thread's DTV by module number.
    Musl { id: NonZeroU64 },
}

/// The address of `offset` within `module`'s TLS block for `thread`.
pub(super) fn tls_address(
    services: &dyn ProcessServices,
    thread: Pid,
    module: TlsModule,
    offset: u64,
) -> Result<VirtualAddress, Arc<str>> {
    let address = match module {
        TlsModule::Glibc { link_map } => {
            glibc::tls_address(services, thread, link_map.get(), offset)
        }
        TlsModule::StaticGlibc => glibc::static_executable_tls_address(services, thread, offset),
        TlsModule::Musl { id } => musl::tls_address(services, thread, id, offset),
    };

    address
        .map(VirtualAddress::new)
        .map_err(|error| error.to_string().into())
}

/// Locates glibc TLS blocks in arbitrary memory for the fuzz harness.
#[cfg(feature = "fuzzing")]
pub(super) fn fuzz_glibc(data: &[u8]) {
    glibc::fuzz(data);
}

/// Each thread's id and thread pointer in a process that uses glibc,
/// which may be running: glibc's lists of its threads, read from memory
/// without stopping it. A thread whose control block does not point to
/// itself, as one being made or freed may not, is left out.
pub(in crate::backend) fn running_thread_pointers(pid: Pid) -> Result<Vec<(u64, u64)>, String> {
    let process = LiveProcess { pid };
    let threads = glibc::thread_pointers(&process).map_err(|error| error.to_string())?;
    Ok(threads
        .into_iter()
        .filter(|&(_, pointer)| read_word(&process, Some(pointer)) == Ok(pointer))
        .collect())
}

/// The thread pointer of `thread`, which addresses its control block once
/// the C library sets it up.
fn thread_pointer(services: &dyn ProcessServices, thread: Pid) -> Result<u64, TlsError> {
    let thread_pointer = services
        .registers(thread)
        .ok_or_else(|| failed(format!("the registers of thread {thread} are unavailable")))?
        .fs_base;
    if thread_pointer == 0 {
        return Err(TlsError::Deferred);
    }

    if read_word(services, Some(thread_pointer))? != thread_pointer {
        return Err(failed(format!(
            "the thread pointer {thread_pointer:#x} does not address a thread control block"
        )));
    }

    Ok(thread_pointer)
}

/// The address of the DTV of the thread whose control block is at
/// `thread_pointer`.
fn dtv(services: &dyn ProcessServices, thread_pointer: u64) -> Result<u64, TlsError> {
    read_word(services, thread_pointer.checked_add(DTV_POINTER_OFFSET))
}

/// Reads a word of the C library's thread state at `address`, which is
/// `None` when computing it overflowed.
fn read_word(services: &dyn ProcessServices, address: Option<u64>) -> Result<u64, TlsError> {
    let address = address.ok_or_else(|| failed("the C library's thread state overflows"))?;
    let mut bytes = [0; 8];
    if services.read(address, &mut bytes) {
        Ok(u64::from_le_bytes(bytes))
    } else {
        Err(failed(format!(
            "the C library's thread state at {address:#x} is unreadable"
        )))
    }
}

/// Read-only process state that TLS lookups query.
pub(super) trait ProcessServices {
    fn read(&self, address: u64, output: &mut [u8]) -> bool;

    fn registers(&self, lwp: Pid) -> Option<libc::user_regs_struct>;

    /// The address of `symbol`, preferring its definition in `object`, a
    /// prefix of a module's file name, and otherwise taking it from any
    /// module.
    fn lookup_symbol(&self, object: Option<&str>, symbol: &str) -> Option<u64>;
}

/// A live process read through `process_vm_readv`, ptrace, and `/proc`.
pub(super) struct LiveProcess {
    pub(super) pid: Pid,
}

impl ProcessServices for LiveProcess {
    fn read(&self, address: u64, output: &mut [u8]) -> bool {
        usize::try_from(address).is_ok_and(|address| read_process(self.pid, address, output))
    }

    fn registers(&self, lwp: Pid) -> Option<libc::user_regs_struct> {
        ptrace::getregs(lwp).ok()
    }

    fn lookup_symbol(&self, object: Option<&str>, symbol: &str) -> Option<u64> {
        lookup_symbol(self.pid, object, symbol)
    }
}

fn read_process(pid: Pid, address: usize, output: &mut [u8]) -> bool {
    let size = output.len();
    let mut local = [IoSliceMut::new(output)];
    let remote = [RemoteIoVec {
        base: address,
        len: size,
    }];
    process_vm_readv(pid, &mut local, &remote).is_ok_and(|read| read == size)
}

fn lookup_symbol(pid: Pid, requested_object: Option<&str>, requested_symbol: &str) -> Option<u64> {
    let mappings = module_mappings(pid).ok()?;
    let mut fallback = None;
    for mapping in mappings {
        let Some(file_name) = mapping.path.file_name() else {
            continue;
        };

        let file_name = file_name.to_string_lossy();
        let preferred = requested_object.is_some_and(|object| file_name.starts_with(object));
        let Ok(bias) = mapped_module_load_bias(&mapping) else {
            continue;
        };

        let Ok(data) = std::fs::read(&mapping.path) else {
            continue;
        };

        let Ok(object) = object::File::parse(data.as_slice()) else {
            continue;
        };

        // Undefined symbols have no address here, and a TLS symbol's value
        // is an offset rather than an address.
        let Some(symbol) = object
            .dynamic_symbols()
            .chain(object.symbols())
            .find(|symbol| {
                symbol.is_definition()
                    && symbol.kind() != SymbolKind::Tls
                    && symbol.name().ok() == Some(requested_symbol)
            })
        else {
            continue;
        };

        let Some(address) = bias.load(ImageAddress::new(symbol.address())) else {
            continue;
        };

        let address = address.get();
        if preferred {
            return Some(address);
        }

        fallback.get_or_insert(address);
    }

    fallback
}

/// Why a TLS address could not be computed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum TlsError {
    /// The thread has not allocated the module's block, so it cannot have
    /// observed any value in it.
    Deferred,

    /// The module has no TLS block.
    NoTls,
    Failed(String),
}

impl std::fmt::Display for TlsError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Deferred => {
                formatter.write_str("the thread has not allocated the module's TLS block")
            }
            Self::NoTls => formatter.write_str("the module has no TLS block"),
            Self::Failed(message) => formatter.write_str(message),
        }
    }
}

fn failed(message: impl Into<String>) -> TlsError {
    TlsError::Failed(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn musl(id: u64) -> TlsModule {
        TlsModule::Musl {
            id: NonZeroU64::new(id).expect("module numbers start at one"),
        }
    }

    #[test]
    fn each_c_library_identifies_its_modules_tls() {
        let link_maps = [
            LoadBias::new(0x1000),
            LoadBias::new(0x2000),
            LoadBias::new(0x3000),
            LoadBias::new(0x4000),
            LoadBias::new(0x5000),
        ]
        .map(|load_bias| (load_bias, VirtualAddress::new(load_bias.get() + 1)));

        // The executable and a library have TLS and the loader none; the
        // fourth module could not be loaded.
        let has_tls = BTreeMap::from([
            (LoadBias::new(0x1000), true),
            (LoadBias::new(0x2000), false),
            (LoadBias::new(0x3000), true),
            (LoadBias::new(0x5000), true),
        ]);
        assert_eq!(
            CLibrary::Musl.tls_modules(&link_maps, LoadBias::new(0x1000), &has_tls),
            BTreeMap::from([
                (LoadBias::new(0x1000), musl(1)),
                (LoadBias::new(0x3000), musl(2))
            ])
        );

        // Without the loader's list, only the executable has a number.
        assert_eq!(
            CLibrary::Musl.tls_modules(&[], LoadBias::new(0x1000), &has_tls),
            BTreeMap::from([(LoadBias::new(0x1000), musl(1))])
        );
        assert_eq!(
            CLibrary::Musl.tls_modules(&link_maps[1..3], LoadBias::new(0x2000), &has_tls),
            BTreeMap::from([(LoadBias::new(0x3000), musl(1))])
        );

        // glibc finds every listed module's block through its link map.
        assert_eq!(
            CLibrary::Glibc.tls_modules(&link_maps[..2], LoadBias::new(0x1000), &has_tls),
            BTreeMap::from([
                (
                    LoadBias::new(0x1000),
                    TlsModule::Glibc {
                        link_map: VirtualAddress::new(0x1001)
                    }
                ),
                (
                    LoadBias::new(0x2000),
                    TlsModule::Glibc {
                        link_map: VirtualAddress::new(0x2001)
                    }
                ),
            ])
        );

        // Statically linked glibc identifies only the executable, if it has TLS.
        assert_eq!(
            CLibrary::StaticGlibc.tls_modules(&link_maps[..3], LoadBias::new(0x1000), &has_tls),
            BTreeMap::from([(LoadBias::new(0x1000), TlsModule::StaticGlibc)])
        );
        assert_eq!(
            CLibrary::StaticGlibc.tls_modules(&[], LoadBias::new(0x2000), &has_tls),
            BTreeMap::new()
        );
    }
}
