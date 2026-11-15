
















use std::hint::black_box;
use std::io::BufRead as _;
use std::time::Duration;

use tokio::sync::oneshot;

async fn waiting(me: u64, gate: oneshot::Receiver<u64>) -> u64 {
    let before = black_box(me);
    let _at = truth::at(me, "gate");
    let got = gate.await.unwrap_or(0); // AWAIT: waiting
    before + got // STEP: waiting-after
}

async fn alone(gate: oneshot::Receiver<u64>) -> u64 {
    let me = truth::start();
    let got = waiting(me, gate).await; // AWAIT: alone
    truth::end(me);
    got
}

async fn selecting(gate: oneshot::Receiver<u64>, other: oneshot::Receiver<()>) -> u64 {
    let me = truth::start();


    let picked = tokio::select! {
        biased;
        got = waiting(me, gate) => got, // AWAIT: select
        _ = other => 0, // STEP: select-other
    };
    truth::end(me); // STEP: select-end
    black_box(picked)
}

async fn timing(gate: oneshot::Receiver<u64>) -> u64 {
    let me = truth::start();
    let timed = tokio::time::timeout(Duration::from_millis(20), waiting(me, gate)).await; // AWAIT: timeout
    truth::end(me); // STEP: timeout-after
    black_box(timed.unwrap_or(0))
}


fn parked() {
    while truth::parked_at("gate") != 1 {
        std::thread::yield_now();
    }
}

fn main() {
    let mode = std::env::args().nth(1).expect("a mode");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_time()
        .build()
        .expect("a runtime");
    let (open, gate) = oneshot::channel::<u64>();
    let result = match mode.as_str() {
        "abort" => {
            let task = runtime.spawn(alone(gate));
            parked();
            task.abort();
            runtime.block_on(task).map_or(0, black_box)
        }
        "select" => {
            let (finish, other) = oneshot::channel();
            let task = runtime.spawn(selecting(gate, other));
            parked();
            finish.send(()).expect("the task selects");
            runtime.block_on(task).expect("the task ends")
        }
        "timeout" => runtime
            .block_on(runtime.spawn(timing(gate)))
            .expect("the task ends"),
        "shutdown" => {
            drop(runtime.spawn(alone(gate)));
            parked();
            drop(runtime);
            0
        }
        "current-shutdown" => {
            let current = tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("a runtime");
            current.block_on(async {
                drop(tokio::spawn(alone(gate)));
                while truth::parked_at("gate") != 1 {
                    tokio::task::yield_now().await;
                }
            });
            drop(current);
            0
        }
        "hold" => {
            let task = runtime.spawn(alone(gate));
            parked();
            while !truth::workers_parked(runtime.handle()) {
                std::thread::yield_now();
            }
            truth::line(&[&"held"]);
            let mut input = String::new();
            std::io::stdin().lock().read_line(&mut input).expect("a line");
            if input.trim_end() == "exit" {
                std::process::exit(3);
            }
            open.send(7).expect("the task waits");
            return truth::line(&[&"result", &runtime.block_on(task).expect("the task ends")]);
        }
        other => panic!("unknown mode {other}"),
    };
    truth::line(&[&"result", &result]);

    drop(open);
}
