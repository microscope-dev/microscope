pub use microscope::syscalls::present::*;

use microscope::syscalls::{EventFlags, Outcome, Pid, SyscallEvent, Tid};

#[must_use]
pub fn caught(stop: &microscope::SyscallStop) -> String {
    let event = SyscallEvent {
        thread: Tid::new(0),
        process: Pid::new(0),
        call: stop.call,
        args: stop.args,
        captured: stop.captured.to_vec(),
        outcome: stop
            .returned
            .map_or(Outcome::Unfinished, Outcome::of_return),
        entered: None,
        exited: None,
        stack: None,
        flags: EventFlags::default(),
    };

    let context = Context {
        fds: None,
        before: true,
    };

    if stop.returned.is_some() {
        call(&event, context).text()
    } else {
        entry(&event, context).text()
    }
}
