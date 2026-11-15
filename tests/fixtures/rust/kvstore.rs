




use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::thread;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Get,
    Put,
}

#[derive(Debug)]
struct Entry {
    value: String,
}

#[derive(Debug, Default)]
struct Stats {
    gets: u64,
    puts: u64,
    misses: u64,
}

#[derive(Debug)]
struct Store {
    table: BTreeMap<String, Entry>,
    stats: Stats,
}

#[derive(Clone, Debug)]
struct Request {
    op: Op,
    key: String,
    value: &'static str,
}

#[derive(Default)]
struct Pending {
    items: VecDeque<Request>,
    closed: bool,
}

struct Queue {
    pending: Mutex<Pending>,
    changed: Condvar,
}

const CAPACITY: usize = 16;
const NAMES: [&str; 4] = ["alice", "bob", "carol", "dave"];

static SERVER: Mutex<Store> = Mutex::new(Store {
    table: BTreeMap::new(),
    stats: Stats {
        gets: 0,
        puts: 0,
        misses: 0,
    },
});
static QUEUE: Queue = Queue {
    pending: Mutex::new(Pending {
        items: VecDeque::new(),
        closed: false,
    }),
    changed: Condvar::new(),
};
static INPUT_ENDED: AtomicBool = AtomicBool::new(false);


fn handle_request(server: &Mutex<Store>, req: &Request) -> i32 {
    let mut store = server.lock().unwrap();
    let store = &mut *store;
    let found = store.table.contains_key(&req.key);
    let mut status = 0;
    match req.op {
        Op::Get => {
            store.stats.gets += 1;
            if !found {
                store.stats.misses += 1;
                status = -1;
            }
        }
        Op::Put => {
            store.stats.puts += 1;
            let entry = store.table.entry(req.key.clone()).or_insert_with(|| Entry {
                value: String::new(),
            });
            entry.value.clear();
            entry.value.push_str(req.value);
        }
    }
    status
}

fn next_request() -> Option<Request> {
    let mut pending = QUEUE.pending.lock().unwrap();
    while pending.items.is_empty() && !pending.closed {
        pending = QUEUE.changed.wait(pending).unwrap();
    }
    let req = pending.items.pop_front();
    if req.is_some() {
        QUEUE.changed.notify_all();
    }
    req
}

fn worker(id: u64) {
    while let Some(req) = next_request() {
        let status = handle_request(&SERVER, &req);
        let verb = if req.op == Op::Put { "put" } else { "get" };
        let mut out = std::io::stdout().lock();
        writeln!(out, "worker {id}: {verb} {} -> {status}", req.key).unwrap();
        out.flush().unwrap();
    }
}

fn submit(req: Request) {
    let mut pending = QUEUE.pending.lock().unwrap();
    while pending.items.len() == CAPACITY {
        pending = QUEUE.changed.wait(pending).unwrap();
    }
    pending.items.push_back(req);
    QUEUE.changed.notify_all();
}

fn read_input() {
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let mut out = std::io::stdout().lock();
        writeln!(out, "input: {line}").unwrap();
        out.flush().unwrap();
    }
    INPUT_ENDED.store(true, Ordering::SeqCst);
}

fn main() {
    let workers: Vec<_> = (1..=2)
        .map(|id| thread::spawn(move || worker(id)))
        .collect();
    let input = thread::spawn(read_input);

    let pause = Duration::from_millis(20);
    let mut round: u64 = 0;
    while !INPUT_ENDED.load(Ordering::SeqCst) {
        let op = if round % 3 == 2 { Op::Get } else { Op::Put };
        let key = format!("user:{}", 1000 + round % 24);
        submit(Request {
            op,
            key,
            value: NAMES[(round % 4) as usize],
        });
        thread::sleep(pause);
        round += 1;
    }
    QUEUE.pending.lock().unwrap().closed = true;
    QUEUE.changed.notify_all();
    for worker in workers {
        worker.join().unwrap();
    }
    input.join().unwrap();
    let stats = &SERVER.lock().unwrap().stats;
    println!(
        "served {} puts, {} gets, {} misses",
        stats.puts, stats.gets, stats.misses
    );
}
