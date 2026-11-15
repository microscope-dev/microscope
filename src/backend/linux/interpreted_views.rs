use std::sync::Arc;

use super::evaluation::{Place, StopMachine, ViewBound};
use super::native::InspectionOps;
use super::presentation::{
    MAX_DEPTH, ModuleScope, drawings, interpreted_scan_key, view_presentation,
};
use super::runtimes::BoundRuntime;
use crate::eval::target::Stop;
use crate::runtime_model::{InterpretedHandle, InterpretedSlot};
use crate::view::run::Failure;
use crate::view::{Choice, ViewSet};
use crate::{Presentation, RuntimeId, TypeInfo, ValueChildren, VariableState};

#[derive(Debug)]
pub(super) struct InterpretedView {
    pub(super) runtime: RuntimeId,
    pub(super) object: InterpretedHandle,
    pub(super) bound: Arc<ViewBound>,
}

impl<P: InspectionOps> StopMachine<'_, '_, P> {
    pub(super) fn present_interpreted(
        &mut self,
        runtime: &BoundRuntime,
        slot: InterpretedSlot,
    ) -> Result<(Option<TypeInfo>, VariableState), Stop> {
        let frame = self.frame;
        let at = (frame.stop_id, frame.root().context, frame.resolved().id);
        let viewed = match &slot {
            InterpretedSlot::Value(value) => Some((
                (value.object, value.address),
                value.class,
                (value.elements(), value.attributes),
            )),
            _ => None,
        };

        let (type_info, mut state) = super::interpreted::present_slot(at, runtime, slot);
        let Some(((object, address), class, children)) = viewed else {
            return Ok((type_info, state));
        };

        let controller = frame.controller;
        if !controller.views.enabled || self.depth >= MAX_DEPTH {
            return Ok((type_info, state));
        }

        let Some(bound) = self.class_view(runtime, class).bound.clone() else {
            return Ok((type_info, state));
        };

        let VariableState::Available {
            presentation: Some(stored),
            text,
            ..
        } = &mut state
        else {
            return Ok((type_info, state));
        };

        let this = Place::Interpreted {
            runtime: runtime.id,
            object,
        };
        let result = controller.views.scan(
            frame.stop_id,
            interpreted_scan_key(&bound, runtime.id, object),
            |checkpoints| {
                self.in_share(|machine| {
                    crate::view::run::present(&bound, machine, this, checkpoints)
                })
            },
        );

        let name = crate::view::name_of(&bound.view);
        match result {
            Ok(presented) => {
                let raw = super::interpreted::children_reference(
                    at,
                    runtime,
                    (object, address),
                    children,
                );
                let shown = presented.text.clone();
                let detail = (presented.summary == *stored.summary)
                    .then(|| stored.detail.clone())
                    .flatten();
                let mut presentation = view_presentation(name, &bound, &raw, presented);
                presentation.detail = detail;
                *stored = Arc::new(through_runtime(presentation, (runtime.id, object), &bound));
                *text = shown.map(Arc::new);
            }

            Err(Failure::Problem(problem)) => {
                let mut presentation = (**stored).clone();
                presentation.summary =
                    format!("{} <view {name}: {problem}>", presentation.summary).into();
                presentation.view = name;
                presentation.problem = Some(problem);
                *stored = Arc::new(presentation);
            }
            Err(Failure::Debugger(error)) => return Err(Stop::Failed(error)),
        }

        Ok((type_info, state))
    }

    pub(super) fn interpreted_state(
        &mut self,
        runtime: &BoundRuntime,
        slot: InterpretedSlot,
    ) -> crate::Result<(Option<TypeInfo>, VariableState)> {
        match self.present_interpreted(runtime, slot) {
            Ok(presented) => Ok(presented),
            Err(Stop::Missing(state)) => Ok((None, *state)),
            Err(stop) => Err(super::presentation::stopped(stop)),
        }
    }

    fn class_view(
        &mut self,
        runtime: &BoundRuntime,
        class: InterpretedHandle,
    ) -> Arc<Choice<super::evaluation::StopStep>> {
        let frame = self.frame;
        let controller = frame.controller;
        if let Some(choice) = controller
            .views
            .class_choice(frame.stop_id, runtime.id, class)
        {
            return choice;
        }

        let budget = &mut *self.budget;
        let lineage = frame
            .with_stop(runtime, |stop| {
                runtime.model.interpreted_lineage(stop, class, budget)
            })
            .ok();
        let choice = Arc::new(
            match (controller.modules.get(&runtime.module.id), lineage) {
                (Some(module), Some(lineage)) => crate::view::choose_dynamic(
                    &[&controller.views.set, &ViewSet::built_in()],
                    &lineage,
                    &ModuleScope { controller, module },
                ),
                _ => Choice::default(),
            },
        );

        controller
            .views
            .keep_class_choice(frame.stop_id, runtime.id, class, Arc::clone(&choice));
        choice
    }
}

fn through_runtime(
    mut presentation: Presentation,
    (runtime, object): (RuntimeId, InterpretedHandle),
    bound: &Arc<ViewBound>,
) -> Presentation {
    if let ValueChildren::Available(reference) = &presentation.children {
        let mut reference = (**reference).clone();
        if let Some(view) = &mut reference.view {
            view.bound = Some(Arc::new(InterpretedView {
                runtime,
                object,
                bound: Arc::clone(bound),
            }));
        }

        let children = ValueChildren::Available(Arc::new(reference));
        presentation.visualizers = drawings(bound, &children);
        presentation.children = children;
    }

    presentation
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::super::presentation::Views;
    use crate::runtime_model::InterpretedHandle;
    use crate::view::Choice;
    use crate::{RuntimeId, StopId};

    #[test]
    fn a_class_view_is_kept_for_its_stop_alone() {
        let views = Views::default();
        let (runtime, class) = (RuntimeId::new(1), InterpretedHandle::new(0x1000));
        let (first, second) = (StopId::new(1), StopId::new(2));
        assert!(views.class_choice(first, runtime, class).is_none());
        views.keep_class_choice(first, runtime, class, Arc::new(Choice::default()));
        assert!(views.class_choice(first, runtime, class).is_some());
        assert!(views.class_choice(second, runtime, class).is_none());

        views.keep_class_choice(first, runtime, class, Arc::new(Choice::default()));
        assert!(views.class_choice(second, runtime, class).is_none());

        views.keep_class_choice(second, runtime, class, Arc::new(Choice::default()));
        views.forget_scans();
        assert!(views.class_choice(second, runtime, class).is_none());
    }
}
