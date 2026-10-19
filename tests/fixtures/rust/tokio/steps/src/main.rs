//! Tasks a test steps through, across awaits. Three tasks run the same
//! async functions. Each waits at its gate, which a plain thread opens
//! only once every task waits there and the runtime's workers park, so
//! that an await a test steps over is pending, and the other tasks run
//! the same code meanwhile. Then each goes round a loop of awaits that
//! yield, and finishes.
//!
//! With no argument the tasks run on a multi-thread runtime's two
//! workers; with `current`, on a current-thread runtime; with `local`, in
//! a `LocalSet` the main thread runs on a current-thread runtime. With
//! `nested`, one task on a multi-thread runtime spawns another and awaits
//! it.

use std::hint::black_box;

use tokio::sync::oneshot;

const TASKS: usize = 3;

async fn inner(me: u64, gate: oneshot::Receiver<u64>) -> u64 {
    let before = black_box(me * 10); // STEP: inner
    let at = truth::at(me, "gate");
    let opened = gate.await.unwrap_or(0); // AWAIT: inner
    drop(at); // STEP: inner-after
    before + opened
}

async fn outer(me: u64, gate: oneshot::Receiver<u64>) -> u64 {
    let start = black_box(me + 1); // STEP: outer
    let got = inner(me, gate).await; // AWAIT: outer
    start + got // STEP: outer-after
}

async fn rounds(me: u64) -> u64 {
    let mut total = 0;
    for round in 0..3 {
        total += round; // STEP: round
        tokio::task::yield_now().await; // AWAIT: round
    }
    total + me // STEP: rounds-after
}

async fn task(gate: oneshot::Receiver<u64>) -> u64 {
    let me = truth::start(); // FIRST: task
    let got = outer(me, gate).await; // STEP: task
    let more = rounds(me).await; // AWAIT: rounds
    truth::end(me);
    got + more // STEP: task-last
}

async fn child(parent: u64) -> u64 {
    let me = truth::start(); // FIRST: child
    truth::end(me);
    parent + me
}

async fn parent() -> u64 {
    let me = truth::start();
    let spawned = black_box(me); // BEFORE: nested
    let child = tokio::spawn(child(spawned)); // SPAWN: nested
    let sum = child.await.expect("the child ends");
    truth::end(me);
    sum
}

fn main() {
    let mode = std::env::args().nth(1);
    if mode.as_deref() == Some("nested") {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .build()
            .expect("a runtime");
        let sum = runtime
            .block_on(runtime.spawn(parent()))
            .expect("the parent ends");
        truth::line(&[&"sum", &sum]);
        return;
    }
    let runtime = match mode.as_deref() {
        None => tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .build(),
        Some("current" | "local") => tokio::runtime::Builder::new_current_thread().build(),
        Some(other) => panic!("unknown mode {other}"),
    }
    .expect("a runtime");
    let local = (mode.as_deref() == Some("local")).then(tokio::task::LocalSet::new);
    let mut gates = Vec::new();
    let mut handles = Vec::new();
    for _ in 0..TASKS {
        let (open, gate) = oneshot::channel();
        gates.push(open); // SPAWNS: none
        let handle = match &local {
            Some(local) => local.spawn_local(task(gate)), // SPAWN: local
            None => runtime.spawn(task(gate)),            // SPAWN: runtime
        };
        handles.push(handle);
    }
    // A task marks the gate before it polls it, so the gates open only
    // once the workers park too: then every task's await is pending.
    let handle = runtime.handle().clone();
    let opener = std::thread::spawn(move || {
        while truth::parked_at("gate") != TASKS || !truth::workers_parked(&handle) {
            std::thread::yield_now();
        }
        for (value, open) in (0..).zip(gates) {
            open.send(value).expect("the task waits"); // WAKES: gate
        }
    });
    let joined = async {
        let mut sum = 0;
        for handle in handles {
            sum += handle.await.expect("the task ends");
        }
        sum
    };
    let sum = match &local {
        Some(local) => local.block_on(&runtime, joined),
        None => runtime.block_on(joined),
    };
    opener.join().expect("the gates open");
    truth::line(&[&"sum", &sum]);
}
