











use std::sync::mpsc as channel;
use std::sync::{Arc, Mutex};

use tokio::sync::oneshot;



async fn migrating(
    gate: oneshot::Receiver<()>,
    parked_on: channel::Sender<i32>,
    resumed: channel::Sender<()>,
) -> u64 {
    let me = truth::start();
    let before = truth::gettid();
    parked_on.send(before).expect("the program waits");
    let at = truth::at(me, "gate");
    gate.await.expect("the gate opens"); // AWAIT: migrate
    drop(at); // STEP: migrated
    let after = truth::gettid();
    truth::line(&[&"migrated", &me, &before, &after]);
    resumed.send(()).expect("the holder waits");
    truth::end(me);
    me
}

fn main() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .build()
        .expect("a runtime");
    let (open, gate) = oneshot::channel();
    let (parked_on, before) = channel::channel();
    let (resumed, held_until) = channel::channel();
    let task = runtime.spawn(migrating(gate, parked_on, resumed));
    let before = before.recv().expect("the task runs");
    while truth::parked_at("gate") != 1 {
        std::thread::yield_now();
    }


    let held_until = Arc::new(Mutex::new(held_until));
    let holder = loop {
        let (placed, on_task_thread) = channel::channel();
        let held_until = Arc::clone(&held_until);
        let holder = runtime.spawn(async move {
            if truth::gettid() != before {
                placed.send(false).expect("the program waits");
                return;
            }
            truth::line(&[&"holder", &truth::id(), &truth::gettid()]);
            placed.send(true).expect("the program waits");
            tokio::task::block_in_place(|| {
                let resumed = held_until.lock().expect("one holder");
                resumed.recv().expect("the task resumes"); // HOLD: in place
            });
        });
        if on_task_thread.recv().expect("the holder runs") {
            break holder;
        }
        runtime.block_on(holder).expect("the holder ends");
    };



    while !truth::workers_parked(runtime.handle()) {
        std::thread::yield_now();
    }
    open.send(()).expect("the task waits");
    let me = runtime.block_on(async move {
        let me = task.await.expect("the task ends");
        holder.await.expect("the holder ends");
        me
    });
    truth::line(&[&"ended", &me]);
}
