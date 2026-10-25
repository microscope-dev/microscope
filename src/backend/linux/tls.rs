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

use std::collections::{BTreeMap, BTreeSet};
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

/// A core's thread notes may use a different PID namespace than the
/// target's runtime. The libc descriptor must have one verified provider.
pub(super) fn target_thread_id(
    services: &dyn ProcessServices,
    thread: Pid,
    descriptor: u64,
) -> Option<crate::runtime_model::TargetThreadId> {
    crate::runtime_model::TargetThreadId::new(
        glibc::target_thread_id(services, thread, descriptor).ok()?,
    )
}

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

/// Finds each module's thread-local storage for the threads of a running
/// process that uses glibc, from its memory alone, without stopping it: the
/// loader's `_r_debug` lists each module's `link_map`, and glibc's own
/// descriptors and a thread's DTV, from its thread pointer, place the
/// module's block, as for a stopped thread.
///
/// What does not change while the process runs is kept: the addresses of
/// the symbols the lookups read, which would take reading every mapped
/// file again, and each module's `link_map`, which is proven again by its
/// load bias at each use. A process that execs maps other files: a
/// lookup that fails forgets what was kept and tries once more.
pub struct RunningTls {
    pid: Pid,
    symbols: std::cell::RefCell<BTreeMap<SymbolKey, Option<u64>>>,
    link_maps: std::cell::RefCell<BTreeMap<u64, u64>>,
}

/// A symbol looked up, by the module preferred and its name.
type SymbolKey = (Option<String>, String);

/// The most modules the loader's list holds before it counts as corrupt.
const MAX_LINK_MAPS: usize = 1 << 12;

/// Where the loader's `r_debug` keeps the head of its list, and each
/// `link_map` its load bias and the next (`<link.h>`).
const R_MAP: u64 = 8;
const L_ADDR: u64 = 0;
const L_NEXT: u64 = 24;

impl RunningTls {
    #[must_use]
    pub fn new(pid: u32) -> Self {
        Self {
            pid: Pid::from_raw(i32::try_from(pid).unwrap_or(0)),
            symbols: std::cell::RefCell::default(),
            link_maps: std::cell::RefCell::default(),
        }
    }

    /// The address of `offset` in the block of the module loaded with
    /// `bias`, for the thread whose thread pointer is `thread_pointer`.
    pub fn address(&self, thread_pointer: u64, bias: LoadBias, offset: u64) -> Result<u64, String> {
        self.try_address(thread_pointer, bias, offset).or_else(|_| {
            self.symbols.borrow_mut().clear();
            self.link_maps.borrow_mut().clear();
            self.try_address(thread_pointer, bias, offset)
        })
    }

    fn try_address(&self, thread_pointer: u64, bias: LoadBias, offset: u64) -> Result<u64, String> {
        if self.pid.as_raw() <= 0 {
            return Err(format!("process {} has no Linux id", self.pid));
        }
        let services = RunningThread {
            tls: self,
            thread_pointer,
        };
        let link_map = self.link_map(&services, bias)?;
        glibc::tls_address(&services, self.pid, link_map, offset).map_err(|error| error.to_string())
    }

    /// The `link_map` of the module loaded with `bias`.
    fn link_map(&self, services: &RunningThread<'_>, bias: LoadBias) -> Result<u64, String> {
        let word =
            |address: u64| read_word(services, Some(address)).map_err(|error| error.to_string());
        if let Some(&kept) = self.link_maps.borrow().get(&bias.get())
            && word(kept.wrapping_add(L_ADDR)) == Ok(bias.get())
        {
            return Ok(kept);
        }

        let r_debug = services
            .lookup_symbol(Some("ld-linux"), "_r_debug")
            .ok_or("the loader names no _r_debug")?;
        let mut map = word(r_debug.wrapping_add(R_MAP))?;
        let mut visited = BTreeSet::new();
        while map != 0 {
            if visited.len() == MAX_LINK_MAPS || !visited.insert(map) {
                return Err("the loader's list of modules does not end".to_owned());
            }
            if word(map.wrapping_add(L_ADDR))? == bias.get() {
                self.link_maps.borrow_mut().insert(bias.get(), map);
                return Ok(map);
            }
            map = word(map.wrapping_add(L_NEXT))?;
        }
        Err(format!(
            "the loader lists no module loaded with the bias {:#x}",
            bias.get()
        ))
    }
}

/// A running thread as glibc's lookups read it: the process's memory, and
/// of the thread's registers its thread pointer alone, which it keeps for
/// life.
struct RunningThread<'a> {
    tls: &'a RunningTls,
    thread_pointer: u64,
}

impl ProcessServices for RunningThread<'_> {
    fn read(&self, address: u64, output: &mut [u8]) -> bool {
        usize::try_from(address).is_ok_and(|address| read_process(self.tls.pid, address, output))
    }

    fn registers(&self, _lwp: Pid) -> Option<libc::user_regs_struct> {
        let mut values = [0; super::core_dump::GENERAL_REGISTER_COUNT];
        values[FS_BASE] = self.thread_pointer;
        Some(super::core_dump::user_registers(values))
    }

    fn lookup_symbol(&self, object: Option<&str>, symbol: &str) -> Option<u64> {
        let key = (object.map(str::to_owned), symbol.to_owned());
        if let Some(&kept) = self.tls.symbols.borrow().get(&key) {
            return kept;
        }
        let found = lookup_symbol(self.tls.pid, object, symbol);
        self.tls.symbols.borrow_mut().insert(key, found);
        found
    }
}

/// Where `fs_base` is in the kernel's `user_regs_struct`.
const FS_BASE: usize = 21;

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

    /// The load bias of this process's C library, and the offset of its
    /// `errno` in its block of thread-local storage.
    fn errno_place() -> (LoadBias, u64) {
        let pid = nix::unistd::getpid();
        let mapping = module_mappings(pid)
            .unwrap()
            .into_iter()
            .find(|mapping| {
                mapping
                    .path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with("libc.so"))
            })
            .expect("this process maps glibc");
        let bias = mapped_module_load_bias(&mapping).unwrap();
        let data = std::fs::read(&mapping.path).unwrap();
        let object = object::File::parse(data.as_slice()).unwrap();
        let errno = object
            .dynamic_symbols()
            .find(|symbol| symbol.kind() == SymbolKind::Tls && symbol.name() == Ok("errno"))
            .expect("glibc exports errno");
        (bias, errno.address())
    }

    /// Each thread's `errno`, a variable of the C library's thread-local
    /// storage, is found from its thread pointer alone while it runs on.
    #[test]
    fn a_running_threads_thread_local_storage_is_found_from_memory() {
        let pid = u32::try_from(nix::unistd::getpid().as_raw()).unwrap();
        let (bias, offset) = errno_place();
        let tls = RunningTls::new(pid);
        let read_errno = |tid: u32| {
            let pointers = running_thread_pointers(Pid::from_raw(pid.cast_signed())).unwrap();
            let (_, pointer) = pointers
                .into_iter()
                .find(|&(listed, _)| listed == u64::from(tid))
                .expect("glibc lists the thread");
            let address = tls.address(pointer, bias, offset).unwrap();
            let mut value = [0; 4];
            assert!(read_process(
                Pid::from_raw(pid.cast_signed()),
                usize::try_from(address).unwrap(),
                &mut value
            ));
            i32::from_le_bytes(value)
        };

        let (ready, parked) = std::sync::mpsc::channel();
        let (release, wait) = std::sync::mpsc::channel::<()>();
        let other = std::thread::spawn(move || {
            nix::errno::Errno::set_raw(4321);
            let tid = u32::try_from(nix::unistd::gettid().as_raw()).unwrap();
            ready.send(tid).unwrap();
            wait.recv().ok();
        });
        let other_tid = parked
            .recv_timeout(std::time::Duration::from_secs(20))
            .unwrap();

        let own = u32::try_from(nix::unistd::gettid().as_raw()).unwrap();
        nix::errno::Errno::set_raw(1234);
        assert_eq!(read_errno(own), 1234);
        assert_eq!(read_errno(other_tid), 4321);
        nix::errno::Errno::set_raw(77);
        assert_eq!(read_errno(own), 77, "the same address read again");

        // A module loaded with no listed bias is refused, not guessed.
        let refused = tls
            .address(0, LoadBias::new(bias.get() ^ 0x1000), offset)
            .unwrap_err();
        assert!(refused.contains("lists no module"), "{refused}");

        release.send(()).unwrap();
        other.join().unwrap();
    }

    #[test]
    fn a_running_threads_registers_hold_only_its_thread_pointer() {
        let tls = RunningTls::new(1);
        let thread = RunningThread {
            tls: &tls,
            thread_pointer: 0x7f00_1234_5000,
        };
        let registers = thread.registers(Pid::from_raw(1)).unwrap();
        assert_eq!(registers.fs_base, 0x7f00_1234_5000);
        assert_eq!((registers.rip, registers.rsp, registers.rbp), (0, 0, 0));
    }

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
