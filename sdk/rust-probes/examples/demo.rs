//! A small instrumented service to record and explore with microscope.
//!
//! A producer queues requests at a rate that rises and falls; four workers
//! take them, each request a zone labeled with its route, holding nested
//! zones for its steps, and a cache miss now and then that goes to the
//! "database". The queue's depth is plotted, slow requests leave a message,
//! and a render thread marks a frame sixty times a second.
//!
//! From `sdk/rust-probes`, building with debug information so stacks show
//! the functions the compiler inlined:
//!
//! ```sh
//! CARGO_PROFILE_RELEASE_DEBUG=true cargo build --release --example demo --features enabled
//! microscope record -o demo.usprof -- target/release/examples/demo 5
//! microscope web demo.usprof
//! ```

use std::collections::VecDeque;
use std::hint::black_box;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use microscope_probes::{frame, label, message, plot, zone};

const WORKERS: usize = 4;
const ROUTES: [&str; 4] = ["/users", "/orders", "/search", "/health"];

/// One request: its route, and how much work each of its steps takes.
struct Request {
    id: u64,
    route: &'static str,
    parse: u64,
    query: u64,
    miss: bool,
    render: u64,
}

/// A small, seeded random number generator, so runs are alike.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }
}

/// Spins for about `units` microseconds.
#[inline(never)]
fn work(units: u64) -> u64 {
    (0..units * 3000).fold(0, |sum, value| black_box(sum ^ value))
}

#[inline(never)]
fn parse(request: &Request) {
    let _zone = zone!("parse");
    black_box(work(request.parse));
}

#[inline(never)]
fn database(request: &Request) {
    let _zone = zone!("database");
    black_box(work(request.query * 4));
}

#[inline(never)]
fn query(request: &Request) {
    let _zone = zone!("query");
    black_box(work(request.query));
    if request.miss {
        let _miss = zone!("cache miss");
        database(request);
    }
}

#[inline(never)]
fn render(request: &Request) {
    let _zone = zone!("render");
    black_box(work(request.render));
}

fn handle(request: &Request) {
    let started = Instant::now();
    let _zone = zone!("request");
    let _route = label!("route", request.route);

    parse(request);
    query(request);
    render(request);

    let took = started.elapsed();
    if took > Duration::from_millis(2) {
        message!(
            "request {} to {} took {:?}",
            request.id,
            request.route,
            took
        );
    }
}

/// The queue workers take requests from, and whether the run is over.
struct Queue {
    requests: Mutex<VecDeque<Request>>,
    ready: Condvar,
    done: AtomicBool,
}

fn worker(queue: &Queue) {
    loop {
        let request = {
            let mut requests = queue.requests.lock().expect("the queue");
            loop {
                if let Some(request) = requests.pop_front() {
                    break request;
                }

                if queue.done.load(Ordering::Relaxed) {
                    return;
                }

                requests = queue
                    .ready
                    .wait_timeout(requests, Duration::from_millis(10))
                    .expect("the queue")
                    .0;
            }
        };

        handle(&request);
    }
}

fn produce(queue: &Queue, until: Instant) {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let started = Instant::now();
    let mut id = 0;

    while Instant::now() < until {
        // The load rises and falls every two seconds.
        let phase = started.elapsed().as_millis() % 2000;
        let burst = if phase < 1000 { 1 + phase / 250 } else { 1 };

        for _ in 0..burst {
            id += 1;
            let route = ROUTES[usize::try_from(rng.below(4)).expect("an index")];
            let request = Request {
                id,
                route,
                parse: 20 + rng.below(30),
                query: if route == "/search" {
                    200 + rng.below(300)
                } else {
                    40 + rng.below(60)
                },
                miss: rng.below(10) == 0,
                render: if route == "/health" {
                    5
                } else {
                    50 + rng.below(100)
                },
            };

            let mut requests = queue.requests.lock().expect("the queue");
            requests.push_back(request);
            plot!("queue depth", requests.len() as f64);
            drop(requests);
            queue.ready.notify_one();
        }

        std::thread::sleep(Duration::from_micros(500));
    }
}

fn render_loop(queue: &Queue) {
    while !queue.done.load(Ordering::Relaxed) {
        {
            let _zone = zone!("draw");
            black_box(work(300));
        }

        frame!();
        std::thread::sleep(Duration::from_micros(16_667 - 300));
    }
}

fn main() {
    let seconds: u64 = std::env::args()
        .nth(1)
        .map_or(5, |seconds| seconds.parse().expect("a number of seconds"));

    // A recording switches the sites on just after the program starts.
    let deadline = Instant::now() + Duration::from_secs(2);
    while !microscope_probes::listening() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }

    let queue = Arc::new(Queue {
        requests: Mutex::new(VecDeque::new()),
        ready: Condvar::new(),
        done: AtomicBool::new(false),
    });
    let until = Instant::now() + Duration::from_secs(seconds);

    std::thread::scope(|scope| {
        for index in 0..WORKERS {
            let queue = &queue;
            std::thread::Builder::new()
                .name(format!("worker {index}"))
                .spawn_scoped(scope, move || worker(queue))
                .expect("a worker");
        }

        let queue = &queue;
        std::thread::Builder::new()
            .name("render".into())
            .spawn_scoped(scope, move || render_loop(queue))
            .expect("the render thread");

        produce(queue, until);
        queue.done.store(true, Ordering::Relaxed);
        queue.ready.notify_all();
    });
}
