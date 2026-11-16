use std::collections::{BTreeMap, BTreeSet};

use nix::unistd::Pid;

use crate::protocol::BreakpointSpec;
use crate::{Error, GotTarget, ImageAddress, ModuleId, Result, SymbolKind, VirtualAddress};

use super::breakpoints::remove_breakpoint_owner_from;
use super::native::LinuxTraceOps;
use super::{BreakpointOwner, Controller};

type Resolver = (ModuleId, ImageAddress);

#[derive(Debug, Default)]
pub(super) struct IndirectFunctions {
    chosen: BTreeMap<Resolver, ImageAddress>,

    entries: BTreeMap<VirtualAddress, Resolver>,

    calls: Vec<Call>,

    returns: BTreeSet<VirtualAddress>,
}

impl IndirectFunctions {
    pub(super) fn forget(&mut self, address: VirtualAddress) {
        self.entries.remove(&address);
        self.returns.remove(&address);
        self.calls.retain(|call| call.returns_to != address);
    }

    pub(super) fn planted(&self) -> BTreeSet<VirtualAddress> {
        self.entries.keys().chain(&self.returns).copied().collect()
    }
}

#[derive(Debug, Clone, Copy)]
struct Call {
    thread: Pid,
    resolver: Resolver,
    returns_to: VirtualAddress,

    stack: u64,
}

impl<P: LinuxTraceOps> Controller<P> {
    pub(super) fn chosen_implementation(
        &self,
        module: ModuleId,
        resolver: ImageAddress,
    ) -> Option<ImageAddress> {
        let inferior = self.session.inferior()?;
        inferior.indirect.chosen.get(&(module, resolver)).copied()
    }

    fn wanted_resolvers(&self, adding: Option<&BreakpointSpec>) -> BTreeSet<Resolver> {
        let Some(inferior) = self.session.inferior() else {
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

    pub(super) fn learn_from_got(&mut self, adding: Option<&BreakpointSpec>) {
        let wanted = self.wanted_resolvers(adding);
        let Some(inferior) = self.session.inferior_mut() else {
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

        let Some(inferior) = self.session.inferior_mut() else {
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

    pub(super) fn note_resolver(&mut self, pid: Pid, address: VirtualAddress) -> Result<()> {
        let inferior = self.session.inferior().ok_or(Error::NotRunning)?;
        let indirect = &inferior.indirect;
        if !indirect.entries.contains_key(&address) && !indirect.returns.contains(&address) {
            return Ok(());
        }

        let registers = self.ptrace.registers(pid)?;

        if let Some(&resolver) = indirect.entries.get(&address) {
            let pending =
                registers.pending_return(|address| self.ptrace.read_word(pid, address))?;
            let returns_to = pending.returns_to;
            record!("{pid} calls the resolver at {address}, returning to {returns_to}");
            let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;
            inferior.indirect.calls.push(Call {
                thread: pid,
                resolver,
                returns_to,
                stack: pending.stack,
            });

            return self.queue_module_refresh();
        }

        let Some(index) = indirect.calls.iter().position(|call| {
            call.thread == pid
                && call.returns_to == address
                && call.stack == registers.stack_pointer()
        }) else {
            return Ok(());
        };

        let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;
        let call = inferior.indirect.calls.remove(index);
        let (module, resolver) = call.resolver;
        let chosen = self.modules.get(&module).and_then(|defining| {
            implementation_in(
                defining,
                VirtualAddress::new(registers.return_value()),
                resolver,
            )
        });

        record!(
            "the resolver {resolver} in module {module} returned {:#x}, choosing {chosen:?}",
            registers.return_value()
        );
        if let Some(implementation) = chosen {
            let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;
            inferior
                .indirect
                .chosen
                .insert(call.resolver, implementation);
        }

        self.queue_module_refresh()
    }
}

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
