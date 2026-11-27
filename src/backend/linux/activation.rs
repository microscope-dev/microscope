//! Frames on a movable runtime task stack are recorded as offsets below the stack top
//! rather than raw addresses, so step-over and step-out comparisons remain valid even if
//! the runtime reallocates the stack or migrates the task to another OS thread.

use nix::unistd::Pid;

use crate::{TaskId, VirtualAddress};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StackOwner {
    Thread(Pid),

    Task(TaskId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Depth {
    Address(u64),

    BelowTop(u64),
}

impl Depth {
    const fn is_deeper_than(self, other: Self) -> bool {
        match (self, other) {
            (Self::Address(this), Self::Address(other)) => this < other,
            (Self::BelowTop(this), Self::BelowTop(other)) => this > other,
            (Self::Address(_), Self::BelowTop(_)) | (Self::BelowTop(_), Self::Address(_)) => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Activation {
    owner: StackOwner,
    depth: Depth,
}

impl Activation {
    pub(super) const fn on_task(task: TaskId, below_top: u64) -> Self {
        Self {
            owner: StackOwner::Task(task),
            depth: Depth::BelowTop(below_top),
        }
    }

    pub(super) const fn on_task_stack(self) -> Option<(TaskId, u64)> {
        match (self.owner, self.depth) {
            (StackOwner::Task(task), Depth::BelowTop(below_top)) => Some((task, below_top)),
            _ => None,
        }
    }

    pub(super) fn is_callee_of(self, other: Self) -> bool {
        self.owner == other.owner && self.depth.is_deeper_than(other.depth)
    }

    pub(super) fn has_returned(self, position: StackPosition) -> bool {
        self.owner == position.owner && !position.depth.is_deeper_than(self.depth)
    }

    pub(super) fn just_returned(self, position: StackPosition) -> bool {
        self.owner == position.owner && self.depth == position.depth
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct StackPosition {
    owner: StackOwner,
    depth: Depth,
}

impl StackPosition {
    pub(super) fn is_deeper_than(self, other: Self) -> bool {
        self.owner == other.owner && self.depth.is_deeper_than(other.depth)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct TaskStack {
    pub(super) task: TaskId,

    pub(super) low: u64,

    pub(super) high: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct StackView {
    thread: Pid,

    task: Option<TaskStack>,
}

impl StackView {
    pub(super) const fn thread(pid: Pid) -> Self {
        Self {
            thread: pid,
            task: None,
        }
    }

    pub(super) const fn task(pid: Pid, task: TaskStack) -> Self {
        Self {
            thread: pid,
            task: Some(task),
        }
    }

    const fn locate(self, address: u64) -> (StackOwner, Depth) {
        match self.task {
            Some(stack) if stack.low <= address && address <= stack.high => (
                StackOwner::Task(stack.task),
                Depth::BelowTop(stack.high - address),
            ),
            _ => (StackOwner::Thread(self.thread), Depth::Address(address)),
        }
    }

    pub(super) const fn activation(self, cfa: VirtualAddress) -> Activation {
        let (owner, depth) = self.locate(cfa.get());
        Activation { owner, depth }
    }

    pub(super) const fn position(self, stack_pointer: u64) -> StackPosition {
        let (owner, depth) = self.locate(stack_pointer);
        StackPosition { owner, depth }
    }

    pub(super) fn cfa_of(self, activation: Activation) -> Option<VirtualAddress> {
        match (activation.owner, activation.depth, self.task) {
            (StackOwner::Thread(pid), Depth::Address(address), _) if pid == self.thread => {
                Some(VirtualAddress::new(address))
            }

            (StackOwner::Task(task), Depth::BelowTop(offset), Some(stack))
                if task == stack.task =>
            {
                stack.high.checked_sub(offset).map(VirtualAddress::new)
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activations_compare_only_on_their_own_stack() {
        let stack = StackView::thread(Pid::from_raw(1));
        let other = StackView::thread(Pid::from_raw(2));
        let outer = stack.activation(VirtualAddress::new(0x1000));
        let inner = stack.activation(VirtualAddress::new(0x0f00));
        assert!(inner.is_callee_of(outer));
        assert!(!outer.is_callee_of(inner));
        assert!(!inner.is_callee_of(inner));
        assert!(
            !other
                .activation(VirtualAddress::new(0x0f00))
                .is_callee_of(outer)
        );

        assert!(!inner.has_returned(stack.position(0x0ef8)));
        assert!(inner.has_returned(stack.position(0x0f00)));
        assert!(inner.has_returned(stack.position(0x0f08)));
        assert!(!inner.has_returned(other.position(0x0f08)));
        assert!(
            stack
                .position(0x0ef8)
                .is_deeper_than(stack.position(0x0f00))
        );
    }

    #[test]
    fn a_task_s_activations_survive_its_stack_moving() {
        let task = TaskId {
            runtime: crate::RuntimeId::new(1),
            number: 7,
        };

        let thread = Pid::from_raw(1);
        let before = StackView::task(
            thread,
            TaskStack {
                task,
                low: 0x1_0000,
                high: 0x1_2000,
            },
        );

        let after = StackView::task(
            Pid::from_raw(2),
            TaskStack {
                task,
                low: 0x5_0000,
                high: 0x5_4000,
            },
        );
        let frame = before.activation(VirtualAddress::new(0x1_1f00));
        assert_eq!(after.activation(VirtualAddress::new(0x5_3f00)), frame);
        assert_eq!(after.cfa_of(frame), Some(VirtualAddress::new(0x5_3f00)));
        assert!(
            after
                .activation(VirtualAddress::new(0x5_3e00))
                .is_callee_of(frame)
        );
        assert!(!frame.has_returned(after.position(0x5_3ef8)));
        assert!(frame.has_returned(after.position(0x5_3f08)));

        let system = after.activation(VirtualAddress::new(0x9_0000));
        assert!(!system.is_callee_of(frame) && !frame.is_callee_of(system));
        assert!(!frame.has_returned(after.position(0x9_0000)));
        assert_eq!(before.cfa_of(system), None);
    }
}
