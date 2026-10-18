//! A program instrumented with microscope's probes SDK. Each mode waits until
//! microscope listens, does its work, and exits.
//!
//! - `zones`: three rounds of nested zones, each in a label, with a plot,
//!   a message, and a frame, while a worker thread runs zones of its own,
//!   then a trigger.
//! - `burst COUNT`: COUNT zones as fast as it can write them.
//! - `churn COUNT`: COUNT threads, one after another, each one zone.
//! - `fork`: a zone around a forked child, whose own zone records nothing.
//! - `exec`: executes this program again as `execed`, which waits until
//!   microscope listens to the new program and runs a zone.
//! - `quiet`: says it is ready to be attached to, then zones until the
//!   sites switch themselves off.
//! - `cost COUNT`: times COUNT zones as they are, off unless microscope reads;
//!   `cost-on COUNT` waits until it does.
//! - `rate`: zones without end, a line for every hundred, listened to or
//!   not.
//! - `defined`: sites defined as it runs, as a language that names its
//!   zones at run time does: three rounds of a zone, a plot, and a zone
//!   in another name, then a message whose site it defines once microscope
//!   listens.
//! - `defines`: for each name a line of its standard input gives, a zone's
//!   site of that name, and a line with the address of its state byte.

use std::io::Write as _;
use std::time::{Duration, Instant};

use microscope_probes::{abi, define, frame, label, message, plot, trigger, zone};

fn main() {
    let arguments: Vec<String> = std::env::args().collect();
    let count = || arguments[2].parse::<u64>().expect("a count");
    if arguments[1] == "quiet" {
        println!("READY");
    }
    if arguments[1] == "cost" {
        cost(count());
        return;
    }
    if arguments[1] == "rate" {
        rate();
        return;
    }
    if arguments[1] == "defines" {
        defines();
        return;
    }
    if arguments[1] == "defined" {
        defined();
        return;
    }
    wait_until_listening();
    match arguments[1].as_str() {
        "zones" => zones(),
        "burst" => burst(count()),
        "churn" => churn(count()),
        "fork" => forked(),
        "exec" => exec(),
        "execed" => {
            let _zone = zone!("after");
        }
        "quiet" => quiet(),
        "exiting" => exiting(),
        "cost-on" => cost(count()),
        mode => panic!("unknown mode {mode}"),
    }
}

/// Waits until microscope has switched the sites on.
fn wait_until_listening() {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !microscope_probes::listening() {
        assert!(Instant::now() < deadline, "microscope never listened");
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn zones() {
    let worker = std::thread::Builder::new()
        .name("worker".into())
        .spawn(|| {
            let _zone = zone!("work");
            for _ in 0..2 {
                let _step = zone!("step");
                std::hint::black_box(spin(1000));
            }
        })
        .expect("spawn");
    for round in 0..3_u32 {
        let _round = label!("round", round);
        let _outer = zone!("outer");
        {
            let _a = zone!("a");
            std::hint::black_box(spin(100));
        }
        {
            let _b = zone!("b");
            let _leaf = zone!("leaf");
            plot!("round", f64::from(round));
            message!("round {round} of {}", 3);
        }
        frame!();
    }
    worker.join().expect("join");
    trigger!("done");
}

fn burst(count: u64) {
    for _ in 0..count {
        let _tick = zone!("tick");
    }
}

fn churn(count: u64) {
    for _ in 0..count {
        std::thread::spawn(|| {
            let _life = zone!("life");
        })
        .join()
        .expect("join");
    }
}

unsafe extern "C" {
    fn fork() -> i32;
    fn waitpid(pid: i32, status: *mut i32, options: i32) -> i32;
    fn _exit(status: i32) -> !;
}

fn forked() {
    let _parent = zone!("parent");
    // SAFETY: the child only writes and exits.
    let child = unsafe { fork() };
    if child == 0 {
        {
            let _child = zone!("child");
        }
        let line = format!("child listening: {}\n", microscope_probes::listening());
        let _ = std::io::stdout().write_all(line.as_bytes());
        let _ = std::io::stdout().flush();
        // SAFETY: the child ends here, without its parent's exit handlers.
        unsafe { _exit(0) };
    }
    let mut status = 0;
    // SAFETY: the child is this process's.
    unsafe { waitpid(child, &raw mut status, 0) };
}

fn exec() {
    let program = std::env::current_exe().expect("this program");
    let error =
        std::os::unix::process::CommandExt::exec(std::process::Command::new(program).arg("execed"));
    panic!("exec: {error}");
}

fn quiet() {
    println!("listening");
    let deadline = Instant::now() + Duration::from_secs(30);
    while microscope_probes::listening() {
        assert!(Instant::now() < deadline, "the sites never switched off");
        let _spin = zone!("spin");
        std::hint::black_box(spin(10));
    }
    println!("quiet");
}

fn cost(count: u64) {
    let started = Instant::now();
    for _ in 0..count {
        let _tick = zone!("cost");
    }
    #[expect(clippy::cast_precision_loss, reason = "a mean")]
    let each = started.elapsed().as_secs_f64() * 1e9 / count as f64;
    let state = if microscope_probes::listening() { "on" } else { "off" };
    println!("a zone {state}: {each:.2} ns");
}

/// Steady work for measuring what recording costs: units of a hundred
/// zones, each around some microseconds of work, as instrumentation would
/// mark a request's steps, a line written as each unit ends, whether or
/// not microscope listens. A measurer counts the lines in a window.
fn rate() {
    let mut out = std::io::stdout();
    loop {
        for round in 0..100 {
            let _zone = zone!("rate");
            std::hint::black_box(spin(50_000 + round));
        }
        // Writing fails once the measurer stops reading.
        if writeln!(out, "u").is_err() {
            return;
        }
    }
}

fn defined() {
    let load = define(abi::SITE_ZONE, "dynamic.load", "app.py", 3).expect("a zone");
    let rows = define(abi::SITE_PLOT, "dynamic.rows", "app.py", 4).expect("a plot");
    let other = define(abi::SITE_ZONE, "other", "app.py", 5).expect("a zone");
    wait_until_listening();
    for round in 0..3_u32 {
        let _load = load.zone();
        rows.plot(f64::from(round));
        let _other = other.zone();
    }

    let late = define(abi::SITE_MESSAGE, "dynamic.late", "app.py", 9).expect("a message");
    let deadline = Instant::now() + Duration::from_secs(30);
    while !late.on() {
        assert!(Instant::now() < deadline, "microscope never switched it on");
        std::thread::sleep(Duration::from_millis(1));
    }
    late.message(format_args!("defined {}", "late"));
}

fn defines() {
    for name in std::io::stdin().lines() {
        let name = name.expect("a name");
        let site = define(abi::SITE_ZONE, &name, "app.py", 1).expect("a zone");
        // The ABI puts a run-time site's state byte at its offset 1.
        println!("SITE {:#x}", std::ptr::from_ref(site).addr() + 1);
        std::io::stdout().flush().expect("flush");
    }
}

/// Once a line arrives, defines a site, records a zone of it, and exits,
/// waiting at its exit for microscope to read what it wrote.
fn exiting() {
    println!("LISTENING");
    std::io::stdout().flush().expect("flush");
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).expect("a line");
    let site = define(abi::SITE_ZONE, "last", "app.py", 1).expect("a zone");
    drop(site.zone());
    println!("DONE");
    std::io::stdout().flush().expect("flush");
}

fn spin(rounds: u64) -> u64 {
    (0..rounds).fold(0, |sum, value| std::hint::black_box(sum ^ value))
}
