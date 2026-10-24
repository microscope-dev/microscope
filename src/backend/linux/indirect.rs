//! Breakpoints on indirect functions, such as glibc's `strlen`, whose
//! symbol names a resolver that the loader calls, as it relocates a module
//! referring to the function, to choose an implementation for the machine.
//!
//! The implementation is learned as gdb learns it, without calling into the
//! program: from a GOT slot the loader already filled with it, or else by
//! catching the resolver's return when the loader next calls it. Until
//! then, a breakpoint on the function waits, pending.

use std::collections::{BTreeMap, BTreeSet};

use nix::unistd::Pid;

use crate::protocol::BreakpointSpec;
use crate::{Error, GotTarget, ImageAddress, ModuleId, Result, SymbolKind, VirtualAddress};

use super::breakpoints::remove_breakpoint_owner_from;
use super::native::LinuxTraceOps;
use super::{BreakpointOwner, Controller};

/// An indirect function's resolver: its module and its image address.
type Resolver = (ModuleId, ImageAddress);

#[derive(Debug, Default)]
pub(super) struct IndirectFunctions {
    /// The implementation each resolver chose, in the resolver's module.
    chosen: BTreeMap<Resolver, ImageAddress>,

    /// The resolvers whose entries are planted, by entry.
    entries: BTreeMap<VirtualAddress, Resolver>,

    /// Calls of resolvers under way, each awaiting its return.
    calls: Vec<Call>,

    /// The return sites planted for `calls`.
    returns: BTreeSet<VirtualAddress>,
}

impl IndirectFunctions {
    /// Forgets a site whose memory is gone.
    pub(super) fn forget(&mut self, address: VirtualAddress) {
        self.entries.remove(&address);
        self.returns.remove(&address);
        self.calls.retain(|call| call.returns_to != address);
    }

    /// Every site planted: the resolvers' entries and their calls' returns.
    pub(super) fn planted(&self) -> BTreeSet<VirtualAddress> {
        self.entries.keys().chain(&self.returns).copied().collect()
    }
}

#[derive(Debug, Clone, Copy)]
struct Call {
    thread: Pid,
    resolver: Resolver,
    returns_to: VirtualAddress,

    /// The stack pointer once the call returns, which tells its return
    /// from a deeper call's return to the same place.
    stack: u64,
}

impl<P: LinuxTraceOps> Controller<P> {
    /// The implementation an indirect function's resolver chose, once
    /// known.
    pub(super) fn chosen_implementation(
        &self,
        module: ModuleId,
        resolver: ImageAddress,
    ) -> Option<ImageAddress> {
        let inferior = self.inferior.as_ref()?;
        inferior.indirect.chosen.get(&(module, resolver)).copied()
    }

    /// The resolvers of the indirect functions that enabled breakpoints,
    /// and `adding`, name and whose implementations are unknown.
    fn wanted_resolvers(&self, adding: Option<&BreakpointSpec>) -> BTreeSet<Resolver> {
        let Some(inferior) = self.inferior.as_ref() else {
            return BTreeSet::new();
        };

        let names = self
            .breakpoints
            .iter()
            .filter(|breakpoint| breakpoint.enabled)
            .map(|breakpoint| &breakpoint.spec)
            .chain(adding)
            .filter_map(|spec| match spec {
                BreakpointSpec::Function(name) if !crate::names_runtime_function(name) => {
                    Some(name.as_str())
                }
                _ => None,
            })
            .collect::<BTreeSet<_>>();
        let mut wanted = BTreeSet::new();

        for (&module, runtime) in &self.modules {
            for name in &names {
                // A function the debug information describes is broken at
                // as it describes it.
                if !matches!(
                    runtime.image.functions_located(name, None),
                    Err(Error::FunctionNotFound(_))
                ) {
                    continue;
                }

                wanted.extend(
                    runtime
                        .image
                        .symbols_answering(name)
                        .filter(|symbol| {
                            symbol.kind() == SymbolKind::IndirectFunction
                                && symbol.extent().is_some()
                        })
                        .map(|symbol| (module, symbol.address()))
                        .filter(|resolver| !inferior.indirect.chosen.contains_key(resolver)),
                );
            }
        }

        wanted
    }

    /// Learns, from the GOT slots the loader has filled, the
    /// implementations that the indirect functions enabled breakpoints,
    /// and `adding`, name were resolved to.
    pub(super) fn learn_from_got(&mut self, adding: Option<&BreakpointSpec>) {
        let wanted = self.wanted_resolvers(adding);
        let Some(inferior) = self.inferior.as_mut() else {
            return;
        };

        if wanted.is_empty() {
            return;
        }

        let pid = inferior.memory_thread();
        let mut learned = Vec::new();

        for &(module, resolver) in &wanted {
            let Some(defining) = self.modules.get(&module) else {
                continue;
            };

            let names = defining
                .image
                .symbols()
                .filter(|symbol| symbol.address() == resolver && symbol.exported())
                .map(crate::image::symbols::Symbol::unversioned_name)
                .collect::<BTreeSet<_>>();

            // A slot of the module's own that the resolver fills, or one
            // of any module's that imports the function by name.
            let mut slots = Vec::new();

            for (&holder, runtime) in &self.modules {
                for slot in runtime.image.got_slots() {
                    let fills = match &slot.target {
                        GotTarget::Indirect(address) => holder == module && *address == resolver,
                        GotTarget::Import(name) => names.contains(&**name),
                    };

                    if fills && let Ok(address) = runtime.loaded.virtual_address(slot.address) {
                        slots.push(address);
                    }
                }
            }

            for slot in slots {
                let Ok(value) = self.ptrace.read_word(pid, slot.get()) else {
                    continue;
                };

                // A slot the loader has yet to relocate, or one bound to
                // another module's definition, holds no code of the
                // resolver's module.
                if let Some(implementation) =
                    implementation_in(defining, VirtualAddress::new(value), resolver)
                {
                    learned.push(((module, resolver), implementation));
                    break;
                }
            }
        }

        inferior.indirect.chosen.extend(learned);
    }

    /// Plants the entries of the resolvers that enabled breakpoints wait
    /// for, and the returns of their calls under way, and removes the
    /// rest.
    pub(super) fn sync_resolvers(&mut self) -> Result<()> {
        let wanted = self.wanted_resolvers(None);
        let mut entries = BTreeMap::new();

        for &(module, resolver) in &wanted {
            if let Some(entry) = self
                .modules
                .get(&module)
                .and_then(|runtime| runtime.loaded.virtual_address(resolver).ok())
            {
                entries.insert(entry, (module, resolver));
            }
        }

        let Some(inferior) = self.inferior.as_mut() else {
            return Ok(());
        };

        let live = |pid: &Pid| inferior.threads.contains_key(pid);
        let calls = (inferior.indirect.calls.iter())
            .filter(|call| wanted.contains(&call.resolver) && live(&call.thread))
            .copied()
            .collect::<Vec<_>>();
        let returns = calls
            .iter()
            .map(|call| call.returns_to)
            .collect::<BTreeSet<_>>();
        let planted = inferior.indirect.planted();
        let needed = entries
            .keys()
            .chain(&returns)
            .copied()
            .collect::<BTreeSet<_>>();

        // The record changes with each site, so that a failed edit leaves
        // it naming exactly the sites planted.
        for &address in planted.difference(&needed) {
            remove_breakpoint_owner_from(
                &self.ptrace,
                inferior,
                address,
                BreakpointOwner::Resolver,
            )?;
            inferior.indirect.entries.remove(&address);
            inferior.indirect.returns.remove(&address);
        }

        let pid = inferior.memory_thread();
        for &address in needed.difference(&planted) {
            self.ptrace.install_breakpoint(
                pid,
                &mut inferior.breakpoints,
                address,
                BreakpointOwner::Resolver,
            )?;
            match entries.get(&address) {
                Some(&resolver) => {
                    inferior.indirect.entries.insert(address, resolver);
                }
                None => {
                    inferior.indirect.returns.insert(address);
                }
            }
        }

        inferior.indirect.entries = entries;
        inferior.indirect.calls = calls;
        inferior.indirect.returns = returns;
        Ok(())
    }

    /// Notes a thread that reached a resolver's entry, or the return of
    /// its call, if `address` is one, and stops every thread to plant the
    /// return, or to break at the implementation the resolver chose.
    pub(super) fn note_resolver(&mut self, pid: Pid, address: VirtualAddress) -> Result<()> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        let indirect = &inferior.indirect;
        if !indirect.entries.contains_key(&address) && !indirect.returns.contains(&address) {
            return Ok(());
        }

        let registers = self.ptrace.registers(pid)?;

        if let Some(&resolver) = indirect.entries.get(&address) {
            let returns_to = VirtualAddress::new(self.ptrace.read_word(pid, registers.rsp)?);
            record!("{pid} calls the resolver at {address}, returning to {returns_to}");
            let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
            inferior.indirect.calls.push(Call {
                thread: pid,
                resolver,
                returns_to,
                stack: registers.rsp.wrapping_add(8),
            });

            return self.queue_module_refresh();
        }

        let Some(index) = indirect.calls.iter().position(|call| {
            call.thread == pid && call.returns_to == address && call.stack == registers.rsp
        }) else {
            return Ok(());
        };

        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        let call = inferior.indirect.calls.remove(index);
        let (module, resolver) = call.resolver;
        let chosen = self.modules.get(&module).and_then(|defining| {
            implementation_in(defining, VirtualAddress::new(registers.rax), resolver)
        });

        record!(
            "the resolver {resolver} in module {module} returned {:#x}, choosing {chosen:?}",
            registers.rax
        );
        if let Some(implementation) = chosen {
            let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
            inferior
                .indirect
                .chosen
                .insert(call.resolver, implementation);
        }

        self.queue_module_refresh()
    }
}

/// Where in the resolver's module an implementation is, when `address` is
/// code of that module other than the resolver or a PLT stub: a slot the
/// loader has yet to relocate or bind points at its own stub.
fn implementation_in(
    defining: &super::RuntimeModule,
    address: VirtualAddress,
    resolver: ImageAddress,
) -> Option<ImageAddress> {
    let image_address = defining.loaded.image_address(address).ok()?;
    (image_address != resolver
        && defining.image.sections().iter().any(|section| {
            section.executable
                && section.range.contains(image_address)
                && !section.name.starts_with(".plt")
                && &*section.name != ".iplt"
        }))
    .then_some(image_address)
}
