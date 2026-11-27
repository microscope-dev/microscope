//! Maps `ExecutionContext`s (OS threads and runtime tasks) to 32-bit DAP thread IDs.
//!
//! Direct OS thread IDs and task numbers in `1..ALLOCATED` (`2^30`) keep their
//! own numeric values when free; `0`, collisions, wider task IDs, and the
//! truncation placeholder are allocated monotonically in `[ALLOCATED, i32::MAX]`.

use std::collections::HashMap;

use microscope::{ExecutionContext, ThreadId};

use super::protocol::ErrorBody;

const ALLOCATED: i64 = 1 << 30;

const MAX_ID: i64 = i32::MAX as i64;

#[derive(Debug)]
pub struct ThreadHandles {
    next: i64,
    ids: HashMap<ExecutionContext, i64>,
    contexts: HashMap<i64, ExecutionContext>,

    placeholder: Option<i64>,
}

impl Default for ThreadHandles {
    fn default() -> Self {
        Self {
            next: ALLOCATED,
            ids: HashMap::new(),
            contexts: HashMap::new(),
            placeholder: None,
        }
    }
}

impl ThreadHandles {
    pub fn id(&mut self, context: ExecutionContext) -> Result<i64, ErrorBody> {
        if let Some(&id) = self.ids.get(&context) {
            return Ok(id);
        }

        let own = match context {
            ExecutionContext::Thread(thread) => thread.get(),
            ExecutionContext::Task(task) => task.number,
        };

        let id = match i64::try_from(own)
            .ok()
            .filter(|id| (1..ALLOCATED).contains(id) && !self.contexts.contains_key(id))
        {
            Some(id) => id,
            None => self.allocate().ok_or_else(|| {
                ErrorBody::new(format!(
                    "{context} cannot be shown: every client thread id is in use"
                ))
            })?,
        };

        self.ids.insert(context, id);
        self.contexts.insert(id, context);
        Ok(id)
    }

    pub fn placeholder(&mut self) -> Result<i64, ErrorBody> {
        if let Some(id) = self.placeholder {
            return Ok(id);
        }

        let id = self
            .allocate()
            .ok_or_else(|| ErrorBody::new("every client thread id is in use"))?;
        self.placeholder = Some(id);
        Ok(id)
    }

    fn allocate(&mut self) -> Option<i64> {
        let id = self.next;
        (id <= MAX_ID).then(|| {
            self.next += 1;
            id
        })
    }

    pub fn context(&self, id: i64) -> Result<ExecutionContext, ErrorBody> {
        if let Some(&context) = self.contexts.get(&id) {
            return Ok(context);
        }

        if self.placeholder == Some(id) {
            return Err(ErrorBody::new(format!(
                "{id} is not a thread: it counts the threads the list leaves out"
            )));
        }

        if (1..ALLOCATED).contains(&id) {
            return Ok(ExecutionContext::Thread(ThreadId::new(id.unsigned_abs())));
        }

        Err(ErrorBody::new(format!("there is no thread {id}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use microscope::{RuntimeId, TaskId};

    #[test]
    fn contexts_keep_their_own_numbers_unless_taken_and_others_get_lasting_ids() {
        let mut handles = ThreadHandles::default();
        let thread = |id| ExecutionContext::Thread(ThreadId::new(id));
        let task = |runtime, number| {
            ExecutionContext::Task(TaskId {
                runtime: RuntimeId::new(runtime),
                number,
            })
        };

        assert_eq!(handles.id(thread(4321)), Ok(4321));
        assert_eq!(handles.context(4321), Ok(thread(4321)));
        assert_eq!(handles.id(task(0, 7)), Ok(7));
        assert_eq!(handles.context(7), Ok(task(0, 7)));

        assert_eq!(handles.context(99), Ok(thread(99)));

        let taken = handles.id(task(0, 4321)).expect("an id");
        assert!(taken >= ALLOCATED);
        assert_eq!(handles.id(task(0, 4321)), Ok(taken));
        assert_eq!(handles.context(taken), Ok(task(0, 4321)));
        let wide = handles.id(task(1, u64::MAX)).expect("an id");
        assert!(wide > taken);
        assert_eq!(handles.context(wide), Ok(task(1, u64::MAX)));
        let other = handles.id(task(1, 7)).expect("an id");
        assert!(other >= ALLOCATED, "another runtime's task 7");

        let more = handles.placeholder().expect("an id");
        assert_eq!(handles.placeholder(), Ok(more));
        assert!(handles.context(more).is_err());
        assert!(handles.context(0).is_err());
        assert!(handles.context(MAX_ID).is_err());

        handles.next = MAX_ID + 1;
        assert!(handles.id(task(0, u64::MAX)).is_err());
    }
}
