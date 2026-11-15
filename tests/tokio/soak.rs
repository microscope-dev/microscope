use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use microscope::{Condition, StopReason};

use crate::invariants::check_tokio_stop;
use crate::stops::{integer, line};
use crate::support::{ExternalProcess, Scenario};

const CLIENTS: u64 = 4;

const LINES_PER_ROUND: u64 = 64;

const ANSWER_TIMEOUT: Duration = Duration::from_mins(2);

const ROUND_TIMEOUT: Duration = Duration::from_mins(1);

const WARM_UP_ROUNDS: u64 = 20;
const MEMORY_MARGIN: u64 = 16 << 20;

struct Load {
    stop: Arc<AtomicBool>,
    answered: Arc<AtomicU64>,
    clients: Vec<JoinHandle<Result<(), String>>>,
}

impl Load {
    fn start(address: &str) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let answered = Arc::new(AtomicU64::new(0));
        let clients = (0..CLIENTS)
            .map(|client| {
                let (address, stop, answered) =
                    (address.to_owned(), Arc::clone(&stop), Arc::clone(&answered));
                std::thread::spawn(move || {
                    let stream = TcpStream::connect(&address).map_err(|error| error.to_string())?;

                    stream
                        .set_read_timeout(Some(ANSWER_TIMEOUT))
                        .map_err(|error| error.to_string())?;

                    let mut writer = stream.try_clone().map_err(|error| error.to_string())?;
                    let mut reader = BufReader::new(stream);
                    let mut sent = 0_u64;
                    while !stop.load(Ordering::Relaxed) {
                        sent += 1;
                        let line = format!("client {client} line {sent}");
                        writeln!(writer, "{line}").map_err(|error| error.to_string())?;

                        let mut answer = String::new();
                        reader
                            .read_line(&mut answer)
                            .map_err(|error| format!("{line}: {error}"))?;

                        if !answer.starts_with(&format!("{line} ")) {
                            return Err(format!("{line} was answered {answer:?}"));
                        }

                        answered.fetch_add(1, Ordering::Relaxed);
                    }

                    Ok(())
                })
            })
            .collect();
        Self {
            stop,
            answered,
            clients,
        }
    }

    fn answered(&self) -> u64 {
        self.answered.load(Ordering::Relaxed)
    }

    fn finish(self) -> u64 {
        self.stop.store(true, Ordering::Relaxed);
        for client in self.clients {
            client
                .join()
                .expect("a client panicked")
                .unwrap_or_else(|error| panic!("a client failed: {error}"));
        }

        self.answered.load(Ordering::Relaxed)
    }
}

fn live_heap() -> u64 {
    u64::try_from(microscope::timings::alloc::totals().live_bytes).expect("a live heap")
}

async fn attach(server: &ExternalProcess) -> Scenario {
    Scenario::attached("server", server.attach().await).checking_stops(check_tokio_stop)
}

async fn check(scenario: &Scenario, round: u64) {
    if let Err(problem) = check_tokio_stop(scenario.handle().clone()).await {
        panic!("round {round}: {problem}");
    }
}

#[tokio::test]
#[ignore = "run through `SOAK_MINUTES=N just stress`"]
async fn soak() {
    let seconds = std::env::var("MICROSCOPE_SOAK_SECONDS")
        .ok()
        .and_then(|seconds| seconds.parse().ok())
        .unwrap_or(60);

    let deadline = Instant::now() + Duration::from_secs(seconds);
    let server = ExternalProcess::spawn(&Scenario::fixture("tokio-server-o0"));

    let address = server
        .ready_line()
        .strip_prefix("READY ")
        .expect("the server's address")
        .to_owned();

    let load = Load::start(&address);
    let answer = line("server/src/main.rs", "// SERVED: answer");
    let mut scenario = attach(&server).await;
    let mut breakpoint = None;
    let mut baseline = None;
    let mut round = 0_u64;
    let mut hits = 0_u64;

    while Instant::now() < deadline {
        round += 1;

        if round.is_multiple_of(10) {
            scenario.shutdown().await;
            scenario = attach(&server).await;
            breakpoint = None;
        }

        check(&scenario, round).await;

        if round.is_multiple_of(3) {
            let added = scenario
                .add_source_breakpoint("server/src/main.rs", answer)
                .await;
            let condition = Condition::parse("served % 97 == 0").expect("a condition");
            scenario
                .operation(
                    "condition",
                    scenario
                        .handle()
                        .set_breakpoint_condition(added.id, Some(condition)),
                )
                .await;
            breakpoint = Some(added.id);
        } else if let Some(id) = breakpoint.take() {
            scenario.remove_breakpoint(id).await;
        }

        let before = load.answered();
        let mut running = scenario.start_resuming().await;
        let round_deadline = Instant::now() + ROUND_TIMEOUT;
        let reason = loop {
            if running.is_finished() {
                break (&mut running).await.expect("the resume").expect("a stop");
            }

            if load.answered() >= before + LINES_PER_ROUND {
                match scenario.handle().pause().await {
                    Ok(_) | Err(microscope::Error::AlreadyStopped) => {}
                    Err(error) => panic!("round {round}: pause: {error}"),
                }

                break running.await.expect("the resume").expect("a stop");
            }

            assert!(
                Instant::now() < round_deadline,
                "round {round}: the server answered {} lines",
                load.answered() - before
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        };

        check(&scenario, round).await;

        match reason {
            StopReason::Pause => {}
            StopReason::Breakpoint { .. } => {
                hits += 1;
                let count = integer(&scenario, "served").await;
                assert!(
                    count.is_some_and(|count| count % 97 == 0),
                    "round {round}: served {count:?}"
                );
            }
            other => panic!("round {round}: {other:?}"),
        }

        let heap = live_heap();
        match baseline {
            None if round == WARM_UP_ROUNDS => baseline = Some(heap),
            Some(baseline) => assert!(
                heap <= baseline + MEMORY_MARGIN,
                "round {round}: the debugger's live heap grew from {baseline} bytes to {heap}"
            ),
            None => {}
        }
    }

    if let Some(id) = breakpoint {
        scenario.remove_breakpoint(id).await;
    }

    scenario.shutdown().await;

    let answered = load.finish();
    assert!(answered > 0, "the server answered nothing");

    let mut quit = TcpStream::connect(&address).expect("connect to quit");
    writeln!(quit, "quit").expect("ask the server to quit");

    assert_eq!(server.wait().code(), Some(0));

    eprintln!("soak: {round} rounds, {hits} at the breakpoint, {answered} lines answered");
}
