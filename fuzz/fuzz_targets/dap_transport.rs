#![no_main]

use libfuzzer_sys::fuzz_target;

#[allow(dead_code, reason = "the target exercises reading only")]
#[path = "../../src/dap/transport.rs"]
mod transport;

fuzz_target!(|data: &[u8]| {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");

    runtime.block_on(async {
        let mut reader = tokio::io::BufReader::new(data);

        while let Ok(Some(frame)) = transport::read_frame(&mut reader).await {
            assert!(frame.body.len() <= transport::MAX_FRAME);
        }
    });
});
