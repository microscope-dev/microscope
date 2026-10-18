//! Host/native id collisions must never assign Python state to the wrong thread.

use super::*;
use crate::runtime_model::{RuntimeModel as _, TargetThreadId};

const HOST: ThreadId = ThreadId::new(5000);
const COLLISION: ThreadId = ThreadId::new(7);

fn world() -> (Process, u64) {
    let mut process = Process::new(layout());
    let code = process.code("main", 1, &[(2, 0)]);
    let frame = process.frame(0, code, process.instruction(code, 0));
    let state = process.thread_state(0, frame, 7);
    let interpreter = process.interpreter(0, 0, state);
    let runtime = process.runtime(interpreter);
    process.memory.threads = vec![COLLISION, HOST];
    process
        .memory
        .target_ids
        .insert(HOST, TargetThreadId::new(7));
    process
        .memory
        .target_ids
        .insert(COLLISION, TargetThreadId::new(8));
    assert_eq!(
        process
            .memory
            .target_thread_id(HOST)
            .expect("proven id")
            .get(),
        7
    );
    for (field, offset) in [
        (Field::InterpreterCevalGil, 24),
        (Field::InterpreterGil, 32),
        (Field::InterpreterGilLocked, 32),
        (Field::InterpreterGilHolder, 40),
    ] {
        process.layout.set_field(field, offset);
    }
    let gil = process.alloc(SMALL);
    process.memory.write(gil, 1, 4);
    process.memory.write(gil + 8, state, 8);
    process.word(interpreter, Field::InterpreterCevalGil, gil);
    (process, runtime)
}

fn model(process: &Process, runtime: u64) -> super::super::PythonRuntime {
    super::super::PythonRuntime {
        free_threaded: false,
        entries: Vec::new(),
        evaluation: Vec::new(),
        version: crate::image::python::PyVersion(0x030e_00f0),
        reading: Ok(super::super::Reading {
            runtime: crate::AddressRange {
                start: crate::ImageAddress::new(runtime),
                end: crate::ImageAddress::new(runtime + SMALL),
            },
            layout: process.layout.clone(),
            code_type: crate::ImageAddress::new(process.code_type),
            types: Err("not needed for frames or labels".into()),
            secret: None,
            code: std::sync::Mutex::new(CodeTable::default()),
        }),
    }
}

#[test]
fn python_frames_use_proven_target_thread_ids_even_when_a_host_id_collides() {
    let (process, runtime) = world();
    let stacks = model(&process, runtime)
        .interpreted(&process.memory)
        .expect("interprets");
    assert!(stacks.gaps.is_empty(), "{:?}", stacks.gaps);
    assert_eq!(stacks.value.threads.len(), 1);
    assert_eq!(stacks.value.threads[0].thread, HOST);
}

#[test]
fn gil_labels_use_proven_target_thread_ids_even_when_a_host_id_collides() {
    let (process, runtime) = world();
    let labels = model(&process, runtime).thread_labels(&process.memory);
    assert!(labels.gaps.is_empty(), "{:?}", labels.gaps);
    assert_eq!(labels.value.len(), 1);
    assert_eq!(labels.value[0].thread, HOST);
    assert_eq!(labels.value[0].labels, [("gil".into(), "held".into())]);
}

#[test]
fn unavailable_or_ambiguous_ids_never_fall_back_to_numeric_identity() {
    for ambiguous in [false, true] {
        let (mut process, runtime) = world();
        process
            .memory
            .target_ids
            .insert(HOST, ambiguous.then(|| TargetThreadId::new(7).unwrap()));
        process.memory.target_ids.insert(
            COLLISION,
            ambiguous.then(|| TargetThreadId::new(7).unwrap()),
        );
        let model = model(&process, runtime);
        let stacks = model.interpreted(&process.memory).expect("interprets");
        assert!(
            stacks.value.threads.is_empty(),
            "ambiguous={ambiguous}: {stacks:?}"
        );
        assert!(!stacks.gaps.is_empty());
        let labels = model.thread_labels(&process.memory);
        assert!(labels.value.is_empty(), "ambiguous={ambiguous}: {labels:?}");
        assert!(!labels.gaps.is_empty());
        let states = model.walked_threads(&process.memory).expect("walks states");
        assert!(states.gaps.is_empty(), "{:?}", states.gaps);
        assert_eq!(states.value.len(), 1);
        assert_eq!(states.value[0].native, 7);
        if !ambiguous {
            assert_eq!(stacks.gaps.len(), 1);
            assert!(stacks.gaps[0].contains("platform supplied no proven"));
        }
    }
}

#[test]
fn duplicate_host_entries_are_deduplicated_but_excessive_catalogs_are_bounded() {
    let (mut process, _) = world();
    process.memory.threads = vec![HOST, HOST];
    let ids = super::super::threads::ThreadIds::new(&process.memory).unwrap();
    assert_eq!(ids.resolve(7), Ok(HOST));
    assert!(ids.resolve(0).is_err());
    process.memory.threads = (1..=16385).map(ThreadId::new).collect();
    assert!(super::super::threads::ThreadIds::new(&process.memory).is_err());
}
