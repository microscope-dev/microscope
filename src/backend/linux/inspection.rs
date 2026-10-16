//! Variable, expression, and global inspection at a stopped snapshot.

use std::collections::BTreeMap;
use std::sync::Arc;

use nix::unistd::Pid;

use std::rc::Rc;

use crate::debug_info::{
    EntryParameter, VariableContext, VariableRegister, VariableRuntime, VariableRuntimeError,
};

use crate::inspection::{InspectionBudget, MAX_INSPECTION_LIMITS};
use crate::protocol::{GlobalVariableQuery, StopId, VariableQuery};
use crate::{
    AddressRange, CodeInstanceId, Error, GlobalVariablePage, GlobalVariableReference, ImageAddress,
    InspectedValue, LoadedGlobalVariableInfo, LoadedModule, MemoryReadCompletion, RegisterSnapshot,
    Result, StackFrameId, TlsUnavailableReason, VariableSnapshot, VariableState,
    VariableUnavailableReason, VirtualAddress,
};

use super::callers::{Callers, FrameAt};
use super::evaluation::StopMachine;
use super::frames::{FrameRegisters, FrameScope, ResolvedFrame, StackRoot};
use super::memory::read_logical_memory;
use super::native::InspectionOps;
use super::registers::{
    Fxsave, x86_64_caller_variable_register, x86_64_general_variable_register,
    x86_64_register_snapshot, x86_64_x87_variable_register, x86_64_xmm_variable_register,
};

use super::tls::TlsModule;
use super::{
    BreakpointSite, Controller, Inferior, MAX_VALUE_CHILD_PAGE_LIMIT, RuntimeModule,
    validate_image_current, validate_public_stop, validate_stopped_thread,
};

impl<P: InspectionOps> Controller<P> {
    /// The inferior, once `stop_id` is its current stop, `pid` one of its
    /// stopped threads, and its image the one the thread executes.
    pub(super) fn stopped_inferior(&self, stop_id: StopId, pid: Pid) -> Result<&Inferior> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        validate_public_stop(inferior, Some(stop_id))?;
        validate_stopped_thread(inferior, pid)?;
        validate_image_current(inferior)?;
        Ok(inferior)
    }

    pub(super) fn registers(
        &self,
        stop_id: StopId,
        root: &StackRoot,
        frame: StackFrameId,
    ) -> Result<RegisterSnapshot> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        validate_public_stop(inferior, Some(stop_id))?;
        validate_stopped_thread(inferior, root.reader())?;
        let native = root
            .thread()
            .map(|pid| self.ptrace.registers(pid))
            .transpose()?;

        // A parked task's innermost registers are those its runtime saved.
        let resolved = if frame == StackFrameId::INNERMOST && native.is_some() {
            None
        } else {
            Some(self.resolve_frame(inferior, root, frame)?)
        };

        if let Some(frame) = resolved
            .as_ref()
            .and_then(|resolved| resolved.frame.as_ref())
            .filter(|frame| !frame.kind.has_registers())
        {
            return Err(if frame.kind.is_suspended() {
                Error::FrameSuspended
            } else {
                Error::FrameInterpreted
            });
        }

        let discarded = crate::unwind::RegisterFile::new([]);
        let caller = match resolved.as_ref().map(|frame| &frame.registers) {
            Some(FrameRegisters::Caller(registers)) => Some(registers),
            Some(FrameRegisters::Discarded) => Some(&discarded),
            Some(FrameRegisters::Thread(_)) | None => None,
        };

        Ok(x86_64_register_snapshot(
            self.revision,
            root.context,
            self.module_image.target(),
            native.as_ref(),
            caller,
        ))
    }

    /// Returns the module describing a frame's function, the frame's address
    /// in its image, and the frame's source scope, or `None` when no single
    /// function scope applies: code without debug information, or an
    /// innermost frame with no single inline chain.
    pub(super) fn frame_scope(
        &self,
        frame: &ResolvedFrame,
    ) -> Option<(&RuntimeModule, ImageAddress, Option<CodeInstanceId>)> {
        let (module, address) = frame.code?;
        let selected = match frame.scope {
            // A suspended frame's variables are in its future, which no
            // scope of its function's code describes.
            FrameScope::Unavailable | FrameScope::Suspended { .. } => return None,
            FrameScope::Function => None,
            FrameScope::Inline(instance) => Some(instance),
        };

        Some((self.modules.get(&module)?, address, selected))
    }

    /// Reads a frame's registers and memory for values `module` describes.
    pub(super) fn frame_runtime<'a>(
        &'a self,
        inferior: &'a Inferior,
        root: &StackRoot,
        frame: &'a ResolvedFrame,
        module: &'a RuntimeModule,
    ) -> LinuxVariableRuntime<'a, P> {
        LinuxVariableRuntime {
            ptrace: &self.ptrace,
            pid: root.reader(),
            thread: root.thread(),
            loaded_module: module.loaded,
            image_range: module.image.address_range(),
            breakpoints: &inferior.breakpoints,
            registers: &frame.registers,
            floating: None,
            cfa: frame.cfa.clone(),
            tls: module.tls,
            frame: FrameAt {
                activation: frame.activation,
                code: frame.code,
                depth: 0,
            },
            callers: Some(Callers::new(self, inferior, root.clone())),
            below_stack_pointer: frame.below_stack_pointer.clone(),
        }
    }

    pub(super) fn variables(
        &self,
        stop_id: StopId,
        root: &StackRoot,
        frame: StackFrameId,
        query: &VariableQuery,
        limits: crate::InspectionLimits,
    ) -> Result<VariableSnapshot> {
        validate_inspection_limits(limits)?;
        let mut budget = InspectionBudget::new(limits);
        let inferior = self.stopped_root(stop_id, root)?;

        // An inline frame sees its instance's variables; a physical frame,
        // its function's own.
        let resolved = self.resolve_frame(inferior, root, frame)?;
        let scope = self.frame_scope(&resolved);
        let suspended = matches!(resolved.scope, FrameScope::Suspended { .. });
        let inspect_locals = |budget: &mut InspectionBudget| {
            if suspended {
                return self.async_variables(stop_id, root, &resolved, query, budget);
            }

            let (module, address, selected) = scope.ok_or(Error::VariableContextUnsupported)?;
            let mut runtime = self.frame_runtime(inferior, root, &resolved, module);
            module.variables.inspect(
                address,
                selected,
                query,
                variable_context(stop_id, root.context, frame, module, Some(address)),
                &mut runtime,
                budget,
            )
        };

        let mut variables = match query {
            VariableQuery::Global(global) => {
                vec![self.inspect_loaded_global(inferior, root, &resolved, *global, &mut budget)?]
            }
            VariableQuery::All => {
                let mut variables = inspect_locals(&mut budget)?;

                // A step out's stop shows what the finished function
                // returned beside the frame it returned to.
                if let Some(returned) = inferior
                    .public_stop
                    .as_ref()
                    .and_then(|stop| stop.returned.as_ref())
                {
                    variables.extend(self.returned_variables(
                        returned,
                        stop_id,
                        root,
                        frame,
                        &mut budget,
                    )?);
                }

                variables
            }
            VariableQuery::Name(name) => {
                let local = if scope.is_some() || suspended {
                    inspect_locals(&mut budget)
                } else {
                    Err(Error::VariableNotFound(name.clone()))
                };

                match local {
                    Ok(variables) => variables,
                    Err(Error::VariableNotFound(_)) => {
                        let global = self.loaded_global_named(name)?;
                        vec![self.inspect_loaded_global(
                            inferior,
                            root,
                            &resolved,
                            global,
                            &mut budget,
                        )?]
                    }
                    Err(error) => return Err(error),
                }
            }
        };

        // Views present what the provider read, with the same budget.
        {
            let scope = self.frame_for(inferior, stop_id, root, &resolved);
            let mut machine = StopMachine::new(&scope, &mut budget, true);
            for variable in &mut variables {
                machine.present_state(variable.type_info.clone(), &mut variable.state)?;
            }
        }

        Ok(VariableSnapshot {
            revision: self.revision,
            stop_id,
            context: root.context,
            stack_frame: frame,
            frame: resolved.presented,
            frame_address: resolved.cfa.ok(),
            target: self.module_image.target(),
            variables: variables.into(),
            completion: budget.completion(),
            usage: budget.usage(),
        })
    }

    /// A capability's stack and module, once its stop is current. Nothing
    /// a stale capability names is consulted.
    fn capability_module(
        &self,
        stop_id: StopId,
        context: crate::ExecutionContext,
        module: crate::ModuleId,
        image: crate::ModuleImageId,
    ) -> Result<(&Inferior, StackRoot, &RuntimeModule)> {
        let root = self.stack_root(stop_id, context)?;
        let inferior = self.stopped_root(stop_id, &root)?;
        let module = self
            .modules
            .get(&module)
            .ok_or(Error::ModuleNotLoaded(module))?;
        if module.loaded.image != image {
            return Err(Error::StaleModuleImage);
        }

        Ok((inferior, root, module))
    }

    /// The one global named `name` among the loaded modules.
    fn loaded_global_named(&self, name: &str) -> Result<GlobalVariableReference> {
        let mut matches = Vec::new();
        for module in self.modules.values() {
            match module.image.global_named(name) {
                Ok(global) => matches.push(GlobalVariableReference {
                    module: module.loaded.id,
                    image: module.loaded.image,
                    variable: global.id,
                }),
                Err(Error::VariableNotFound(_)) => {}
                Err(error) => return Err(error),
            }
        }

        match matches.as_slice() {
            [global] => Ok(*global),
            [] => Err(Error::VariableNotFound(name.to_owned())),
            _ => Err(Error::AmbiguousLoadedGlobalVariable {
                selector: name.to_owned(),
                candidates: matches,
            }),
        }
    }

    pub(super) fn inspect_loaded_global(
        &self,
        inferior: &Inferior,
        root: &StackRoot,
        frame: &ResolvedFrame,
        global: GlobalVariableReference,
        budget: &mut InspectionBudget,
    ) -> Result<crate::Variable> {
        let module = self
            .modules
            .get(&global.module)
            .ok_or(Error::ModuleNotLoaded(global.module))?;
        if module.loaded.image != global.image {
            return Err(Error::StaleModuleImage);
        }

        let context_address = global_context_address(frame, module);
        let mut runtime = self.frame_runtime(inferior, root, frame, module);
        let mut variable = module.variables.inspect_global(
            global.variable,
            context_address,
            variable_context(
                public_stop_id(inferior),
                root.context,
                frame.id,
                module,
                context_address,
            ),
            &mut runtime,
            budget,
        )?;
        variable.global = Some(global);
        Ok(variable)
    }

    /// The elements `start..end` of an inspected array or slice, by source
    /// index.
    pub(super) fn range_page(
        &self,
        stop_id: StopId,
        inspected: &InspectedValue,
        start: i128,
        end: i128,
        budget: &mut InspectionBudget,
    ) -> Result<crate::ValueChildPage> {
        let length = validate_range_length(start, end)?;
        let empty = |offset, total| crate::ValueChildPage {
            stop_id,
            offset,
            total,
            children: Arc::from([]),
            completion: budget.completion(),
            usage: budget.usage(),
        };

        let invalid = |why: String| Err(Error::InvalidValueRange(why.into()));
        let (lower_bound, count, reference) = match &inspected.state {
            VariableState::Available {
                value: crate::VariableValue::Array { dimensions, .. },
                children: crate::ValueChildren::Available(reference),
                ..
            } => {
                let [dimension] = dimensions.as_ref() else {
                    return invalid("ranges currently require a one-dimensional array".into());
                };

                (dimension.lower_bound, dimension.count, reference)
            }

            VariableState::Available {
                value: crate::VariableValue::Slice { length, .. },
                children: crate::ValueChildren::Available(reference),
                ..
            } => (0, *length, reference),
            VariableState::Available { .. } => {
                return Err(Error::IndexAccessOnNonIndexable {
                    type_name: inspected
                        .type_info
                        .as_ref()
                        .map_or_else(|| Arc::from("<unknown>"), |info| Arc::clone(&info.name)),
                });
            }
            VariableState::Unavailable(VariableUnavailableReason::InspectionLimit(_)) => {
                return Ok(empty(0, 0));
            }
            VariableState::Unavailable(reason) => {
                return invalid(format!("the selected aggregate is unavailable: {reason}"));
            }
            VariableState::Malformed(reason) => {
                return invalid(format!(
                    "the selected aggregate is malformed: {}",
                    reason.description
                ));
            }
            VariableState::Invalid { reason, .. } => {
                return invalid(format!(
                    "the selected aggregate has an invalid value: {reason}"
                ));
            }
        };

        // Once `start` is in bounds, `end`, which is not below it, can only
        // be past the last element.
        let relative = |index: i128| {
            index
                .checked_sub(lower_bound)
                .and_then(|index| u64::try_from(index).ok())
                .filter(|index| *index <= count)
        };

        let out_of_bounds = |index| Error::ValueIndexOutOfBounds {
            index,
            lower_bound,
            count,
        };

        let offset = relative(start).ok_or_else(|| out_of_bounds(start))?;
        relative(end).ok_or_else(|| out_of_bounds(end.checked_sub(1).unwrap_or(end)))?;
        if length == 0 {
            return Ok(empty(offset, count));
        }

        self.value_children_with_budget(
            reference,
            &crate::ValueChildQuery {
                offset,
                limit: u32::try_from(length)
                    .map_err(|_| Error::internal("a validated range length fits u32"))?,
            },
            budget,
        )
    }

    pub(super) fn dereference(
        &self,
        reference: &crate::DereferenceReference,
        limits: crate::InspectionLimits,
    ) -> Result<crate::DereferencedValue> {
        validate_inspection_limits(limits)?;
        let mut budget = InspectionBudget::new(limits);
        let (inferior, root, module) = self.capability_module(
            reference.stop_id,
            reference.context,
            reference.module,
            reference.image,
        )?;

        // The capability evaluates in the frame that produced it, whichever
        // frame is selected now.
        let frame = self.resolve_frame(inferior, &root, reference.frame)?;
        let mut runtime = self.frame_runtime(inferior, &root, &frame, module);
        let mut value = module
            .variables
            .dereference(reference, &mut runtime, &mut budget)?;
        let scope = self.frame_for(inferior, reference.stop_id, &root, &frame);
        let mut machine = StopMachine::new(&scope, &mut budget, true);
        machine.present_state(Some(value.type_info.clone()), &mut value.state)?;
        value.completion = budget.completion();
        value.usage = budget.usage();
        Ok(value)
    }

    pub(super) fn value_children(
        &self,
        reference: &crate::ValueChildrenReference,
        query: &crate::ValueChildQuery,
        limits: crate::InspectionLimits,
    ) -> Result<crate::ValueChildPage> {
        validate_inspection_limits(limits)?;
        let mut budget = InspectionBudget::new(limits);
        self.value_children_with_budget(reference, query, &mut budget)
    }

    pub(super) fn value_children_with_budget(
        &self,
        reference: &crate::ValueChildrenReference,
        query: &crate::ValueChildQuery,
        budget: &mut InspectionBudget,
    ) -> Result<crate::ValueChildPage> {
        let (inferior, root, module) = self.capability_module(
            reference.stop_id,
            reference.context,
            reference.module,
            reference.image,
        )?;
        if !(1..=MAX_VALUE_CHILD_PAGE_LIMIT).contains(&query.limit) {
            return Err(Error::InvalidValueChildPageLimit(query.limit));
        }

        if let Some(view) = &reference.view {
            return self.view_children(reference, view, query.offset, query.limit, budget);
        }

        let frame = self.resolve_frame(inferior, &root, reference.frame)?;
        let mut runtime = self.frame_runtime(inferior, &root, &frame, module);
        let mut page = module.variables.value_children(
            reference,
            query.offset,
            query.limit,
            &mut runtime,
            budget,
        )?;
        let scope = self.frame_for(inferior, reference.stop_id, &root, &frame);
        let mut machine = StopMachine::new(&scope, budget, true);
        let mut children = page.children.to_vec();
        for child in &mut children {
            // A base-class subobject is part of an object, not one of the
            // type it dynamically is.
            machine.dynamic = !matches!(child.relationship, crate::ValueChildRelationship::Base(_));
            machine.present_state(Some(child.type_info.clone()), &mut child.state)?;
        }

        page.children = children.into();
        page.completion = budget.completion();
        page.usage = budget.usage();
        Ok(page)
    }

    pub(super) fn globals(&self, query: &GlobalVariableQuery) -> Result<GlobalVariablePage> {
        if !(1..=256).contains(&query.limit) {
            return Err(Error::InvalidGlobalPageLimit(query.limit));
        }

        let running = self.inferior.is_some();
        let mut matches = self
            .modules
            .values()
            .flat_map(|module| {
                module
                    .image
                    .globals()
                    .filter(|global| {
                        query.filter.as_ref().is_none_or(|filter| {
                            global.name.contains(filter)
                                || global.qualified_name.contains(filter)
                                || global
                                    .linkage_name
                                    .as_ref()
                                    .is_some_and(|linkage| linkage.contains(filter))
                        })
                    })
                    .map(|global| LoadedGlobalVariableInfo {
                        module: running.then_some(module.loaded),
                        image: module.image.id(),
                        variable: global,
                    })
            })
            .collect::<Vec<_>>();
        matches.sort_by(|left, right| {
            left.variable
                .qualified_name
                .cmp(&right.variable.qualified_name)
                .then_with(|| left.image.cmp(&right.image))
                .then_with(|| left.variable.id.cmp(&right.variable.id))
        });
        let total = matches.len() as u64;
        let start = usize::try_from(query.offset)
            .unwrap_or(usize::MAX)
            .min(matches.len());
        let end = start
            .saturating_add(query.limit as usize)
            .min(matches.len());
        let variables = matches[start..end].to_vec().into();
        Ok(GlobalVariablePage {
            revision: self.revision,
            offset: query.offset,
            total,
            variables,
        })
    }
}

pub(super) const fn variable_context(
    stop_id: StopId,
    context: crate::ExecutionContext,
    frame: StackFrameId,
    module: &RuntimeModule,
    address: Option<ImageAddress>,
) -> VariableContext {
    VariableContext {
        stop_id,
        context,
        frame,
        module: module.loaded.id,
        image: module.loaded.image,
        address,
    }
}

/// The stop whose capabilities inspection hands out: the public stop, or
/// none at all for values read at a breakpoint hit.
fn public_stop_id(inferior: &Inferior) -> StopId {
    inferior
        .public_stop
        .as_ref()
        .map_or_else(|| StopId::new(0), |stop| stop.id)
}

/// The instruction context that selects a global's range-gated location
/// entries. A frame executing another module gives none, so the provider
/// refuses to guess rather than resolving against an unrelated address.
pub(super) fn global_context_address(
    frame: &ResolvedFrame,
    module: &RuntimeModule,
) -> Option<ImageAddress> {
    frame
        .code
        .filter(|(code_module, _)| *code_module == module.loaded.id)
        .map(|(_, address)| address)
}

pub(super) struct LinuxVariableRuntime<'a, P: InspectionOps> {
    pub(super) ptrace: &'a P,

    /// A stopped thread through which the process's memory is read.
    pub(super) pid: Pid,

    /// The thread whose thread-local storage the frame sees; none for a
    /// parked task.
    pub(super) thread: Option<Pid>,
    pub(super) loaded_module: LoadedModule,

    /// The image addresses the module's segments span.
    pub(super) image_range: AddressRange<ImageAddress>,
    pub(super) breakpoints: &'a BTreeMap<VirtualAddress, BreakpointSite>,
    pub(super) registers: &'a FrameRegisters,
    pub(super) floating: Option<std::result::Result<Fxsave, Arc<str>>>,
    pub(super) cfa: std::result::Result<VirtualAddress, VariableRuntimeError>,
    pub(super) tls: Option<TlsModule>,
    pub(super) frame: FrameAt,

    /// The stack's activations, which entry values find callers among.
    pub(super) callers: Option<Rc<Callers<'a, P>>>,

    /// Where the frame's task stack is below its stack pointer, which the
    /// frame's values may not read.
    pub(super) below_stack_pointer: Option<std::ops::Range<u64>>,
}

impl<P: InspectionOps> VariableRuntime for LinuxVariableRuntime<'_, P> {
    fn function_at(&self, address: VirtualAddress) -> Option<Arc<str>> {
        self.callers.as_ref()?.function_at(address)
    }

    fn register(
        &mut self,
        register: crate::unwind::DwarfRegister,
    ) -> std::result::Result<VariableRegister, VariableRuntimeError> {
        let native = match self.registers {
            FrameRegisters::Thread(native) => native,
            FrameRegisters::Caller(registers) => {
                return x86_64_caller_variable_register(registers, register);
            }
            FrameRegisters::Discarded => {
                return Err(VariableUnavailableReason::CallFrameUnavailable(
                    crate::CallFrameUnavailableReason::TailCall,
                )
                .into());
            }
        };

        if let Some(value) = x86_64_general_variable_register(native, register) {
            return Ok(value);
        }

        if (17..=40).contains(&register.number()) {
            let floating = self.floating.get_or_insert_with(|| {
                self.ptrace
                    .floating_registers(self.pid)
                    .map_err(|error| Arc::from(error.to_string()))
            });
            return floating
                .as_ref()
                .map_err(|error| {
                    VariableRuntimeError::Unavailable(
                        VariableUnavailableReason::RegisterUnavailable(
                            format!("floating-point register {register} ({error})").into(),
                        ),
                    )
                })
                .map(|floating| {
                    if register.number() <= 32 {
                        x86_64_xmm_variable_register(floating, register)
                    } else {
                        x86_64_x87_variable_register(floating, register)
                    }
                });
        }

        Err(VariableRuntimeError::Unavailable(
            crate::UnsupportedVariableFeature::RegisterClass.into(),
        ))
    }

    fn call_frame_cfa(&self) -> std::result::Result<VirtualAddress, VariableRuntimeError> {
        self.cfa.clone()
    }

    fn tls_address(
        &mut self,
        offset: u64,
    ) -> std::result::Result<VirtualAddress, VariableUnavailableReason> {
        let module = self.tls.ok_or(VariableUnavailableReason::TlsUnavailable(
            TlsUnavailableReason::ModuleIdentityUnavailable,
        ))?;
        let thread = self
            .thread
            .ok_or(VariableUnavailableReason::TlsUnavailable(
                TlsUnavailableReason::NoThread,
            ))?;
        self.ptrace
            .tls_address(thread, module, offset)
            .map_err(|reason| {
                VariableUnavailableReason::TlsUnavailable(TlsUnavailableReason::LookupFailed(
                    reason,
                ))
            })
    }

    fn relocate(&self, address: ImageAddress) -> std::result::Result<VirtualAddress, Arc<str>> {
        self.loaded_module
            .virtual_address(address)
            .map_err(|error| error.to_string().into())
    }

    fn image_address(&self, address: VirtualAddress) -> Option<ImageAddress> {
        self.loaded_module
            .image_address(address)
            .ok()
            .filter(|address| self.image_range.contains(*address))
    }

    fn read_memory(
        &mut self,
        address: VirtualAddress,
        size: usize,
    ) -> std::result::Result<Arc<[u8]>, VariableRuntimeError> {
        let end = address
            .get()
            .saturating_add(u64::try_from(size).unwrap_or(u64::MAX));
        if let Some(below) = &self.below_stack_pointer
            && address.get() < below.end
            && below.start < end
        {
            return Err(VariableRuntimeError::Unavailable(
                VariableUnavailableReason::BelowStackPointer { address },
            ));
        }

        let read = read_logical_memory(self.ptrace, self.pid, self.breakpoints, address, size)
            .map_err(|error| match error {
                // A location computed from a meaningless frame base, as
                // before a prologue, can wrap the address space: that one
                // value is unavailable.
                Error::AddressOverflow => {
                    VariableRuntimeError::Unavailable(VariableUnavailableReason::ValueAccess(
                        crate::ValueAccessUnavailableReason::AddressOverflow,
                    ))
                }
                error => VariableRuntimeError::Fatal(error.to_string().into()),
            })?;
        match read.completion {
            MemoryReadCompletion::Complete => Ok(Arc::from(read.bytes)),
            MemoryReadCompletion::Incomplete { next_address, .. } => Err(
                VariableRuntimeError::Unavailable(VariableUnavailableReason::MemoryInaccessible {
                    address,
                    requested: u64::try_from(size).unwrap_or(u64::MAX),
                    completed: u64::try_from(read.bytes.len()).unwrap_or(u64::MAX),
                    next_address,
                }),
            ),
        }
    }

    fn entry_value(
        &mut self,
        parameter: EntryParameter,
        budget: &mut InspectionBudget,
    ) -> std::result::Result<u64, VariableRuntimeError> {
        let callers = self
            .callers
            .as_ref()
            .ok_or(VariableRuntimeError::Unavailable(
                VariableUnavailableReason::EntryValue(crate::EntryValueUnavailableReason::NoCaller),
            ))?;
        callers.entry_value(self.frame, self.loaded_module.id, parameter, budget)
    }
}

/// The number of elements `start..end` holds, at most a page.
fn validate_range_length(start: i128, end: i128) -> Result<i128> {
    let length = end
        .checked_sub(start)
        .filter(|length| *length >= 0)
        .ok_or_else(|| Error::InvalidValueRange("the range end precedes its start".into()))?;
    if length > i128::from(MAX_VALUE_CHILD_PAGE_LIMIT) {
        return Err(Error::InvalidValueRange(
            format!("a range may contain at most {MAX_VALUE_CHILD_PAGE_LIMIT} elements").into(),
        ));
    }

    Ok(length)
}

pub(super) fn validate_inspection_limits(limits: crate::InspectionLimits) -> Result<()> {
    for (resource, value, maximum) in [
        (
            crate::InspectionLimit::Variables,
            limits.variables,
            MAX_INSPECTION_LIMITS.variables,
        ),
        (
            crate::InspectionLimit::ValueNodes,
            limits.value_nodes,
            MAX_INSPECTION_LIMITS.value_nodes,
        ),
        (
            crate::InspectionLimit::AggregateDepth,
            limits.aggregate_depth,
            MAX_INSPECTION_LIMITS.aggregate_depth,
        ),
        (
            crate::InspectionLimit::MemoryReads,
            limits.memory_reads,
            MAX_INSPECTION_LIMITS.memory_reads,
        ),
        (
            crate::InspectionLimit::MemoryBytes,
            limits.memory_bytes,
            MAX_INSPECTION_LIMITS.memory_bytes,
        ),
        (
            crate::InspectionLimit::ExpressionWork,
            limits.expression_work,
            MAX_INSPECTION_LIMITS.expression_work,
        ),
    ] {
        if value == 0 || value > maximum {
            return Err(Error::InvalidInspectionLimit {
                resource,
                value,
                maximum,
            });
        }
    }

    Ok(())
}
