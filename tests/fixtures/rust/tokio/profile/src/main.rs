




use std::hint::black_box;

const TASKS: u64 = 4;
const ROUNDS: u64 = 5;

#[inline(never)]
fn work(round: u64) -> u64 {
    black_box(round) * 3
}

async fn inner(round: u64) -> u64 {
    tokio::task::yield_now().await;
    work(round)
}

async fn outer(round: u64) -> u64 {
    inner(round).await + 1
}

async fn task(me: u64) -> u64 {
    let mut total = 0;
    for round in 0..ROUNDS {
        total += outer(me * ROUNDS + round).await;
    }
    total
}

fn main() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .build()
        .expect("a runtime");
    let tasks: Vec<_> = (0..TASKS).map(|me| runtime.spawn(task(me))).collect();
    let sum: u64 = runtime.block_on(async {
        let mut sum = 0;
        for task in tasks {
            sum += task.await.expect("a task ends");
        }
        sum
    });
    println!("{sum}");
}
