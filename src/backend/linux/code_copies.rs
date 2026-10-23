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
//! page of the module, as code copied whole begins. Writable memory, such
//! as a JIT compiler's, is never taken for a copy, since its bytes may
//! change between stops. A proven copy, and memory proven to be none, is
//! remembered until its mapping changes.

use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::Arc;

use nix::unistd::Pid;

use super::memory::read_logical_block;
use super::native::{InspectionOps, LinuxTraceOps};
use super::{BREAKPOINT_OPCODE, Controller, RuntimeModule};
use crate::protocol::DebuggerEvent;
use crate::{
    AddressRange, ImageAddress, LoadBias, LoadedModule, ModuleId, SymbolKind, VirtualAddress,
};

/// The size of the pages copies begin on.
const PAGE: u64 = 4096;

/// How many of a copy's first bytes pick the function it may copy.
const PROBE: usize = 16;

/// How much of a copy is compared at once.
const CHUNK: usize = 256 * 1024;

/// The most of a copy compared, past which its window ends.
const MAX_COPY: u64 = 256 * 1024 * 1024;

/// What a mapping of anonymous code was found to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Copy {
    /// A copy of the code of the module `source`, which `module` binds.
    Of { module: ModuleId, source: ModuleId },

    /// No copy of any module's code.
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
    pub(super) fn observe_code_copies(&mut self, pid: Pid, anonymous: &[Range<u64>]) {
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
            let mapping = mappings
                .anonymous_code
                .iter()
                .find(|mapping| mapping.contains(&address))?;
            if matches!(
                self.code_copies.get(&(mapping.start, mapping.end)),
                Some(Copy::Of { .. })
            ) {
                return None;
            }
            self.prove_copy(memory, mapping)?
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
        self.code_copies.retain(|mapping, copy| {
            !(matches!(copy, Copy::None) && (mapping.0..mapping.1).contains(&address))
        });
    }
}

impl<P: InspectionOps> Controller<P> {
    /// Brings the modules that bind copies up to date with the mappings of
    /// anonymous code, `anonymous`: a copy whose mapping is gone, or whose
    /// source module is, is unregistered, and each new mapping that is a
    /// copy becomes a module once `scrub` took out the traps it holds.
    pub(super) fn observe_code_copies_with(
        &mut self,
        pid: Pid,
        anonymous: &[Range<u64>],
        scrub: impl Fn(&Self, &[CopiedTrap]) -> Option<()>,
    ) {
        let mut known = std::mem::take(&mut self.code_copies);
        known.retain(|mapping, copy| {
            let current = anonymous.contains(&(mapping.0..mapping.1));
            match *copy {
                Copy::Of { module, source } => {
                    let kept = current && self.modules.contains_key(&source);
                    if !kept {
                        self.unregister_module(module);
                    }
                    kept
                }
                Copy::None => current,
            }
        });

        for mapping in anonymous {
            if known.contains_key(&(mapping.start, mapping.end)) {
                continue;
            }
            let copy = self
                .prove_copy(pid, mapping)
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
                .unwrap_or(Copy::None);
            known.insert((mapping.start, mapping.end), copy);
        }

        self.code_copies = known;
    }

    /// The range of a module's code `mapping` begins with a copy of, if any.
    fn prove_copy(&self, pid: Pid, mapping: &Range<u64>) -> Option<Proven> {
        let probe = raw_block(&self.ptrace, pid, mapping.start, PROBE)?;
        // Unused memory proves nothing.
        if probe.len() < PROBE || probe.iter().all(|byte| *byte == 0) {
            return None;
        }

        self.modules
            .values()
            .filter(|module| module.loaded.window.is_none())
            .find_map(|module| {
                candidates(module).find_map(|(start, end)| {
                    let source = module.loaded.virtual_address(start).ok()?.get();
                    let limit = (end.get() - start.get())
                        .min(mapping.end - mapping.start)
                        .min(MAX_COPY);
                    let probed = limit.min(PROBE as u64);
                    if self.compare(pid, source, mapping.start, probed).0 < probed {
                        return None;
                    }
                    // Code that ends before the probe does is proven whole.
                    let (length, traps) = self.compare(pid, source, mapping.start, limit);
                    (length >= probed).then_some(Proven {
                        source: module.loaded.id,
                        image_start: start,
                        length,
                        traps,
                    })
                })
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
        let path = Arc::new(module.image.path().to_owned());
        self.modules.insert(id, module);
        self.bump_revision();
        let _ = self.events.send(DebuggerEvent::ModuleLoaded {
            revision: self.revision,
            module: crate::LoadedModuleRecord {
                module: loaded,
                path,
            },
        });
        Some(id)
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

/// The functions of `module`'s code that begin a page, with where the
/// code they lie in ends, where a copy of its code may begin.
fn candidates(module: &RuntimeModule) -> impl Iterator<Item = (ImageAddress, ImageAddress)> + '_ {
    let sections = module.image.sections();
    module
        .image
        .symbols()
        .filter(|symbol| {
            symbol.kind() == SymbolKind::Function && symbol.address().get() % PAGE == 0
        })
        .filter_map(move |symbol| {
            let address = symbol.address();
            sections
                .iter()
                .find(|section| section.executable && section.range.contains(address))
                .map(|section| (address, section.range.end))
        })
        .scan(None, |previous, candidate| {
            // Aliases at one address are one candidate.
            let repeated = *previous == Some(candidate.0);
            *previous = Some(candidate.0);
            Some((!repeated).then_some(candidate))
        })
        .flatten()
}
