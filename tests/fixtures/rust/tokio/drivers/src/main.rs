














use std::hint::black_box;
use std::sync::mpsc;

use tokio::sync::oneshot;


const ME: u64 = 0;

async fn waiting(release: oneshot::Receiver<u32>) -> u32 {
    let waiting_local = black_box(7_u64);
    truth::value(ME, "waiting_local", waiting_local);
    let _at = truth::at(ME, "waiting");
    let got = release.await.unwrap_or(0); // AWAIT: waiting
    got + u32::try_from(waiting_local).unwrap_or(0)
}

async fn driven(release: oneshot::Receiver<u32>) -> u32 {
    let driven_local = black_box(9_u64);
    truth::value(ME, "driven_local", driven_local);
    let _at = truth::at(ME, "driven");
    let got = waiting(release).await; // AWAIT: driven
    got + u32::try_from(driven_local).unwrap_or(0)
}




fn checkpoint(driver: i32, release: oneshot::Sender<u32>) {
    while truth::parked_at("waiting") != 1 || !truth::thread_parked(driver) {
        std::thread::yield_now();
    }
    truth::line(&[&"driver", &driver]);
    truth::checkpoint("parked", None);
    release.send(1).expect("the future waits");
}

#[tokio::main]
async fn tokio_main(release: oneshot::Receiver<u32>) -> u32 {
    driven(release).await // AWAIT: main
}

fn main() {
    let (release, released) = oneshot::channel();
    let mode = std::env::args().nth(1);
    match mode.as_deref() {
        None => {
            let driver = truth::gettid();
            let watcher = std::thread::spawn(move || checkpoint(driver, release));
            black_box(tokio_main(released));
            watcher.join().expect("the checkpoint ends");
        }
        Some("current") => {
            let (started, driver) = mpsc::channel();
            let plain = std::thread::spawn(move || {
                started.send(truth::gettid()).expect("the program waits");
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .build()
                    .expect("a runtime");
                black_box(runtime.block_on(driven(released)));
            });
            checkpoint(driver.recv().expect("the thread starts"), release);
            plain.join().expect("the thread ends");
        }
        Some("handle") => {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .build()
                .expect("a runtime");
            let handle = runtime.handle().clone();
            let (started, driver) = mpsc::channel();
            let plain = std::thread::spawn(move || {
                started.send(truth::gettid()).expect("the program waits");
                black_box(handle.block_on(driven(released)));
            });
            checkpoint(driver.recv().expect("the thread starts"), release);
            plain.join().expect("the thread ends");
        }
        Some(other) => panic!("unknown mode {other}"),
    }
    truth::line(&[&"end"]);
}
