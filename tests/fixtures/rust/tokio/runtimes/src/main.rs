












use std::sync::Arc;
use std::sync::mpsc as channel;
use std::time::Duration;

use tokio::runtime::{Builder, Runtime};
use tokio::sync::{Notify, oneshot};
use tokio::task::LocalSet;



async fn parked(runtime: &'static str, tag: &'static str, wait: impl Future<Output = ()>) {
    let me = truth::start();
    truth::value(me, "runtime", runtime);
    let _at = truth::at(me, tag);
    wait.await; // AWAIT: parked
    truth::end(me);
}


async fn sleep() {
    tokio::time::sleep(Duration::from_secs(3600)).await;
}


fn current() -> Runtime {
    Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("a runtime")
}


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



    let (spawned, all_spawned) = channel::channel::<()>();


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
    while join
        && !(truth::all_parked(4)
            && truth::workers_parked(multi.handle())
            && truth::thread_parked(current_tid))
    {
        std::thread::yield_now();
    }

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
