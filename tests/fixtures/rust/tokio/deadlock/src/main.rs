




use std::sync::Arc;

use tokio::sync::{Barrier, Mutex};



async fn grab(name: &'static str, first: Arc<Mutex<u64>>, second: Arc<Mutex<u64>>, both: Arc<Barrier>) {
    let me = truth::start();
    truth::value(me, "holds", name);
    let mut held = first.lock().await;
    *held += me;
    both.wait().await;
    let _at = truth::at(me, "lock");
    let wanted = second.lock().await; // AWAIT: lock
    drop((held, wanted));
    truth::end(me);
}

fn main() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .build()
        .expect("a runtime");
    let left = Arc::new(Mutex::new(0));
    let right = Arc::new(Mutex::new(0));
    let both = Arc::new(Barrier::new(2));
    runtime.spawn(grab("left", Arc::clone(&left), Arc::clone(&right), Arc::clone(&both)));
    runtime.spawn(grab("right", right, left, both));
    while !(truth::parked_at("lock") == 2 && truth::workers_parked(runtime.handle())) {
        std::thread::yield_now();
    }
    truth::checkpoint("deadlock", Some(runtime.handle()));
    std::process::exit(0);
}
