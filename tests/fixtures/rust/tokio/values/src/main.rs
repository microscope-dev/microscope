








use std::future::Future;
use std::hint::black_box;
use std::os::fd::AsRawFd;
use std::os::linux::net::SocketAddrExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::Poll;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::{TcpListener, TcpStream, UdpSocket, UnixListener, UnixStream};
use tokio::runtime::Runtime;
use tokio::sync::{Mutex, Notify, RwLock, Semaphore, broadcast, mpsc, oneshot, watch};
use tokio::task::{JoinHandle, JoinSet};


#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn barrier() {
    black_box(());
}


fn view(expression: &str, shows: impl std::fmt::Display) {
    println!("VIEW: {expression} => {shows}");
}


static STARTED: AtomicUsize = AtomicUsize::new(0);



fn until_waiting(runtime: &Runtime, count: usize) {
    while STARTED.load(Ordering::SeqCst) < count || !truth::workers_parked(runtime.handle()) {
        std::thread::yield_now();
    }
}


fn until_finished<T>(handle: &JoinHandle<T>) {
    while !handle.is_finished() {
        std::thread::yield_now();
    }
}


fn waiter<F: Future + Send + 'static>(runtime: &Runtime, future: F) -> JoinHandle<F::Output>
where
    F::Output: Send,
{
    runtime.spawn(async move {
        STARTED.fetch_add(1, Ordering::SeqCst);
        future.await
    })
}


fn task<T>(handle: &JoinHandle<T>) -> String {
    format!("task {}", handle.id())
}



fn duration(nanoseconds: u128) -> String {
    const SECOND: u128 = 1_000_000_000;
    let (hours, minutes) = (nanoseconds / (3600 * SECOND), nanoseconds / (60 * SECOND) % 60);
    let seconds = nanoseconds % (60 * SECOND);
    let mut text = String::new();
    if hours > 0 {
        text.push_str(&format!("{hours}h"));
    }
    if hours > 0 || minutes > 0 {
        text.push_str(&format!("{minutes}m"));
    }
    text.push_str(&(seconds / SECOND).to_string());
    let fraction = format!("{:09}", seconds % SECOND);
    let fraction = fraction.trim_end_matches('0');
    if !fraction.is_empty() {
        text.push('.');
        text.push_str(fraction);
    }
    text.push('s');
    text
}



fn since_boot(instant: std::time::Instant) -> String {
    let text = format!("{instant:?}");
    let number = |name: &str| -> u128 {
        let start = text.find(name).expect("a field") + name.len() + 2;
        let digits = text[start..].split(|c: char| !c.is_ascii_digit()).next();
        digits.expect("digits").parse().expect("a number")
    };
    duration(number("tv_sec") * 1_000_000_000 + number("tv_nsec"))
}

fn main() {
    let early = tokio::time::Instant::now();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("a runtime");
    let current = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime");
    let _entered = runtime.enter();


    let (release, released) = oneshot::channel::<u32>();
    let pending = runtime.spawn(async move { released.await.unwrap_or(0) });
    let finished = runtime.spawn(async { 42_u32 });
    let panicked = runtime.spawn(async {
        if black_box(true) {
            panic!("the task failed");
        }
        0_u32
    });
    let cancelled = runtime.spawn(std::future::pending::<u32>());
    cancelled.abort();
    let mut taken = runtime.spawn(async { 7_u32 });
    runtime.block_on(&mut taken).expect("the task's output");
    until_finished(&finished);
    until_finished(&panicked);
    until_finished(&cancelled);
    let id = pending.id();
    let failure = runtime
        .block_on(runtime.spawn(async { panic!("joined") }))
        .map(|()| ())
        .expect_err("the task panicked");
    let failed = failure.id();
    let aborted = runtime.spawn(std::future::pending::<()>());
    aborted.abort();
    let abort = runtime.block_on(aborted).expect_err("the task was cancelled");
    let woken = runtime.spawn(std::future::poll_fn(|context| Poll::Ready(context.waker().clone())));
    let woken_id = woken.id();
    let waker = runtime.block_on(woken).expect("the task's waker");


    let unlocked = Mutex::new(1_u32);
    let held = Mutex::new(2_u32);
    let guard = held.try_lock().expect("the lock is free");
    let contended = Arc::new(Mutex::new(3_u32));
    let contended_guard = contended.try_lock().expect("the lock is free");
    let locker = waiter(&runtime, {
        let contended = Arc::clone(&contended);
        async move { *contended.lock().await }
    });
    let readers = RwLock::new(String::from("shared"));
    let read_guard = readers.try_read().expect("the lock is free");
    let written = Arc::new(RwLock::new(4_u32));
    let write_guard = written.try_write().expect("the lock is free");
    let reader = waiter(&runtime, {
        let written = Arc::clone(&written);
        async move { *written.read().await }
    });
    let shared = Arc::new(RwLock::new(5_u32));
    let shared_guard = shared.try_read().expect("the lock is free");
    let writer = waiter(&runtime, {
        let shared = Arc::clone(&shared);
        async move { *shared.write().await }
    });
    let semaphore = Semaphore::new(2);
    let permit = semaphore.try_acquire().expect("a permit");
    let scarce = Arc::new(Semaphore::new(1));
    let scarce_permit = scarce.try_acquire().expect("a permit");
    let acquirer = waiter(&runtime, {
        let scarce = Arc::clone(&scarce);
        async move { scarce.acquire().await.map(|_| ()).is_ok() }
    });
    let shut = Semaphore::new(3);
    shut.close();


    let quiet = Notify::new();
    let notified = quiet.notified();
    let stored = Notify::new();
    stored.notify_one();
    let awaited = Arc::new(Notify::new());
    let notifiee = waiter(&runtime, {
        let awaited = Arc::clone(&awaited);
        async move { awaited.notified().await }
    });
    until_waiting(&runtime, 5);


    let (bounded, mut bounded_receiver) = mpsc::channel::<u32>(4);
    bounded.try_send(10).expect("room");
    bounded.try_send(20).expect("room");
    let (unbounded, unbounded_receiver) = mpsc::unbounded_channel::<&str>();
    unbounded.send("one").expect("a receiver");
    let (backlog, mut backlog_receiver) = mpsc::unbounded_channel::<u32>();
    for value in 0..40 {
        backlog.send(value).expect("a receiver");
    }
    for _ in 0..5 {
        backlog_receiver.try_recv().expect("a value");
    }
    let (closing, mut closed_receiver) = mpsc::channel::<u32>(2);
    closing.try_send(1).expect("room");
    closed_receiver.close();
    let (gone, gone_receiver) = mpsc::channel::<u32>(2);
    gone.try_send(3).expect("room");
    drop(gone);
    let (sent, sent_receiver) = oneshot::channel::<u32>();
    sent.send(5).expect("a receiver");
    let (empty, empty_receiver) = oneshot::channel::<u32>();
    let (abandoned, abandoned_receiver) = oneshot::channel::<u32>();
    drop(abandoned);
    let (refused, mut refusing) = oneshot::channel::<u32>();
    refusing.close();
    let (delivered, mut received) = oneshot::channel::<u32>();
    delivered.send(6).expect("a receiver");
    runtime.block_on(&mut received).expect("the value");
    let (watched, watch_receiver) = watch::channel(8_u32);
    let (lost, orphan) = watch::channel(1_u32);
    drop(lost);
    let (broadcaster, broadcast_receiver) = broadcast::channel::<u32>(4);
    broadcaster.send(9).expect("a receiver");
    let (flood, lagging) = broadcast::channel::<u32>(4);
    for value in 1..=6 {
        flood.send(value).expect("a receiver");
    }


    let instant = tokio::time::Instant::now();
    let sleep = tokio::time::sleep(Duration::from_secs(90));
    let elapsed = tokio::time::sleep_until(early);
    let mut registered = Box::pin(tokio::time::sleep(Duration::from_secs(100)));
    let _ = runtime.block_on(std::future::poll_fn(|context| {
        Poll::Ready(registered.as_mut().poll(context))
    }));
    let interval = runtime.block_on(async {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        interval.tick().await;
        interval
    });
    let current_sleep = {
        let _entered = current.enter();
        tokio::time::sleep(Duration::from_secs(80))
    };


    let mut set = JoinSet::new();
    let first = set.spawn(std::future::pending::<u32>()).id();
    let second = set.spawn(std::future::pending::<u32>()).id();
    let mut mixed = JoinSet::new();
    let waiting = mixed.spawn(std::future::pending::<u32>()).id();
    let done = mixed.spawn(async { 3_u32 });
    while !done.is_finished() {
        std::thread::yield_now();
    }



    let listener = runtime
        .block_on(TcpListener::bind("127.0.0.1:0"))
        .expect("a port to listen on");
    let address = listener.local_addr().expect("the listening address");
    let (client, accepted) = runtime.block_on(async {
        tokio::join!(TcpStream::connect(address), listener.accept())
    });
    let mut client = client.expect("a connection");
    let (accepted, _) = accepted.expect("an accepted connection");
    let accepted_fd = accepted.as_raw_fd();
    let (read_half, write_half) = accepted.into_split();
    let lines = BufReader::new(read_half).lines();
    let mut buffered = BufWriter::new(write_half);
    runtime
        .block_on(buffered.write_all(b"queued"))
        .expect("room in the buffer");
    let client_fd = client.as_raw_fd();
    let (mut client_reader, mut client_writer) = client.split();
    let mut buffer = [0_u8; 16];
    let read = client_reader.read(&mut buffer);
    let write_all = client_writer.write_all(b"hello");
    let udp = runtime
        .block_on(UdpSocket::bind("127.0.0.1:0"))
        .expect("a port");
    let (mut unix, unix_peer) = UnixStream::pair().expect("a pair of sockets");
    let unix_fd = unix.as_raw_fd();
    let (unix_left, mut unix_right) = UnixStream::pair().expect("a pair of sockets");
    let unix_right_fd = unix_right.as_raw_fd();
    let (unix_borrowed_read, unix_borrowed_write) = unix_right.split();
    let unix_left_fd = unix_left.as_raw_fd();
    let (unix_read, unix_write) = unix_left.into_split();
    let mut exact = [0_u8; 8];
    let read_exact = unix.read_exact(&mut exact);
    let name = format!("microscope-values-{}", std::process::id());
    let unix_listener = {
        let address = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes())
            .expect("an abstract name");
        let listener = std::os::unix::net::UnixListener::bind_addr(&address).expect("a name");
        listener.set_nonblocking(true).expect("a nonblocking listener");
        UnixListener::from_std(listener).expect("a registered listener")
    };

    view("id", id);
    view("pending", format!("task {id} pending"));
    view("finished", 42);
    view("finished", format!("children: task = {}, [raw]", finished.id()));
    view("panicked", format!("task {} panicked", panicked.id()));
    view("cancelled", format!("task {} was cancelled", cancelled.id()));
    view("taken", format!("task {}'s output taken", taken.id()));
    view("failure", format!("task {failed} panicked"));
    view("abort", format!("task {} was cancelled", abort.id()));
    view("waker", format!("task {woken_id}"));

    view("unlocked", 1);
    view("unlocked", "children: locked = false, waiters = len=0 [], [raw]");
    view("held", "children: locked = true, waiters = len=0 [], [raw]");
    view("guard", 2);
    view("contended", 3);
    view(
        "contended",
        format!("children: locked = true, waiters = len=1 [{}], strong = 2, weak = 0, [raw]", task(&locker)),
    );
    view("readers", "\"shared\"");
    view("readers", "children: capacity = 6, readers = 1, writer = false, waiters = len=0 [], [raw]");
    view("read_guard", "\"shared\"");
    view(
        "written",
        format!("children: readers = 0, writer = true, waiters = len=1 [{}], strong = 2, weak = 0, [raw]", task(&reader)),
    );
    view("write_guard", 4);
    view(
        "shared",
        format!("children: readers = 1, writer = false, waiters = len=1 [{}], strong = 2, weak = 0, [raw]", task(&writer)),
    );
    view("semaphore", 1);
    view("scarce", 0);
    view(
        "**scarce",
        format!("children: closed = false, waiters = len=1 [{}], [raw]", task(&acquirer)),
    );
    view(
        "scarce",
        format!("children: closed = false, waiters = len=1 [{}], strong = 2, weak = 0, [raw]", task(&acquirer)),
    );
    view("shut", "children: closed = true, waiters = len=0 [], [raw]");

    view("quiet", "empty");
    view("notified", "not yet waiting");
    view("stored", "notified");
    view("awaited", format!("len=1 [{}]", task(&notifiee)));

    view("bounded", "len=2 [10, 20]");
    view("*bounded", "len=2 [10, 20]");
    view("bounded_receiver", "children: [0] = 10, [1] = 20, capacity = 4, closed = false, senders = 1, [raw]");
    view("unbounded_receiver", "len=1 [\"one\"]");
    view("backlog", "count: 35");
    view("closing", "children: [0] = 1, capacity = 2, closed = true, senders = 1, [raw]");
    view("gone_receiver", "children: [0] = 3, capacity = 2, closed = true, senders = 0, [raw]");
    view("sent_receiver", 5);
    view("*sent_receiver", 5);
    view("empty", "empty");
    view("empty_receiver", "empty");
    view("abandoned_receiver", "closed");
    view("refused", "closed");
    view("received", "received");
    view("watched", 8);
    view("*watched", "children: version = 0, closed = false, receivers = 1, [raw]");
    view("watch_receiver", "children: version = 0, closed = false, receivers = 1, seen = true, [raw]");
    view("orphan", "children: version = 0, closed = true, receivers = 1, seen = true, [raw]");
    view("broadcaster", "1 sent");
    view("broadcaster", "children: receivers = 1, closed = false, capacity = 4, [raw]");
    view("broadcast_receiver", "len=1 [9]");
    view("lagging", "len=4 [3, 4, 5, 6]");
    view("lagging", "children: [0] = 3, [1] = 4, [2] = 5, [3] = 6, lagged = true, [raw]");

    view("instant", since_boot(instant.into_std()));
    view("sleep", "sleeping until +{duration 1m20s..1m40s}");
    view("elapsed", "elapsed");
    view("registered", "sleeping until +{duration 1m30s..1m50s}");
    view("interval", "every 1m0s: sleeping until +{duration 50s..1m10s}");
    view("current_sleep", "sleeping until +{duration 1m10s..1m30s}");

    view("set", format!("len=2 [task {second} pending, task {first} pending]"));
    view("mixed", format!("len=2 [3, task {waiting} pending] (any order)"));

    view("listener", format!("fd {}", listener.as_raw_fd()));
    view("client", format!("fd {client_fd}"));
    view("client_reader", format!("fd {client_fd}"));
    view("lines", format!("fd {accepted_fd}"));
    view("lines.reader", format!("fd {accepted_fd}"));
    view("lines.reader", "children: buffered = 0, [raw]");
    view("lines.reader.inner", format!("fd {accepted_fd}"));
    view("buffered.inner", format!("fd {accepted_fd}"));
    view("buffered", format!("fd {accepted_fd}"));
    view("buffered", "children: buffered = 6, [raw]");
    view("client_writer", format!("fd {client_fd}"));
    view("read", format!("reading up to 16 bytes from fd {client_fd}"));
    view("write_all", format!("writing 5 bytes to fd {client_fd}"));
    view("udp", format!("fd {}", udp.as_raw_fd()));
    view("unix_peer", format!("fd {}", unix_peer.as_raw_fd()));
    view("unix_read", format!("fd {unix_left_fd}"));
    view("unix_write", format!("fd {unix_left_fd}"));
    view("unix_borrowed_read", format!("fd {unix_right_fd}"));
    view("unix_borrowed_write", format!("fd {unix_right_fd}"));
    view("read_exact", format!("reading 8 more bytes from fd {unix_fd}"));
    view("unix_listener", format!("fd {}", unix_listener.as_raw_fd()));
    barrier();
    truth::dump_core_if_asked();



    release.send(11).expect("a receiver");
    until_finished(&pending);
    drop(contended_guard);
    until_finished(&locker);
    bounded_receiver.try_recv().expect("a value");
    watched.send(9).expect("a receiver");
    quiet.notify_one();

    view("pending", 11);
    view("contended", "children: locked = false, waiters = len=0 [], strong = 1, weak = 0, [raw]");
    view("bounded", "len=1 [20]");
    view("watch_receiver", 9);
    view("watch_receiver", "children: version = 1, closed = false, receivers = 1, seen = false, [raw]");
    view("quiet", "notified");
    barrier();

    black_box((&finished, &panicked, &cancelled, &taken, &failure, &abort, &waker));
    black_box((&unlocked, &held, &guard, &contended, &readers, &read_guard));
    black_box((&written, &write_guard, &shared, &shared_guard, &semaphore, &permit));
    black_box((&scarce, &scarce_permit, &shut));
    black_box((&quiet, &notified, &stored, &awaited));
    black_box((&bounded, &bounded_receiver, &unbounded, &unbounded_receiver, &backlog_receiver));
    black_box((&closing, &closed_receiver, &gone_receiver, &sent_receiver, &empty));
    black_box((&empty_receiver, &abandoned_receiver, &refused, &refusing, &received));
    black_box((&watched, &watch_receiver, &orphan, &broadcaster, &broadcast_receiver));
    black_box((&flood, &lagging, &instant, &sleep, &elapsed, &registered, &interval));
    black_box((&current_sleep, &set, &mixed));
    black_box((&listener, &lines, &buffered, &read, &write_all, &udp));
    black_box((&unix_peer, &unix_read, &unix_write, &read_exact, &unix_listener));
    black_box((&unix_borrowed_read, &unix_borrowed_write));

    drop(write_guard);
    drop(shared_guard);
    drop(scarce_permit);
    awaited.notify_one();
    runtime.shutdown_background();
    current.shutdown_background();
}
