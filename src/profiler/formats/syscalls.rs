//! A system call trace as samples, for the formats that hold stacks: each
//! call is a sample of its thread whose stack is the call's, with the
//! call's name as its innermost frame.

use std::collections::BTreeMap;

use crate::capture::builder::{CaptureBuilder, LineInput};
use crate::capture::records::{FrameKind, NONE};
use crate::profiler::Profile;
use crate::syscalls::{Abi, SyscallId, SyscallStats, Tid, TraceEvent};

/// What each call weighs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Weigh {
    /// One.
    Calls,

    /// Its time in the kernel, in nanoseconds; nothing for a call that did
    /// not return.
    Time,
}

/// Where the frames that name calls are: in the kernel's half of the
/// address space, where no frame of a program's stack is.
const CALLS_AT: u64 = 0xffff_8000_0000_0000;

/// The profile's calls as samples weighing `weigh`, those the kernel only
/// counted on a thread of their process's id with only their names.
pub fn samples(profile: &Profile, weigh: Weigh) -> Profile {
    let capture = &profile.capture;
    let mut builder = CaptureBuilder::new();
    let map = builder.absorb(capture);
    let mut names: BTreeMap<String, u32> = BTreeMap::new();
    let mut call_frame = |builder: &mut CaptureBuilder, id: SyscallId| {
        let name = match id.abi {
            Abi::X86_64 => id.display_name().into_owned(),
            abi => format!("{}@{}", id.display_name(), abi.suffix()),
        };
        let next = CALLS_AT + names.len() as u64;
        *names.entry(name).or_insert_with_key(|name| {
            let frame = builder.frame(FrameKind::Native, NONE, next, 0);
            let line = LineInput {
                function: name,
                file: None,
                line: 0,
                column: 0,
                declared: None,
                role: 0,
            };

            builder.symbolize(frame, NONE, &[line]);
            frame
        })
    };

    let weight = |stats: &SyscallStats| match weigh {
        Weigh::Calls => stats.calls,
        Weigh::Time => stats.total_ns,
    };

    // `syscalls` reads the rows in order, one call each.
    let syscalls = profile.syscalls();
    let calls = syscalls
        .events
        .iter()
        .filter_map(|event| match &event.event {
            TraceEvent::Call(call) => Some(call),
            _ => None,
        });

    for (row, call) in capture.syscalls.iter().zip(calls) {
        let frame = call_frame(&mut builder, call.call);
        let stack = builder.child(map.stack(row.stack.get()), frame);
        let weight = match weigh {
            Weigh::Calls => 1,
            Weigh::Time => call.duration().unwrap_or(0),
        };

        let thread = map.thread(row.thread.get());
        builder.sample(thread, call.time().cast_signed(), weight, stack, NONE);
    }

    for (&(pid, id), stats) in &syscalls.counted.by_call {
        let frame = call_frame(&mut builder, id);
        let stack = builder.child(NONE, frame);
        let thread = builder.thread(pid, Tid::new(pid.get()), 0);
        builder.sample(thread, 0, weight(stats), stack, NONE);
    }

    builder.setting(
        "weight",
        match weigh {
            Weigh::Calls => "calls",
            Weigh::Time => "nanoseconds",
        },
    );
    Profile {
        capture: builder.finish(),
    }
}
