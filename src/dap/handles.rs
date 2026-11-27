//! Per-stop integer reference table for DAP frames, variable containers, and targets.
//!
//! Reference numbers stay within signed 32-bit bounds for client compatibility and
//! monotonically increase across `clear()` calls so stale handles never alias new ones.

use std::collections::HashMap;
use std::sync::Arc;

use super::protocol::ErrorBody;
use microscope::{
    DereferenceReference, ExecutionContext, StackFrameId, StopContext, ValueChildrenReference,
    VariableKind,
};

/// Cap at `i32::MAX` because many DAP clients store references in signed 32-bit ints.
const MAX_REFERENCE: i64 = i32::MAX as i64;

#[derive(Debug, Clone)]
pub enum Variables {
    Scope {
        context: StopContext,
        kind: VariableKind,
    },

    /// A frame's registers.
    Registers { context: StopContext },

    Statics {
        context: StopContext,
        module: microscope::ModuleId,
        file: microscope::SourceFileId,
    },

    Children {
        context: StopContext,
        reference: Arc<ValueChildrenReference>,
        path: Option<microscope::Expression>,

        indexed: Option<bool>,
    },

    Pointee {
        context: StopContext,
        reference: Box<DereferenceReference>,
        name: Arc<str>,
        path: Option<microscope::Expression>,
    },

    Range {
        context: StopContext,
        expression: microscope::Expression,
    },
}

impl Variables {
    pub const fn indexed(&self) -> Option<bool> {
        match self {
            Self::Scope { .. } | Self::Registers { .. } | Self::Statics { .. } => Some(false),
            Self::Children { indexed, .. } => *indexed,
            Self::Range { .. } => Some(true),
            Self::Pointee { .. } => None,
        }
    }

    pub const fn context(&self) -> StopContext {
        match self {
            Self::Scope { context, .. }
            | Self::Registers { context }
            | Self::Statics { context, .. }
            | Self::Children { context, .. }
            | Self::Pointee { context, .. }
            | Self::Range { context, .. } => *context,
        }
    }
}

#[derive(Debug)]
pub struct References {
    next: i64,
    frames: HashMap<i64, StopContext>,
    frame_ids: HashMap<(ExecutionContext, StackFrameId), i64>,
    variables: HashMap<i64, Variables>,

    paths: HashMap<(i64, String), (StopContext, microscope::Expression)>,

    locations: HashMap<i64, Location>,

    targets: HashMap<i64, microscope::BreakpointSpec>,

    calls: HashMap<i64, microscope::VirtualAddress>,
}

#[derive(Debug, Clone)]
pub enum Location {
    Code(u64),

    Declared {
        module: microscope::ModuleId,
        location: microscope::SourceLocation,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the session used every one of its {MAX_REFERENCE} references")]
pub struct Exhausted;

impl From<Exhausted> for ErrorBody {
    fn from(exhausted: Exhausted) -> Self {
        Self::new(exhausted.to_string())
    }
}

impl Default for References {
    fn default() -> Self {
        Self {
            next: 1,
            frames: HashMap::new(),
            frame_ids: HashMap::new(),
            variables: HashMap::new(),
            paths: HashMap::new(),
            locations: HashMap::new(),
            targets: HashMap::new(),
            calls: HashMap::new(),
        }
    }
}

impl References {
    const fn allocate(&mut self) -> Result<i64, Exhausted> {
        if self.next > MAX_REFERENCE {
            return Err(Exhausted);
        }

        let id = self.next;
        self.next += 1;
        Ok(id)
    }

    pub fn frame(&mut self, context: StopContext) -> Result<i64, Exhausted> {
        if let Some(&id) = self.frame_ids.get(&(context.execution, context.frame)) {
            return Ok(id);
        }

        let id = self.allocate()?;
        self.frames.insert(id, context);
        self.frame_ids
            .insert((context.execution, context.frame), id);
        Ok(id)
    }

    pub const fn label(&mut self) -> Result<i64, Exhausted> {
        self.allocate()
    }

    pub fn variables(&mut self, variables: Variables) -> Result<i64, Exhausted> {
        let id = self.allocate()?;
        self.variables.insert(id, variables);
        Ok(id)
    }

    pub fn location(&mut self, location: Location) -> Result<i64, Exhausted> {
        let id = self.allocate()?;
        self.locations.insert(id, location);
        Ok(id)
    }

    pub fn location_of(&self, id: i64) -> Option<&Location> {
        self.locations.get(&id)
    }

    pub fn goto_target(&mut self, location: microscope::BreakpointSpec) -> Result<i64, Exhausted> {
        let id = self.allocate()?;
        self.targets.insert(id, location);
        Ok(id)
    }

    pub fn target_of(&self, id: i64) -> Option<&microscope::BreakpointSpec> {
        self.targets.get(&id)
    }

    pub fn step_target(&mut self, call: microscope::VirtualAddress) -> Result<i64, Exhausted> {
        let id = self.allocate()?;
        self.calls.insert(id, call);
        Ok(id)
    }

    pub fn call_of(&self, id: i64) -> Option<microscope::VirtualAddress> {
        self.calls.get(&id).copied()
    }

    pub fn frame_context(&self, id: i64) -> Option<StopContext> {
        self.frames.get(&id).copied()
    }

    pub fn variables_of(&self, id: i64) -> Option<&Variables> {
        self.variables.get(&id)
    }

    pub fn record_path(
        &mut self,
        list: i64,
        name: String,
        context: StopContext,
        path: microscope::Expression,
    ) {
        self.paths.insert((list, name), (context, path));
    }

    pub fn child_path(
        &self,
        list: i64,
        name: &str,
    ) -> Option<(StopContext, microscope::Expression)> {
        if let Some(recorded) = self.paths.get(&(list, name.to_owned())) {
            return Some(recorded.clone());
        }

        let row = |parent: &microscope::Expression, name: &str| {
            if name.starts_with('[') {
                let indices: Option<Vec<i128>> = name
                    .strip_prefix('[')?
                    .strip_suffix(']')?
                    .split("][")
                    .map(|index| index.parse().ok())
                    .collect();
                parent.indexed(&indices?)
            } else {
                parent.member(name)
            }
        };

        match self.variables.get(&list)? {
            Variables::Scope { context, .. } => {
                Some((*context, microscope::Expression::name(name)?))
            }
            Variables::Children {
                context,
                path: Some(path),
                ..
            } => Some((*context, row(path, name)?)),
            Variables::Pointee {
                context,
                name: owner,
                path: Some(path),
                ..
            } => {
                let pointee = path.dereferenced()?;
                if name == format!("*{owner}") {
                    Some((*context, pointee))
                } else {
                    Some((*context, row(&pointee, name)?))
                }
            }

            Variables::Range {
                context,
                expression,
            } => Some((*context, row(&expression.range_base()?, name)?)),
            Variables::Registers { context } => Some((
                *context,
                microscope::Expression::parse(&format!("${name}")).ok()?,
            )),
            _ => None,
        }
    }

    pub fn clear(&mut self) {
        self.frames.clear();
        self.frame_ids.clear();
        self.variables.clear();
        self.paths.clear();
        self.locations.clear();
        self.targets.clear();
        self.calls.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use microscope::{StopId, ThreadId};

    fn context(stop: u64, thread: u64) -> StopContext {
        StopContext {
            stop: StopId::new(stop),
            execution: ExecutionContext::Thread(ThreadId::new(thread)),
            frame: StackFrameId::INNERMOST,
        }
    }

    #[test]
    fn exhaustion_is_an_error_rather_than_a_wrap_around() {
        let mut references = References {
            next: MAX_REFERENCE,
            ..References::default()
        };

        assert_eq!(references.frame(context(1, 1)), Ok(MAX_REFERENCE));
        assert_eq!(references.frame(context(1, 2)), Err(Exhausted));
        assert_eq!(
            references
                .variables(Variables::Registers {
                    context: context(1, 1)
                })
                .err(),
            Some(Exhausted)
        );
    }
}
