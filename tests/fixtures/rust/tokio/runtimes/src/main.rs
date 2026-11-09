//! Two runtimes in one process, two `LocalSet`s, and a thread of the
//! program's own. A multi-thread runtime has two tasks parked; a
//! current-thread runtime, which a thread of its own blocks on, has two;
//! a `LocalSet` that another current-thread runtime's thread runs has two,
//! and one that a thread blocking on the multi-thread runtime runs has
//! two, which no runtime lists. A plain thread waits on a channel.
//!
//! Once every task has registered its await and every thread is parked,
//! the program reports its truth and stops at the checkpoint `parked`,
//! where neither set is running. Then it wakes the second set's task `go`,
//! which stops at `task_reached` while the set runs it, and releases
//! everything.

use std::sync::Arc;
use std::sync::mpsc as channel;
use std::time::Duration;

use tokio::runtime::{Builder, Runtime};
use tokio::sync::{Notify, oneshot};
use tokio::task::LocalSet;

/// A task that reports which runtime runs it, then waits at `tag` for
/// `wait`.
async fn parked(runtime: &'static str, tag: &'static str, wait: impl Future<Output = ()>) {
    let me = truth::start();
    truth::value(me, "runtime", runtime);
    let _at = truth::at(me, tag);
    wait.await; // AWAIT: parked
    truth::end(me);
}

/// An hour's sleep, which the program never waits out.
async fn sleep() {
    tokio::time::sleep(Duration::from_secs(3600)).await;
}

/// A current-thread runtime with its timer.
fn current() -> Runtime {
    Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("a runtime")
}

/// Runs `body` on a thread of its own, after sending the thread's id.
fn thread(name: &str, body: impl FnOnce() + Send + 'static) -> (i32, std::thread::JoinHandle<()>) {
    let (started, tid) = channel::channel();
    let handle = std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            started.send(truth::gettid()).expect("the program waits");
            body();
        })
        .expect("a thread");
    (tid.recv().expect("the thread starts"), handle)
}

fn main() {
    // With the argument `clone`, the thread that runs the first set also
    // starts a thread from it. With `join`, that thread runs the set in a
    // `join!` whose other branch calls `join_sibling` while the set waits.
    let argument = std::env::args().nth(1);
    let clone = argument.as_deref() == Some("clone");
    let join = argument.as_deref() == Some("join");
    let multi = Builder::new_multi_thread()
        .worker_threads(2)
        .enable_time()
        .build()
        .expect("a runtime");
    let notify = Arc::new(Notify::new());
    multi.spawn(parked("multi", "sleep", sleep()));
    multi.spawn(parked("multi", "notify", {
        let notify = Arc::clone(&notify);
        async move { notify.notified().await }
    }));

    // Tokio numbers tasks as they are spawned, so each thread spawns its
    // own before the next starts, and every run numbers them alike.
    let (spawned, all_spawned) = channel::channel::<()>();

    // A current-thread runtime, which its thread blocks on until released.
    let (release_current, released) = oneshot::channel::<()>();
    let (sent, received) = oneshot::channel::<()>();
    let (current_tid, current_thread) = thread("current", {
        let spawned = spawned.clone();
        move || {
            current().block_on(async move {
                tokio::spawn(parked("current", "oneshot", async move {
                    let _ = received.await;
                }));
                tokio::spawn(parked("current", "sleep", sleep()));
                spawned.send(()).expect("the program waits");
                let _ = released.await;
            });
        }
    });
    all_spawned.recv().expect("the thread spawns");

    // A set of local tasks, which its thread runs on a current-thread
    // runtime until released.
    let (release_local, local_released) = oneshot::channel::<()>();
    let (local_tid, local_thread) = thread("local", {
        let spawned = spawned.clone();
        move || {
            let local = LocalSet::new();
            let after_sibling = spawned.clone();
            let body = async move {
                let notify = Arc::new(Notify::new());
                tokio::task::spawn_local(parked("local", "notify", {
                    let notify = Arc::clone(&notify);
                    async move { notify.notified().await }
                }));
                tokio::task::spawn_local(parked("local", "sleep", sleep()));
                if clone {
                    std::thread::spawn(|| ()).join().expect("the thread ends");
                }
                if !join {
                    spawned.send(()).expect("the program waits");
                }
                let _ = local_released.await;
            };
            if join {
                #[inline(never)]
                fn join_sibling() {
                    std::hint::black_box(());
                }

                // The block holds the set only through the branch the
                // `join!` keeps. No other thread starts until the sibling
                // has run.
                let mut branch = std::pin::pin!(local.run_until(body));
                current().block_on(async {
                    tokio::join!(biased; branch.as_mut(), async {
                        join_sibling();
                        after_sibling.send(()).expect("the program waits");
                    });
                });
            } else {
                local.block_on(&current(), body);
            }
        }
    });
    all_spawned.recv().expect("the thread spawns");

    // Another, which a thread blocking on the multi-thread runtime runs.
    let (release_shared, shared_released) = oneshot::channel::<()>();
    let go = Arc::new(Notify::new());
    let (went, gone) = channel::channel::<()>();
    let (shared_tid, shared_thread) = thread("shared", {
        let go = Arc::clone(&go);
        let handle = multi.handle().clone();
        move || {
            let local = LocalSet::new();
            handle.block_on(local.run_until(async move {
                tokio::task::spawn_local(parked("shared", "sleep", sleep()));
                tokio::task::spawn_local(async move {
                    let me = truth::start();
                    truth::value(me, "runtime", "shared");
                    {
                        let _at = truth::at(me, "go");
                        go.notified().await; // AWAIT: go
                    }
                    truth::task_reached(me);
                    went.send(()).expect("the program waits");
                    truth::end(me);
                });
                spawned.send(()).expect("the program waits");
                let _ = shared_released.await;
            }));
        }
    });
    all_spawned.recv().expect("the thread spawns");

    // A thread of the program's own.
    let (stop_plain, plain_stopped) = channel::channel::<()>();
    let (plain_tid, plain_thread) = thread("plain", move || {
        let _ = plain_stopped.recv();
    });

    while !truth::all_parked(8)
        || !truth::workers_parked(multi.handle())
        || ![current_tid, local_tid, shared_tid, plain_tid]
            .into_iter()
            .all(truth::thread_parked)
    {
        std::thread::yield_now();
    }
    truth::line(&[&"thread", &"current", &current_tid]);
    truth::line(&[&"thread", &"local", &local_tid]);
    truth::line(&[&"thread", &"shared", &shared_tid]);
    truth::line(&[&"thread", &"plain", &plain_tid]);
    truth::checkpoint("parked", Some(multi.handle()));

    // The set runs its task `go`, which stops where it is reached.
    go.notify_one();
    gone.recv().expect("the set runs its task");

    let _ = sent.send(());
    let _ = release_current.send(());
    let _ = release_local.send(());
    let _ = release_shared.send(());
    let _ = stop_plain.send(());
    notify.notify_one();
    for thread in [current_thread, local_thread, shared_thread, plain_thread] {
        thread.join().expect("a thread ends");
    }
    multi.shutdown_background();
    truth::line(&[&"end"]);
}
