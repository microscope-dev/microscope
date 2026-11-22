use proptest::prelude::*;

use super::{
    EnrollKey, EnrollKeyError, KeyPair, PollResponse, Server, ServerError, signed_message,
};

const RFC_8032_SEED: [u8; 32] = [
    0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec, 0x2c, 0xc4,
    0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03, 0x1c, 0xae, 0x7f, 0x60,
];

#[test]
fn requests_are_signed_exactly_as_services_checks_them() {
    let message = signed_message(
        "https://microscope.dev",
        "/api/agent/v1/refresh",
        b"{\"agent_id\":7}",
    );
    assert_eq!(
        message,
        b"ed25519\0microscope agent request v1\0https://microscope.dev\0POST /api/agent/v1/refresh\0{\"agent_id\":7}"
    );

    let pair = KeyPair::from_seed(RFC_8032_SEED).unwrap();
    assert_eq!(
        pair.signature_header(
            "https://microscope.dev",
            "/api/agent/v1/refresh",
            b"{\"agent_id\":7}"
        ),
        "ed25519 1xIJHJa5encBZVch8wC_T17sIx6wE6B_3LIGNKYPyapvuwki6qrp8JEMdZ4RFhi7pYyF0KKWExG_b0PYUHS0Cg"
    );
    assert_eq!(
        pair.public_key().fingerprint(),
        "SHA256:QDAjKeQfPMdlxEbMOQLsdwVuNewLif//OD7UUhTXxbA"
    );
    assert_eq!(
        serde_json::to_value(pair.public_key()).unwrap(),
        serde_json::json!({ "alg": "ed25519", "key": "11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo" })
    );
}

#[test]
fn enrollment_keys_are_checked_before_they_are_sent() {
    let key = "mse_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaasnb47oni";
    assert_eq!(EnrollKey::parse(key).unwrap().expose(), key);
    assert_eq!(
        format!("{:?}", EnrollKey::parse(key).unwrap()),
        "mse_[redacted]"
    );

    let typo = key.replacen("aaaa", "aaab", 1);
    assert_eq!(EnrollKey::parse(&typo), Err(EnrollKeyError::Mistyped));
    for not in [
        "",
        "mse_",
        "hunter2",
        &key.to_uppercase(),
        &format!("{key}a"),
        &key.replace('a', "1"),
    ] {
        assert_eq!(EnrollKey::parse(not), Err(EnrollKeyError::NotAKey), "{not}");
    }
}

#[test]
fn a_server_url_carries_the_origin_and_maybe_the_key() {
    let (server, key) = Server::parse("https://mse_abc@App.Microscope.dev").unwrap();
    assert_eq!(server.origin(), "https://app.microscope.dev");
    assert_eq!(key.as_deref(), Some("mse_abc"));
    assert_eq!(
        server.url("/api/agent/v1/refresh"),
        "https://app.microscope.dev/api/agent/v1/refresh"
    );
    assert!(!server.loopback());

    let (server, key) = Server::parse("https://microscope.example:8443/").unwrap();
    assert_eq!(server.origin(), "https://microscope.example:8443");
    assert_eq!(key, None);

    assert_eq!(
        Server::parse("http://127.0.0.1:9000").unwrap().0.origin(),
        "http://127.0.0.1:9000"
    );
    assert!(Server::parse("http://[::1]:9000").unwrap().0.loopback());
    assert!(Server::parse("http://localhost").unwrap().0.loopback());

    assert_eq!(
        Server::parse("http://microscope.dev"),
        Err(ServerError::Scheme)
    );
    assert_eq!(
        Server::parse("ftp://microscope.dev"),
        Err(ServerError::Scheme)
    );
    assert_eq!(Server::parse("microscope.dev"), Err(ServerError::Scheme));
    assert_eq!(Server::parse("https://"), Err(ServerError::NoHost));
    assert_eq!(Server::parse("https://key@"), Err(ServerError::NoHost));
    assert_eq!(
        Server::parse("https://microscope.dev/app"),
        Err(ServerError::Path)
    );
    assert_eq!(
        Server::parse("https://microscope.dev?x=1"),
        Err(ServerError::Path)
    );
    assert_eq!(
        Server::parse("http://127.0.0.1.evil.test"),
        Err(ServerError::Scheme)
    );
}

#[test]
fn poll_response_decodes_services_sanitizer_pattern_and_concurrent_bursts() {
    let raw = r#"{
        "version": 42,
        "revoked": false,
        "watch": null,
        "flush": null,
        "policy": {
            "version": 7,
            "sanitizer_version": 3,
            "preset": "prod",
            "time_unit": "1ms",
            "rules": [],
            "project_policies": [
                { "project": "checkout", "preset": "dev", "time_unit": "1us" }
            ],
            "sanitizer": {
                "enabled_builtins": ["emails"],
                "rules": [
                    {
                        "name": "stripe key",
                        "fields": ["syscall.string"],
                        "pattern": "sk_live_[0-9a-zA-Z]+",
                        "action": "redact",
                        "project": "checkout"
                    }
                ],
                "hash_key": "dGVzdGtleQ"
            }
        },
        "bursts": [
            { "id": 10, "duration_ms": 60000, "preset": "dev", "time_unit": "1us", "project": "checkout", "ends_at_ns": 999999999999 },
            { "id": 11, "duration_ms": 60000, "preset": "max", "time_unit": "1ns", "project": "billing", "ends_at_ns": 999999999999 }
        ],
        "poll_after_ms": 50
    }"#;
    let decoded: PollResponse = serde_json::from_str(raw).expect("decode services PollResponse");
    let policy = decoded.policy.expect("policy present");
    let san = policy.sanitizer.expect("sanitizer present");
    assert_eq!(san.rules.len(), 1);
    assert_eq!(san.rules[0].pattern, "sk_live_[0-9a-zA-Z]+");
    assert_eq!(san.rules[0].project.as_deref(), Some("checkout"));
    assert_eq!(decoded.bursts.len(), 2);
    assert_eq!(decoded.bursts[0].project.as_deref(), Some("checkout"));
    assert_eq!(decoded.bursts[1].project.as_deref(), Some("billing"));
}

impl PartialEq for super::KeyPair {
    fn eq(&self, other: &Self) -> bool {
        self.seed() == other.seed()
    }
}

proptest! {
    #[test]
    fn a_stored_key_reads_back_as_the_same_key(seed in any::<[u8; 32]>()) {
        let pair = KeyPair::from_seed(seed).unwrap();
        let read = KeyPair::decode(&pair.encode()).unwrap();
        prop_assert_eq!(read.public_key().fingerprint(), pair.public_key().fingerprint());
        prop_assert!(read == pair);
    }

    #[test]
    fn a_damaged_key_file_never_reads_as_a_key(text in "\\PC{0,80}") {
        if let Some(pair) = KeyPair::decode(&text) {
            let encoded = pair.encode();
            prop_assert_eq!(encoded.trim_end(), text.trim_end_matches('\n'));
        }
    }

    #[test]
    fn server_urls_never_panic_and_keep_their_scheme(url in "\\PC{0,60}") {
        if let Ok((server, _)) = Server::parse(&url) {
            prop_assert!(server.origin().starts_with("https://") || server.loopback());
        }
    }
}
