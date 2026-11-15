



use std::collections::VecDeque;
use std::future::Future;
use std::hint::black_box;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};



struct Yield {
    pending: u32,
}

impl Future for Yield {
    type Output = u32;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<u32> {
        if self.pending == 0 { // STEP: yield-poll
            Poll::Ready(7)
        } else {
            self.pending -= 1;
            context.waker().wake_by_ref();
            Poll::Pending
        }
    }
}

fn pend() -> Yield {
    Yield { pending: 2 }
}

async fn ready(value: u32) -> u32 {
    value + 1 // STEP: ready
}

async fn leaf(id: u32) -> u32 {
    let doubled = id * 2; // STEP: leaf
    let label = format!("leaf {id}"); // STEP: label
    let resumed = pend().await; // AWAIT: leaf
    let after = doubled + resumed + black_box(label).len() as u32; // STEP: leaf-after
    black_box(after)
}

async fn middle(id: u32) -> u32 {
    let base = id + 10; // STEP: middle
    let first = ready(base).await; // STEP: middle-ready
    let second = leaf(id).await; // AWAIT: middle
    first + second // STEP: middle-after
}

async fn walk(id: u32) -> u32 {
    let mut total = 0;
    for step in 0..3 {
        total += step; // STEP: walk-body
        pend().await; // AWAIT: walk
    }
    total + id
}


type Task = Pin<Box<dyn Future<Output = ()>>>;


struct Noop;

impl Wake for Noop {
    fn wake(self: Arc<Self>) {}
}

fn run(tasks: Vec<Task>) {
    let waker = Waker::from(Arc::new(Noop));
    let mut context = Context::from_waker(&waker);
    let mut queue = tasks.into_iter().collect::<VecDeque<_>>();
    while let Some(mut task) = queue.pop_front() {
        if task.as_mut().poll(&mut context).is_pending() {
            queue.push_back(task);
        }
    }
}

static RESULTS: Mutex<Vec<u32>> = Mutex::new(Vec::new());

fn main() {
    let tasks: Vec<Task> = vec![
        Box::pin(async {
            let value = middle(3).await; // STEP: task-a
            RESULTS.lock().unwrap().push(value);
        }),
        Box::pin(async {
            let value = middle(4).await;
            RESULTS.lock().unwrap().push(value);
        }),
        Box::pin(async {
            let value = walk(5).await;
            RESULTS.lock().unwrap().push(value);
        }),
    ];
    run(tasks);
    println!("results {:?}", RESULTS.lock().unwrap());
}
