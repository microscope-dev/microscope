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

pub(super) fn target_thread_id(
    services: &dyn ProcessServices,
    thread: Pid,
    descriptor: u64,
) -> Option<crate::runtime_model::TargetThreadId> {
    crate::runtime_model::TargetThreadId::new(
        glibc::target_thread_id(services, thread, descriptor).ok()?,
    )
}

const MUSL_LOADER_PREFIX: &[u8] = b"ld-musl-";

const MUSL_COPY_TLS: &str = "__copy_tls";

const GLIBC_SETUP_TLS: &str = "__libc_setup_tls";

const DTV_POINTER_OFFSET: u64 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::backend) enum CLibrary {
    Glibc,

    StaticGlibc,
    Musl,
}

impl CLibrary {
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
            Self::StaticGlibc => has_tls
                .get(&main_load_bias)
                .copied()
                .unwrap_or(false)
                .then_some((main_load_bias, TlsModule::StaticGlibc))
                .into_iter()
                .collect(),
            Self::Musl => {
                let order = if link_maps.is_empty() {
                    vec![main_load_bias]
                } else {
                    link_maps.iter().map(|&(load_bias, _)| load_bias).collect()
                };

                let mut modules = BTreeMap::new();
                let mut id = NonZeroU64::MIN;

                for load_bias in order {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TlsModule {
    Glibc { link_map: VirtualAddress },

    StaticGlibc,

    Musl { id: NonZeroU64 },
}

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

pub(in crate::backend) fn running_thread_pointers(pid: Pid) -> Result<Vec<(u64, u64)>, String> {
    let process = LiveProcess { pid };
    let threads = glibc::thread_pointers(&process).map_err(|error| error.to_string())?;
    Ok(threads
        .into_iter()
        .filter(|&(_, pointer)| read_word(&process, Some(pointer)) == Ok(pointer))
        .collect())
}

pub struct RunningTls {
    pid: Pid,
    symbols: std::cell::RefCell<BTreeMap<SymbolKey, Option<u64>>>,
    descriptors: std::cell::RefCell<BTreeMap<u64, [u8; DESCRIPTOR_SIZE]>>,
    link_maps: std::cell::RefCell<BTreeMap<u64, u64>>,
    static_offsets: std::cell::RefCell<BTreeSet<(u64, u64)>>,
}

type SymbolKey = (Option<String>, String);

const MAX_LINK_MAPS: usize = 1 << 12;

const R_MAP: u64 = 8;
const L_ADDR: u64 = 0;
const L_NEXT: u64 = 24;

impl RunningTls {
    #[must_use]
    pub fn new(pid: u32) -> Self {
        Self {
            pid: Pid::from_raw(i32::try_from(pid).unwrap_or(0)),
            symbols: std::cell::RefCell::default(),
            descriptors: std::cell::RefCell::default(),
            link_maps: std::cell::RefCell::default(),
            static_offsets: std::cell::RefCell::default(),
        }
    }

    pub fn address(&self, thread_pointer: u64, bias: LoadBias, offset: u64) -> Result<u64, String> {
        self.try_address(thread_pointer, bias, offset).or_else(|_| {
            self.symbols.borrow_mut().clear();
            self.descriptors.borrow_mut().clear();
            self.link_maps.borrow_mut().clear();
            self.static_offsets.borrow_mut().clear();
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
        let below = glibc::static_offset(&services, link_map).ok().flatten();
        let at = |below: u64| {
            thread_pointer
                .checked_sub(below)
                .and_then(|block| block.checked_add(offset))
        };
        if let Some(below) = below
            && self.static_offsets.borrow().contains(&(link_map, below))
        {
            return at(below).ok_or_else(|| format!("static TLS offset {below:#x} overflows"));
        }

        let address = glibc::tls_address(&services, self.pid, link_map, offset)
            .map_err(|error| error.to_string())?;
        if let Some(below) = below
            && at(below) == Some(address)
        {
            self.static_offsets.borrow_mut().insert((link_map, below));
        }
        Ok(address)
    }

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

    fn read_descriptor(&self, address: u64, output: &mut [u8; DESCRIPTOR_SIZE]) -> bool {
        if let Some(kept) = self.tls.descriptors.borrow().get(&address) {
            *output = *kept;
            return true;
        }
        let read = self.read(address, output);
        if read {
            self.tls.descriptors.borrow_mut().insert(address, *output);
        }
        read
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

pub(super) const DESCRIPTOR_SIZE: usize = 12;

const FS_BASE: usize = 21;

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

fn dtv(services: &dyn ProcessServices, thread_pointer: u64) -> Result<u64, TlsError> {
    read_word(services, thread_pointer.checked_add(DTV_POINTER_OFFSET))
}

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

pub(super) trait ProcessServices {
    fn read(&self, address: u64, output: &mut [u8]) -> bool;

    fn read_descriptor(&self, address: u64, output: &mut [u8; DESCRIPTOR_SIZE]) -> bool {
        self.read(address, output)
    }

    fn registers(&self, lwp: Pid) -> Option<libc::user_regs_struct>;

    fn lookup_symbol(&self, object: Option<&str>, symbol: &str) -> Option<u64>;
}

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum TlsError {
    Deferred,

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

        let refused = tls
            .address(0, LoadBias::new(bias.get() ^ 0x1000), offset)
            .unwrap_err();
        assert!(refused.contains("lists no module"), "{refused}");

        release.send(()).unwrap();
        other.join().unwrap();
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

        assert_eq!(
            CLibrary::Musl.tls_modules(&[], LoadBias::new(0x1000), &has_tls),
            BTreeMap::from([(LoadBias::new(0x1000), musl(1))])
        );
        assert_eq!(
            CLibrary::Musl.tls_modules(&link_maps[1..3], LoadBias::new(0x2000), &has_tls),
            BTreeMap::from([(LoadBias::new(0x3000), musl(1))])
        );

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
