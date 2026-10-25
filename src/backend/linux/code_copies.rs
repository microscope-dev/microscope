//! Copies of a module's code that the process made in anonymous memory.
//!
//! A runtime may copy some of its own machine code into memory no file
//! backs and run it there: V8 copies its embedded builtins next to each of
//! its code ranges, so that calls between them are short. Such memory
//! names no module, so a stack that enters it would end there. A copy that
//! is byte for byte a range of a loaded module's code is that code, at
//! another address: its symbols, roles, and unwind rules hold unchanged.
//! So the copy becomes a module of its own, a second binding of the same
//! image whose window is the range it copies.
//!
//! The proof is a full comparison of the bytes. A candidate is found from
//! where copies begin: a page of anonymous memory that may be executed but
//! not written, whose first bytes are those of a function that begins a
//! page of the module, as code copied whole begins, or of code a runtime
//! says the process keeps in the module, as V8's builtins where no symbol
//! names them. Writable memory, such
//! as a JIT compiler's, is never taken for a copy, since its bytes may
//! change between stops. A proven copy, and memory proven to be none, is
//! remembered until its mapping changes.
//!
//! In a process that runs V8, anonymous code that is no copy is code V8
//! generated, which keeps the frame-pointer chain by its convention; it
//! becomes a module of its own whose frames are unwound by that convention
//! (see [`crate::debug_info::runtime_code`]).

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::sync::Arc;

use nix::unistd::Pid;

use super::memory::read_logical_block;
use super::modules::AnonymousCode;
use super::native::{InspectionOps, LinuxTraceOps};
use super::{BREAKPOINT_OPCODE, Controller, RuntimeModule};
use crate::protocol::DebuggerEvent;
use crate::unwind::code_copies as copies;
use crate::{AddressRange, ImageAddress, LoadBias, LoadedModule, ModuleId, VirtualAddress};

/// How much of a copy is compared at once.
const CHUNK: usize = 256 * 1024;

/// What a mapping of anonymous code was found to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Copy {
    /// A copy of the code of the module `source`, which `module` binds.
    Of { module: ModuleId, source: ModuleId },

    /// Code a runtime generated, which `module` holds.
    Runtime { module: ModuleId },

    /// No copy of any module's code, nor code a runtime is known to make.
    None,
}

/// A range of a module's code that a mapping's bytes begin with.
struct Proven {
    source: ModuleId,
    image_start: ImageAddress,
    length: u64,

    /// The debugger's traps the program copied with the code.
    traps: Vec<CopiedTrap>,
}

/// A trap of the debugger's that the program copied, and the byte the
/// code holds there.
pub(super) struct CopiedTrap {
    address: u64,
    original: u8,
}

impl<P: LinuxTraceOps> Controller<P> {
    /// Brings the modules that bind copies up to date with the process's
    /// mappings of anonymous code, `anonymous`. The debugger's traps a
    /// copy holds come out of it before it becomes a module.
    pub(super) fn observe_code_copies(&mut self, pid: Pid, anonymous: &[AnonymousCode]) {
        self.observe_code_copies_with(pid, anonymous, |controller, traps| {
            for trap in traps {
                let word = controller.ptrace.read_word(pid, trap.address).ok()?;
                let restored = (word & !0xff) | u64::from(trap.original);
                controller
                    .ptrace
                    .write_word(pid, trap.address, restored)
                    .ok()?;
            }
            Some(())
        });
    }

    /// Whether the trap a thread `pid` stopped after is one the program
    /// copied with the code it lies in from a site of the debugger's, into
    /// memory not yet known to hold a copy. If it is, the thread is rewound
    /// to run the code there once the copy is registered.
    pub(super) fn rewind_copied_trap(&self, pid: Pid) -> bool {
        let rewind = || {
            let inferior = self.inferior.as_ref()?;
            let mut registers = self.ptrace.registers(pid).ok()?;
            let address = registers.rip.checked_sub(1)?;
            let memory = inferior.memory_thread();
            let mappings = self.ptrace.module_mappings(memory).ok()?;
            let mapping = &mappings
                .anonymous_code
                .iter()
                .find(|mapping| !mapping.writable && mapping.range.contains(&address))?
                .range;
            if matches!(
                self.code_copies.get(&(mapping.start, mapping.end)),
                Some(Copy::Of { .. })
            ) {
                return None;
            }
            self.prove_copy(memory, mapping, &self.stored_code(memory))?
                .traps
                .iter()
                .any(|trap| trap.address == address)
                .then_some(())?;
            registers.rip = address;
            self.ptrace.set_registers(pid, registers).ok()
        };
        rewind().is_some()
    }

    /// Forgets that the mapping of anonymous code holding `address` held no
    /// copy, so that the next refresh proves it again: the program copied
    /// code there since.
    pub(super) fn forget_copy_at(&mut self, address: u64) {
        let mut known = std::mem::take(&mut self.code_copies);
        known.retain(|mapping, copy| {
            if !(mapping.0..mapping.1).contains(&address) {
                return true;
            }
            match *copy {
                Copy::Of { .. } => true,
                Copy::Runtime { module } => {
                    self.unregister_module(module);
                    false
                }
                Copy::None => false,
            }
        });
        self.code_copies = known;
    }
}

impl<P: InspectionOps> Controller<P> {
    /// Brings the modules that bind copies, and that hold a runtime's code,
    /// up to date with the mappings of anonymous code, `anonymous`: one
    /// whose mapping is gone, or whose source module is, is unregistered.
    /// Each new mapping the process cannot write that is a copy becomes a
    /// module once `scrub` took out the traps it holds; in a process that
    /// runs V8, each other one becomes a module of V8's code.
    pub(super) fn observe_code_copies_with(
        &mut self,
        pid: Pid,
        anonymous: &[AnonymousCode],
        scrub: impl Fn(&Self, &[CopiedTrap]) -> Option<()>,
    ) {
        let mut known = std::mem::take(&mut self.code_copies);
        known.retain(|mapping, copy| {
            let current = anonymous
                .iter()
                .any(|code| code.range == (mapping.0..mapping.1));
            match *copy {
                Copy::Of { module, source } => {
                    let kept = current && self.modules.contains_key(&source);
                    if !kept {
                        self.unregister_module(module);
                    }
                    kept
                }
                Copy::Runtime { module } => {
                    if !current {
                        self.unregister_module(module);
                    }
                    current
                }
                Copy::None => current,
            }
        });

        let runtime = self
            .modules
            .values()
            .any(|module| crate::debug_info::v8::is_v8(&module.image));
        let stored = self.stored_code(pid);
        for code in anonymous {
            let mapping = &code.range;
            if known.contains_key(&(mapping.start, mapping.end)) {
                continue;
            }
            let copy = (!code.writable)
                .then(|| self.prove_copy(pid, mapping, &stored))
                .flatten()
                .and_then(|proven| {
                    // The copied traps come out before the copy's own
                    // sites go in.
                    scrub(self, &proven.traps)?;
                    let module = self.register_copy(mapping, &proven)?;
                    Some(Copy::Of {
                        module,
                        source: proven.source,
                    })
                })
                .or_else(|| {
                    runtime
                        .then(|| self.register_runtime_code(mapping))
                        .flatten()
                        .map(|module| Copy::Runtime { module })
                })
                .unwrap_or(Copy::None);
            known.insert((mapping.start, mapping.end), copy);
        }

        self.code_copies = known;
    }

    /// The modules that hold anonymous code: copies and a runtime's code.
    pub(super) fn anonymous_code_modules(&self) -> BTreeSet<ModuleId> {
        self.code_copies
            .values()
            .filter_map(|copy| match *copy {
                Copy::Of { module, .. } | Copy::Runtime { module } => Some(module),
                Copy::None => None,
            })
            .collect()
    }

    /// The modules that bind copies of another module's code, whose
    /// runtimes are the source module's again.
    pub(super) fn copy_modules(&self) -> BTreeSet<ModuleId> {
        self.code_copies
            .values()
            .filter_map(|copy| match *copy {
                Copy::Of { module, .. } => Some(module),
                Copy::Runtime { .. } | Copy::None => None,
            })
            .collect()
    }

    /// The modules that hold code a runtime generated, which no image
    /// describes.
    pub(super) fn generated_code_modules(&self) -> BTreeSet<ModuleId> {
        self.code_copies
            .values()
            .filter_map(|copy| match *copy {
                Copy::Runtime { module } => Some(module),
                Copy::Of { .. } | Copy::None => None,
            })
            .collect()
    }

    /// Makes the module that holds V8's code in `mapping`.
    fn register_runtime_code(&mut self, mapping: &Range<u64>) -> Option<ModuleId> {
        let id = ModuleId::new(self.next_module_id);
        let image_id = crate::ModuleImageId::new(self.next_image_id);
        let path = std::path::PathBuf::from(format!("[V8 code {:#x}]", mapping.start));
        let range = AddressRange {
            start: ImageAddress::new(mapping.start),
            end: ImageAddress::new(mapping.end),
        };
        let debug = crate::debug_info::runtime_code::load(&path, image_id, range).ok()?;
        self.next_module_id = self.next_module_id.checked_add(1)?;
        self.next_image_id = self.next_image_id.checked_add(1)?;
        let loaded = LoadedModule {
            id,
            image: image_id,
            load_bias: LoadBias::new(0),
            window: None,
        };
        self.add_module(RuntimeModule {
            loaded,
            image: debug.image,
            unwind: debug.unwind,
            variables: debug.variables,
            tls: None,
        });
        Some(id)
    }

    /// The code of each loaded module that its runtimes own as the process
    /// says at the stop ([`crate::runtime_model::RuntimeModel::process_code`]),
    /// read through `pid`, which a copy may begin with: V8's builtins, where
    /// no symbol begins them.
    fn stored_code(&self, pid: Pid) -> Vec<(ModuleId, ImageAddress, ImageAddress)> {
        let Some(inferior) = self.inferior.as_ref() else {
            return Vec::new();
        };
        let runtimes = self.runtimes(inferior);
        self.runtime_process_code(inferior, &runtimes, pid)
            .into_iter()
            .zip(&runtimes)
            .flat_map(|(code, runtime)| {
                let module = runtime.module.id;
                code.into_iter()
                    .filter_map(move |code| Some((module, code.image?.1)))
            })
            .map(|(module, range)| (module, range.start, range.end))
            .collect()
    }

    /// The range of a module's code `mapping` begins with a copy of, if any:
    /// from a function that begins a page, or from where `stored` code
    /// begins.
    fn prove_copy(
        &self,
        pid: Pid,
        mapping: &Range<u64>,
        stored: &[(ModuleId, ImageAddress, ImageAddress)],
    ) -> Option<Proven> {
        let sources: Vec<copies::Source<'_, ModuleId>> = self
            .modules
            .values()
            .map(|module| copies::Source {
                key: module.loaded.id,
                image: &module.image,
                loaded: module.loaded,
                stored: stored
                    .iter()
                    .filter(|(source, ..)| *source == module.loaded.id)
                    .map(|&(_, start, end)| (start, end))
                    .collect(),
            })
            .collect();
        let mut probe = |address| {
            raw_block(&self.ptrace, pid, address, copies::PROBE)?
                .try_into()
                .ok()
        };
        let mut compare = |source, copy, limit| self.compare(pid, source, copy, limit);
        copies::prove(mapping, &sources, &mut probe, &mut compare).map(|proven| Proven {
            source: proven.source,
            image_start: proven.image_start,
            length: proven.length,
            traps: proven.found,
        })
    }

    /// How many bytes from `copy` are those from `source`, up to `limit`,
    /// with where the copy holds a trap the debugger installed at the
    /// source, which the program copied with the code it lies in.
    fn compare(&self, pid: Pid, source: u64, copy: u64, limit: u64) -> (u64, Vec<CopiedTrap>) {
        let mut traps = Vec::new();
        let Some(inferior) = self.inferior.as_ref() else {
            return (0, traps);
        };
        let sites = &inferior.breakpoints;
        let mut length = 0;
        while length < limit {
            let size = usize::try_from((limit - length).min(CHUNK as u64)).unwrap_or(CHUNK);
            let at = VirtualAddress::new(source + length);
            let (Some(original), Some(copied)) = (
                read_logical_block(&self.ptrace, pid, sites, at, size)
                    .ok()
                    .map(|read| read.bytes),
                raw_block(&self.ptrace, pid, copy + length, size),
            ) else {
                return (length, traps);
            };
            for (offset, (original, copied)) in original.iter().zip(&copied).enumerate() {
                let offset = offset as u64;
                if original != copied {
                    let trap_site = sites.get(&VirtualAddress::new(source + length + offset));
                    if *copied != BREAKPOINT_OPCODE || !trap_site.is_some_and(|site| site.installed)
                    {
                        return (length + offset, traps);
                    }
                    traps.push(CopiedTrap {
                        address: copy + length + offset,
                        original: *original,
                    });
                }
            }
            length += original.len().min(copied.len()) as u64;
            if original.len() < size || copied.len() < size {
                return (length, traps);
            }
        }
        (length, traps)
    }

    /// Makes the module that binds a proven copy, whose bytes at `mapping`
    /// are its source module's from `proven.image_start`.
    fn register_copy(&mut self, mapping: &Range<u64>, proven: &Proven) -> Option<ModuleId> {
        let source = self.modules.get(&proven.source)?;
        let image_end = proven.image_start.get().checked_add(proven.length)?;
        let id = ModuleId::new(self.next_module_id);
        self.next_module_id += 1;
        let loaded = LoadedModule {
            id,
            image: source.loaded.image,
            load_bias: LoadBias::between(proven.image_start, VirtualAddress::new(mapping.start)),
            window: Some(AddressRange {
                start: proven.image_start,
                end: ImageAddress::new(image_end),
            }),
        };
        let module = RuntimeModule {
            loaded,
            image: Arc::clone(&source.image),
            unwind: Arc::clone(&source.unwind),
            variables: Arc::clone(&source.variables),
            tls: None,
        };
        self.add_module(module);
        Some(id)
    }

    /// Registers `module` and says so.
    fn add_module(&mut self, module: RuntimeModule) {
        let path = Arc::new(module.image.path().to_owned());
        let loaded = module.loaded;
        self.modules.insert(loaded.id, module);
        self.bump_revision();
        let _ = self.events.send(DebuggerEvent::ModuleLoaded {
            revision: self.revision,
            module: crate::LoadedModuleRecord {
                module: loaded,
                path,
            },
        });
    }
}

/// The `size` bytes at `address` as the process holds them, traps and all.
fn raw_block(ptrace: &impl InspectionOps, pid: Pid, address: u64, size: usize) -> Option<Vec<u8>> {
    let read = read_logical_block(
        ptrace,
        pid,
        &BTreeMap::new(),
        VirtualAddress::new(address),
        size,
    )
    .ok()?;
    (read.bytes.len() == size).then_some(read.bytes)
}
