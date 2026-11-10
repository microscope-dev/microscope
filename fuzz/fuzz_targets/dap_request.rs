#![no_main]

use libfuzzer_sys::fuzz_target;

#[allow(dead_code, reason = "the target exercises parsing only")]
#[path = "../../src/dap/protocol.rs"]
mod protocol;

#[allow(dead_code, reason = "the target exercises the parser only")]
#[path = "../../src/present/completing.rs"]
mod complete;

fuzz_target!(|data: &[u8]| {
    match protocol::parse(data) {
        Ok(protocol::Incoming::Request {
            seq,
            command,
            ..
        }) => {
            let response = protocol::Outgoing::Response {
                request_seq: seq,
                command,
                result: Err(protocol::ErrorBody::new(String::from_utf8_lossy(data))),
            }
            .to_json(1);
            assert!(response["body"].is_object());
        }
        Ok(protocol::Incoming::Response { .. }) => {}
        Err(_) => {
            let _ = protocol::MessageError::request_seq(data);
        }
    }

    if let Ok(text) = std::str::from_utf8(data) {
        let _ = protocol::address(text);

        // What a completion completes is always the end of the text.
        let (_, partial, start) = complete::completing(text);
        assert_eq!(&text[start..], partial);
    }
});
