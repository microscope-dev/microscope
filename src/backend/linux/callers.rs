//! The callers of a frame, which recover the values its function's
//! parameters held on entry from the calls that passed them (DWARF 5
//! section 2.5.1.7), through any chain of tail calls between.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use crate::debug_info::{
    CallSiteId, CallTarget, EntryParameter, TailCallChain, TailJump, VariableRegister,
    VariableRuntime, VariableRuntimeError,
};

use crate::inspection::InspectionBudget;
use crate::{
    EntryValueUnavailableReason, ImageAddress, ModuleId, SymbolBinding, SymbolKind,
    VariableUnavailableReason, VirtualAddress,
};

use super::frames::{FrameRegisters, PhysicalFrame, PhysicalStack, StackRoot, unwind_module_for};
use super::inspection::LinuxVariableRuntime;
use super::native::InspectionOps;
use super::{Controller, Inferior, RuntimeModule};
use crate::unwind::DEFAULT_MAX_FRAMES;

/// How many callers one entry value may consult, through call sites whose
/// values are themselves entry values.
const MAX_ENTRY_VALUE_DEPTH: usize = 16;

/// The frame a runtime reads, as entry values find its caller.
#[derive(Clone, Copy)]
pub(super) struct FrameAt {
    /// The physical activation, innermost first.
    pub(super) activation: usize,

    /// The module describing its code, and its address in that image.
    pub(super) code: Option<(ModuleId, ImageAddress)>,

    /// How many callers away from the inspected frame it is.
    pub(super) depth: usize,
}

/// The activations of a stopped thread or a parked task, unwound once for
/// every entry value one inspection recovers.
pub(super) struct Callers<'a, P: InspectionOps> {
    controller: &'a Controller<P>,
    inferior: &'a Inferior,
    root: StackRoot,

    /// The activations unwound so far, and how many were asked for: fewer
    /// means the stack ended.
    stack: RefCell<Option<(PhysicalStack, usize)>>,
}

const fn unavailable(reason: EntryValueUnavailableReason) -> VariableRuntimeError {
    VariableRuntimeError::Unavailable(VariableUnavailableReason::EntryValue(reason))
}

impl<'a, P: InspectionOps> Callers<'a, P> {
    pub(super) fn new(
        controller: &'a Controller<P>,
        inferior: &'a Inferior,
        root: StackRoot,
    ) -> Rc<Self> {
        Rc::new(Self {
            controller,
            inferior,
            root,
            stack: RefCell::new(None),
        })
    }

    /// The callers of a stack already unwound, as far as `requested`
    /// activations.
    pub(super) fn with_stack(
        controller: &'a Controller<P>,
        inferior: &'a Inferior,
        root: StackRoot,
        stack: PhysicalStack,
        requested: usize,
    ) -> Rc<Self> {
        Rc::new(Self {
            controller,
            inferior,
            root,
            stack: RefCell::new(Some((stack, requested))),
        })
    }

    /// The physical activation at `index`, unwinding further if needed.
    fn activation(&self, index: usize) -> Result<Option<PhysicalFrame>, VariableRuntimeError> {
        let mut stack = self.stack.borrow_mut();
        let complete = stack.as_ref().is_some_and(|(stack, requested)| {
            index < stack.frames.len() || stack.frames.len() < *requested
        });
        if !complete {
            let requested = index.saturating_add(4).min(DEFAULT_MAX_FRAMES);
            let unwound = self
                .controller
                .physical_stack(self.inferior, &self.root, requested)
                .map_err(|error| VariableRuntimeError::Fatal(error.to_string().into()))?;
            *stack = Some((unwound, requested));
        }

        Ok(stack
            .as_ref()
            .and_then(|(stack, _)| stack.frames.get(index))
            .cloned())
    }

    /// The value `parameter` held on entry to the function of the frame at
    /// `frame`, whose location `module` describes.
    pub(super) fn entry_value(
        self: &Rc<Self>,
        frame: FrameAt,
        module: ModuleId,
        parameter: EntryParameter,
        budget: &mut InspectionBudget,
    ) -> Result<u64, VariableRuntimeError> {
        let value = self.caller_value(frame, module, parameter, budget);
        if frame.depth == 0 {
            return value;
        }

        // A caller asks for its own entry value to say what it passed, and
        // what its caller lacks is not what the frame's caller lacks.
        value.map_err(|error| match error {
            VariableRuntimeError::Unavailable(
                reason @ VariableUnavailableReason::EntryValue(_),
            ) => unavailable(EntryValueUnavailableReason::Caller(Box::new(reason))),
            error => error,
        })
    }

    /// What the caller of the frame at `frame` passed for `parameter`.
    fn caller_value(
        self: &Rc<Self>,
        frame: FrameAt,
        module: ModuleId,
        parameter: EntryParameter,
        budget: &mut InspectionBudget,
    ) -> Result<u64, VariableRuntimeError> {
        if frame.depth >= MAX_ENTRY_VALUE_DEPTH {
            return Err(VariableUnavailableReason::EvaluationLimit.into());
        }

        let frame_module = self.module(module)?;
        let entry = self.entry(frame, module, budget)?;

        self.with_caller(
            entry.caller,
            &entry.caller_frame,
            entry.caller_at,
            |caller_runtime| {
                let mut chain = Chain {
                    callee: frame_module,
                    caller: entry.caller,
                    site: entry.site,
                    caller_runtime,
                };

                chain.value(&entry.tail_calls.links, parameter, module, budget)
            },
        )
        .map_err(|error| match error {
            // What the caller cannot provide, the entry value cannot.
            VariableRuntimeError::Unavailable(reason)
                if !matches!(
                    reason,
                    VariableUnavailableReason::EntryValue(_)
                        | VariableUnavailableReason::EvaluationLimit
                        | VariableUnavailableReason::InspectionLimit(_)
                ) =>
            {
                unavailable(EntryValueUnavailableReason::Caller(Box::new(reason)))
            }
            error => error,
        })
    }

    /// Where each function that left by a tail call between the call that
    /// entered the activation at `activation` and its function jumped from,
    /// the last one first. There are none unless the debug information
    /// allows exactly one chain of tail calls, and says where each jumped.
    pub(super) fn tail_jumps(
        self: &Rc<Self>,
        activation: usize,
        code: (ModuleId, ImageAddress),
    ) -> Vec<TailJump> {
        let frame = FrameAt {
            activation,
            code: Some(code),
            depth: 0,
        };

        let mut budget = InspectionBudget::new(crate::InspectionLimits::default());
        let Ok(entry) = self.entry(frame, code.0, &mut budget) else {
            return Vec::new();
        };

        entry
            .tail_calls
            .jumps
            .iter()
            .rev()
            .copied()
            .collect::<Option<Vec<_>>>()
            .unwrap_or_default()
    }

    /// The call that entered the function of the frame at `frame`, whose
    /// location `module` describes, and the one chain of tail calls since.
    fn entry(
        self: &Rc<Self>,
        frame: FrameAt,
        module: ModuleId,
        budget: &mut InspectionBudget,
    ) -> Result<Entry<'a>, VariableRuntimeError> {
        // Only a frame's own function has entry values.
        let Some((_, code)) = frame.code.filter(|(code_module, _)| *code_module == module) else {
            return Err(unavailable(EntryValueUnavailableReason::NoCaller));
        };

        let frame_module = self.module(module)?;

        let caller_index = frame.activation + 1;
        let Some(caller_frame) = self.activation(caller_index)? else {
            return Err(unavailable(EntryValueUnavailableReason::NoCaller));
        };

        // An interrupted frame did not call the frame above it.
        if caller_frame.context.signal_frame {
            return Err(unavailable(EntryValueUnavailableReason::NoCaller));
        }

        let return_address = caller_frame.context.instruction;
        let modules = self.controller.unwind_modules(self.inferior);
        let lookup = return_address
            .get()
            .checked_sub(1)
            .map(VirtualAddress::new)
            .ok_or_else(|| unavailable(EntryValueUnavailableReason::NoCaller))?;
        let Some((unwind_module, caller_code)) = unwind_module_for(&modules, lookup) else {
            return Err(unavailable(EntryValueUnavailableReason::NoCallSite));
        };

        let caller = self.module(unwind_module.loaded.id)?;
        let caller_return = caller
            .loaded
            .image_address(return_address)
            .map_err(|_| unavailable(EntryValueUnavailableReason::NoCallSite))?;

        // Reading the caller's frame costs far more than asking whether a
        // call returns where it does.
        if !caller.variables.may_describe_call_site(caller_return) {
            return Err(unavailable(EntryValueUnavailableReason::NoCallSite));
        }

        let caller_at = FrameAt {
            activation: caller_index,
            code: Some((caller.loaded.id, caller_code)),
            depth: frame.depth + 1,
        };

        let site = self
            .with_caller(caller, &caller_frame, caller_at, |caller_runtime| {
                caller
                    .variables
                    .call_site(caller_return, caller_runtime, budget)
            })?
            .ok_or_else(|| unavailable(EntryValueUnavailableReason::NoCallSite))?;

        let target = self.call_target(caller, site.target)?;
        let from = frame_module
            .loaded
            .image_address(target)
            .ok()
            .filter(|address| frame_module.image.contains_address(*address))
            .ok_or_else(|| unavailable(EntryValueUnavailableReason::TargetMismatch))?;
        let tail_calls = frame_module.variables.tail_calls(from, code)?;
        self.entered_as_described(module, &tail_calls.functions)?;

        Ok(Entry {
            caller,
            caller_frame,
            caller_at,
            site: site.id,
            tail_calls,
        })
    }

    /// Runs `read` with a runtime that reads `caller_frame`, which runs
    /// `caller`'s code.
    fn with_caller<T>(
        self: &Rc<Self>,
        caller: &RuntimeModule,
        caller_frame: &PhysicalFrame,
        caller_at: FrameAt,
        read: impl FnOnce(&mut LinuxVariableRuntime<'_, P>) -> T,
    ) -> T {
        let modules = self.controller.unwind_modules(self.inferior);
        let reader = self.root.reader();
        let cfa =
            self.controller
                .frame_cfa(reader, &modules, caller_at.code, &caller_frame.registers);
        let below_stack_pointer =
            self.controller
                .below_stack_pointer(self.inferior, &self.root, caller_frame);
        let caller_registers = FrameRegisters::Caller(caller_frame.registers.clone());
        let mut caller_runtime = LinuxVariableRuntime {
            ptrace: &self.controller.ptrace,
            pid: reader,
            thread: self.root.thread(),
            loaded_module: caller.loaded,
            image_range: caller.image.address_range(),
            breakpoints: &self.inferior.breakpoints,
            registers: &caller_registers,
            floating: None,
            cfa,
            tls: caller.tls,
            frame: caller_at,
            callers: Some(Rc::clone(self)),
            below_stack_pointer,
        };

        read(&mut caller_runtime)
    }

    /// Where a call site in `caller` calls.
    fn call_target(
        &self,
        caller: &RuntimeModule,
        target: CallTarget,
    ) -> Result<VirtualAddress, VariableRuntimeError> {
        match target {
            CallTarget::Code(address) => caller
                .loaded
                .virtual_address(address)
                .map_err(|error| VariableRuntimeError::Malformed(error.to_string().into())),
            CallTarget::Computed(address) => Ok(address),
            CallTarget::Symbol(name) => self
                .symbol(&name)
                .ok_or_else(|| unavailable(EntryValueUnavailableReason::UnknownTarget)),
            CallTarget::Unknown => Err(unavailable(EntryValueUnavailableReason::UnknownTarget)),
        }
    }

    fn module(&self, id: ModuleId) -> Result<&'a RuntimeModule, VariableRuntimeError> {
        self.controller
            .modules
            .get(&id)
            .ok_or_else(|| VariableRuntimeError::Fatal(format!("module {id} is not loaded").into()))
    }

    /// Fails unless the call and its tail calls entered `module`'s
    /// `functions`, named as linker symbols: one another module also
    /// defines may have taken the call, or a jump through the procedure
    /// linkage table, in its place.
    fn entered_as_described(
        &self,
        module: ModuleId,
        functions: &[Option<Arc<str>>],
    ) -> Result<(), VariableRuntimeError> {
        for (position, name) in functions.iter().enumerate() {
            if name
                .as_deref()
                .is_some_and(|name| self.defined_elsewhere(module, name))
            {
                return Err(unavailable(if position == 0 {
                    EntryValueUnavailableReason::UnknownTarget
                } else {
                    EntryValueUnavailableReason::TailCalls
                }));
            }
        }

        Ok(())
    }

    /// Whether a module other than `module` exports `name` as code, which
    /// calls to `module`'s function of that name may reach instead. The
    /// executable comes first in every lookup, so nothing takes its
    /// functions' place. Untyped symbols count: assembly often leaves
    /// functions so.
    fn defined_elsewhere(&self, module: ModuleId, name: &str) -> bool {
        module != self.controller.main.id
            && self.controller.modules.values().any(|other| {
                other.loaded.id != module
                    && other.image.symbols_named(name).any(|symbol| {
                        symbol.exported()
                            && symbol.binding() != SymbolBinding::Local
                            && symbol.kind() != SymbolKind::Data
                    })
            })
    }

    /// The function whose first instruction `address` is, by its symbol's
    /// name, in whichever module holds it.
    pub(super) fn function_at(&self, address: VirtualAddress) -> Option<Arc<str>> {
        let modules = self.controller.unwind_modules(self.inferior);
        let (module, image_address) = unwind_module_for(&modules, address)?;
        let symbol = module.image.symbolize(image_address)?;
        (symbol.offset == 0
            && matches!(
                symbol.kind,
                SymbolKind::Function | SymbolKind::IndirectFunction
            ))
        .then(|| {
            symbol
                .demangled_name()
                .map_or_else(|| Arc::clone(&symbol.name), Arc::from)
        })
    }

    /// The one address a linker symbol defines a function at, across every
    /// loaded module. An indirect function's callers reach whichever
    /// implementation its resolver chose, which is not its address.
    fn symbol(&self, name: &str) -> Option<VirtualAddress> {
        let mut found = None;
        for module in self.controller.modules.values() {
            for symbol in module.image.symbols_named(name) {
                if symbol.binding() == SymbolBinding::Local || symbol.kind() != SymbolKind::Function
                {
                    continue;
                }

                let address = module.loaded.virtual_address(symbol.address()).ok()?;
                if found.is_some_and(|found| found != address) {
                    return None;
                }

                found = Some(address);
            }
        }

        found
    }
}

/// The call that entered a frame's function, from the frame that made it,
/// and the tail calls since.
struct Entry<'a> {
    caller: &'a RuntimeModule,
    caller_frame: PhysicalFrame,
    caller_at: FrameAt,
    site: CallSiteId,
    tail_calls: TailCallChain,
}

/// The call that entered a frame's function, and the tail calls after it.
struct Chain<'c, 'r, 'a, P: InspectionOps> {
    callee: &'a RuntimeModule,
    caller: &'a RuntimeModule,
    site: CallSiteId,
    caller_runtime: &'c mut LinuxVariableRuntime<'r, P>,
}

impl<P: InspectionOps> Chain<'_, '_, '_, P> {
    /// The value passed for `parameter`, which `requester` describes, by the
    /// last of `links`, the tail calls between the call and the frame, or by
    /// the call itself.
    fn value(
        &mut self,
        links: &[CallSiteId],
        parameter: EntryParameter,
        requester: ModuleId,
        budget: &mut InspectionBudget,
    ) -> Result<u64, VariableRuntimeError> {
        let (module, runtime_links) = match links.split_last() {
            None => (self.caller, None),
            Some((last, rest)) => (self.callee, Some((*last, rest))),
        };

        // A parameter entry names a parameter only within its own module.
        if matches!(parameter, EntryParameter::Parameter(_)) && module.loaded.id != requester {
            return Err(unavailable(EntryValueUnavailableReason::NoParameter));
        }

        match runtime_links {
            None => {
                let site = self.site;
                module
                    .variables
                    .call_site_value(site, parameter, self.caller_runtime, budget)
            }
            Some((last, rest)) => module.variables.call_site_value(
                last,
                parameter,
                &mut TailCallFrame {
                    chain: self,
                    links: rest,
                },
                budget,
            ),
        }
    }
}

/// A function that left by a tail call: nothing of its state survives the
/// jump but the values its own caller passed it.
struct TailCallFrame<'f, 'c, 'r, 'a, P: InspectionOps> {
    chain: &'f mut Chain<'c, 'r, 'a, P>,
    links: &'f [CallSiteId],
}

const fn discarded() -> VariableRuntimeError {
    unavailable(EntryValueUnavailableReason::DiscardedState)
}

impl<P: InspectionOps> VariableRuntime for TailCallFrame<'_, '_, '_, '_, P> {
    fn register(
        &mut self,
        _register: crate::unwind::DwarfRegister,
    ) -> Result<VariableRegister, VariableRuntimeError> {
        Err(discarded())
    }

    fn call_frame_cfa(&self) -> Result<VirtualAddress, VariableRuntimeError> {
        Err(discarded())
    }

    fn tls_address(&mut self, _offset: u64) -> Result<VirtualAddress, VariableUnavailableReason> {
        Err(VariableUnavailableReason::EntryValue(
            EntryValueUnavailableReason::DiscardedState,
        ))
    }

    fn relocate(&self, address: ImageAddress) -> Result<VirtualAddress, Arc<str>> {
        self.chain
            .callee
            .loaded
            .virtual_address(address)
            .map_err(|error| error.to_string().into())
    }

    fn image_address(&self, address: VirtualAddress) -> Option<ImageAddress> {
        let callee = self.chain.callee;
        callee
            .loaded
            .image_address(address)
            .ok()
            .filter(|address| callee.image.address_range().contains(*address))
    }

    fn read_memory(
        &mut self,
        _address: VirtualAddress,
        _size: usize,
    ) -> Result<Arc<[u8]>, VariableRuntimeError> {
        Err(discarded())
    }

    fn entry_value(
        &mut self,
        parameter: EntryParameter,
        budget: &mut InspectionBudget,
    ) -> Result<u64, VariableRuntimeError> {
        let requester = self.chain.callee.loaded.id;
        self.chain.value(self.links, parameter, requester, budget)
    }
}
