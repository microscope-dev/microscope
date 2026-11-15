

















use std::hint::black_box;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Barrier, Mutex, Notify, Semaphore, mpsc, oneshot};
use tokio::task::JoinHandle;


enum Wait {
    Channel(mpsc::Receiver<u32>),
    Sleep,
    Lock(Arc<Mutex<u32>>),
    Join(JoinHandle<u32>),
    Notified(Arc<Notify>),
    Oneshot(oneshot::Receiver<u32>),
    Barrier(Arc<Barrier>),
    Permit(Arc<Semaphore>),
}

impl Wait {
    fn tag(&self) -> &'static str {
        match self {
            Self::Channel(_) => "channel",
            Self::Sleep => "sleep",
            Self::Lock(_) => "lock",
            Self::Join(_) => "join",
            Self::Notified(_) => "notify",
            Self::Oneshot(_) => "oneshot",
            Self::Barrier(_) => "barrier",
            Self::Permit(_) => "permit",
        }
    }
}

async fn leaf(me: u64, wait: Wait) -> u32 {
    let leaf_local = black_box(me * 100 + 3);
    truth::value(me, "leaf_local", leaf_local);
    truth::task_reached(me);
    let tag = wait.tag();
    let _at = truth::at(me, tag);
    let got = match wait {
        Wait::Channel(mut receiver) => receiver.recv().await.unwrap_or(0), // AWAIT: channel
        Wait::Sleep => {
            tokio::time::sleep(Duration::from_secs(3600)).await; // AWAIT: sleep
            0
        }
        Wait::Lock(mutex) => *mutex.lock().await, // AWAIT: lock
        Wait::Join(handle) => handle.await.unwrap_or(0), // AWAIT: join
        Wait::Notified(notify) => {
            notify.notified().await; // AWAIT: notify
            4
        }
        Wait::Oneshot(receiver) => receiver.await.unwrap_or(0), // AWAIT: oneshot
        Wait::Barrier(barrier) => {
            barrier.wait().await; // AWAIT: barrier
            6
        }
        Wait::Permit(semaphore) => {
            let _permit = semaphore.acquire().await; // AWAIT: permit
            7
        }
    };
    truth::here(me);
    got + u32::try_from(leaf_local % 7).unwrap_or(0)
}

async fn middle(me: u64, wait: Wait) -> u32 {
    let middle_local = black_box(me * 100 + 2);
    truth::value(me, "middle_local", middle_local);
    let _at = truth::at(me, "middle");
    let got = leaf(me, wait).await; // AWAIT: middle
    got + u32::try_from(middle_local % 5).unwrap_or(0)
}

async fn top(wait: Wait) -> u32 {
    let me = truth::start();
    let top_local = black_box(me * 100 + 1);
    truth::value(me, "top_local", top_local);
    let got = {
        let _at = truth::at(me, "top");
        middle(me, wait).await // AWAIT: top
    };
    truth::end(me);
    got + u32::try_from(top_local % 3).unwrap_or(0)
}






#[cfg(feature = "wrapped")]
mod wrap {
    use std::future::Future;
    use std::panic::AssertUnwindSafe;
    use std::pin::Pin;

    use futures_util::FutureExt;

    tokio::task_local! {
        static DOING: &'static str;
    }

    pub fn boxed(
        future: impl Future<Output = u32> + Send + 'static,
    ) -> Pin<Box<dyn Future<Output = u32> + Send>> {
        Box::pin(future)
    }

    pub fn scoped(
        future: impl Future<Output = u32> + Send + 'static,
    ) -> impl Future<Output = u32> + Send + 'static {
        DOING.scope("notified", future)
    }

    pub fn caught(
        future: impl Future<Output = u32> + Send + 'static,
    ) -> impl Future<Output = u32> + Send + 'static {
        AssertUnwindSafe(future)
            .catch_unwind()
            .map(|ended| ended.unwrap_or(0))
    }
}

#[cfg(not(feature = "wrapped"))]
mod wrap {
    pub const fn boxed<F>(future: F) -> F {
        future
    }

    pub const fn scoped<F>(future: F) -> F {
        future
    }

    pub const fn caught<F>(future: F) -> F {
        future
    }
}



async fn run(handle: Option<tokio::runtime::Handle>) {
    let (sender, receiver) = mpsc::channel(1);
    let mutex = Arc::new(Mutex::new(3));
    let guard = mutex.clone().lock_owned().await;
    let notify = Arc::new(Notify::new());
    let (oneshot_sender, oneshot_receiver) = oneshot::channel();
    let barrier = Arc::new(Barrier::new(2));
    let semaphore = Arc::new(Semaphore::new(0));

    let channel = tokio::spawn(top(Wait::Channel(receiver))); // SPAWN: channel
    let mut tasks = vec![
        tokio::spawn(wrap::boxed(top(Wait::Sleep))), // SPAWN: sleep
        tokio::spawn(top(Wait::Lock(mutex))), // SPAWN: lock
        tokio::spawn(top(Wait::Join(channel))), // SPAWN: join
        tokio::spawn(wrap::scoped(top(Wait::Notified(notify.clone())))), // SPAWN: notify
        tokio::spawn(wrap::caught(top(Wait::Oneshot(oneshot_receiver)))), // SPAWN: oneshot
        tokio::spawn(top(Wait::Barrier(barrier.clone()))), // SPAWN: barrier
        tokio::spawn(top(Wait::Permit(semaphore.clone()))), // SPAWN: permit
    ];



    let (started_sender, started) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel::<()>();
    let blocking = tokio::task::spawn_blocking(move || {
        started_sender.send(truth::gettid()).expect("the program waits");
        released.recv().expect("the program releases the closure");
    });
    let blocking_thread = started.recv().expect("the closure starts");
    let queued = tokio::task::spawn_blocking(|| ()); // SPAWN: queued

    while !truth::all_parked(8) || handle.as_ref().is_some_and(|handle| !truth::workers_parked(handle)) {
        if handle.is_some() {
            std::thread::yield_now();
        } else {
            tokio::task::yield_now().await;
        }
    }
    truth::line(&[&"blocking", &blocking.id(), &"running", &blocking_thread]);
    truth::line(&[&"blocking", &queued.id(), &"queued"]);

    let fresh = handle.is_none().then(|| {
        notify.notify_one();
        truth::line(&[&"woken", &tasks[3].id()]);
        let fresh = tokio::spawn(top(Wait::Sleep)); // SPAWN: fresh
        truth::line(&[&"spawned", &fresh.id()]);
        fresh
    });
    truth::checkpoint("parked", handle.as_ref());


    tasks.remove(0).abort();
    if let Some(fresh) = fresh {
        fresh.abort();
    } else {
        notify.notify_one();
    }
    release.send(()).expect("the closure waits");
    blocking.await.expect("the closure ends");
    queued.await.expect("the queued closure ends");
    sender.send(1).await.expect("the channel's task waits");
    drop(guard);
    oneshot_sender.send(5).expect("the oneshot's task waits");
    barrier.wait().await;
    semaphore.add_permits(1);
    for task in tasks {
        task.await.expect("a task ends");
    }
    truth::line(&[&"end"]);
}

fn main() {
    let current = std::env::args().nth(1).as_deref() == Some("current");
    if current {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .max_blocking_threads(1)
            .enable_time()
            .build()
            .expect("a runtime");
        runtime.block_on(run(None));
    } else {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .max_blocking_threads(1)
            .enable_time()
            .build()
            .expect("a runtime");
        let handle = runtime.handle().clone();


        std::thread::scope(|scope| {
            scope.spawn(|| runtime.block_on(run(Some(handle))));
        });
    }
}
