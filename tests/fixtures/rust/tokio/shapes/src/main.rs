







use std::future::Future;
use std::hint::black_box;
use std::pin::Pin;

async fn plain(value: u64) -> u64 {
    black_box(value + 1) // STEP: plain
}

async fn generic<T: Into<u64>>(value: T) -> u64 {
    black_box(value.into() + 4) // STEP: generic
}

trait Shape {
    async fn method(&self, value: u64) -> u64;
}

struct Square;

impl Shape for Square {
    async fn method(&self, value: u64) -> u64 {
        black_box(value + 5) // STEP: method
    }
}

impl Square {
    async fn inherent(&self, value: u64) -> u64 {
        black_box(value + 6) // STEP: inherent
    }
}

async fn shapes(me: u64) -> u64 {
    let a = plain(me).await; // INTO: plain
    let block = async move {
        black_box(me + 2) // STEP: block
    };
    let b = block.await; // INTO: block
    let boxed: Pin<Box<dyn Future<Output = u64> + Send>> = Box::pin(async move {
        black_box(me + 3) // STEP: boxed
    });
    let c = boxed.await; // INTO: boxed
    let small = u32::try_from(me).unwrap_or(0);
    let d = generic(small).await; // INTO: generic
    let e = Square.method(me).await; // INTO: method
    let h = Square.inherent(me).await; // INTO: inherent
    let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
    sender.send(me).await.unwrap_or(());
    let f = receiver.recv().await; // INTO: recv
    sender.send(me).await.unwrap_or(()); // STEP: recv-after
    let g = receiver.recv().await; // INTO: recv-again
    a + b + c + d + e + h + f.unwrap_or(0) + g.unwrap_or(0) // STEP: recv-again-after
}

fn main() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("a runtime");
    let total = runtime
        .block_on(runtime.spawn(shapes(1)))
        .expect("the task ends");
    truth::line(&[&"total", &total]);
}
