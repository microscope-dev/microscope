//! Detects anonymous executable mappings that are verbatim copies of code from a
//! loaded module (e.g. V8 remapping embedded builtins) or JIT-generated code.
//!
//! When a runtime copies a code page that already has a software breakpoint
//! installed, this module scrubs the copied trap byte and rewinds any thread
//! that hits a copied trap before refresh runs.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::sync::Arc;

use nix::unistd::Pid;

use super::memory::read_logical_block;
use super::modules::AnonymousCode;
use super::native::{InspectionOps, LinuxTraceOps};
use super::trap::Trap;
use super::{BREAKPOINT_OPCODE, Controller, RuntimeModule};
use crate::protocol::DebuggerEvent;
use crate::unwind::code_copies as copies;
use crate::{AddressRange, ImageAddress, LoadBias, LoadedModule, ModuleId, VirtualAddress};

const CHUNK: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Copy {
    Of { module: ModuleId, source: ModuleId },

    Runtime { module: ModuleId },

    None,
}

struct Proven {
    source: ModuleId,
    image_start: ImageAddress,
    length: u64,

    traps: Vec<CopiedTrap>,
}

pub(super) struct CopiedTrap {
    pub(super) address: u64,
    pub(super) original: u8,
}

impl<P: LinuxTraceOps> Controller<P> {
    pub(super) fn observe_code_copies(&mut self, pid: Pid, anonymous: &[AnonymousCode]) {
        self.observe_code_copies_with(pid, anonymous, |controller, traps| {
            controller.restore_copied_traps(pid, traps)
        });
    }

    pub(super) fn restore_copied_traps(&self, pid: Pid, traps: &[CopiedTrap]) -> Option<()> {
        for trap in traps {
            super::memory::patch_byte(
                VirtualAddress::new(trap.address),
                trap.original,
                |word| self.ptrace.read_word(pid, word),
                |word, value| self.ptrace.write_word(pid, word, value),
            )
            .ok()?;
        }
        Some(())
    }

    pub(super) fn rewind_copied_trap(&self, pid: Pid) -> bool {
        let rewind = || {
            let inferior = self.session.inferior()?;
            let mut registers = self.ptrace.registers(pid).ok()?;
            let address = registers.breakpoint_address()?.get();
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
            registers.set_program_counter(VirtualAddress::new(address));
            self.ptrace.set_registers(pid, registers).ok()
        };
        rewind().is_some()
    }

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

    pub(super) fn anonymous_code_modules(&self) -> BTreeSet<ModuleId> {
        self.code_copies
            .values()
            .filter_map(|copy| match *copy {
                Copy::Of { module, .. } | Copy::Runtime { module } => Some(module),
                Copy::None => None,
            })
            .collect()
    }

    pub(super) fn copy_modules(&self) -> BTreeSet<ModuleId> {
        self.code_copies
            .values()
            .filter_map(|copy| match *copy {
                Copy::Of { module, .. } => Some(module),
                Copy::Runtime { .. } | Copy::None => None,
            })
            .collect()
    }

    pub(super) fn generated_code_modules(&self) -> BTreeSet<ModuleId> {
        self.code_copies
            .values()
            .filter_map(|copy| match *copy {
                Copy::Runtime { module } => Some(module),
                Copy::Of { .. } | Copy::None => None,
            })
            .collect()
    }

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

    fn stored_code(&self, pid: Pid) -> Vec<(ModuleId, ImageAddress, ImageAddress)> {
        let Some(inferior) = self.session.inferior() else {
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

    fn compare(&self, pid: Pid, source: u64, copy: u64, limit: u64) -> (u64, Vec<CopiedTrap>) {
        let mut traps = Vec::new();
        let Some(inferior) = self.session.inferior() else {
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
                    if *copied != BREAKPOINT_OPCODE
                        || !trap_site.is_some_and(|site| site.trap == Trap::Installed)
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
