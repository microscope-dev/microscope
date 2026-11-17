use std::collections::BTreeMap;

use proptest::prelude::*;

use super::{describe, random_boot_id, route_target, utc};
use crate::agent::{config::load, store::Status};

#[test]
fn utc_matches_known_dates() {
    assert_eq!(utc(0), "1970-01-01 00:00:00 UTC");
    assert_eq!(utc(951_782_400), "2000-02-29 00:00:00 UTC");
    assert_eq!(utc(1_795_001_234), "2026-11-18 11:27:14 UTC");
    assert_eq!(utc(4_107_542_399), "2100-02-28 23:59:59 UTC");
    assert_eq!(utc(-1), "1969-12-31 23:59:59 UTC");
}

proptest! {
    #[test]
    fn utc_agrees_with_counting_days(days in 0_i64..200_000, secs in 0_i64..86_400) {
        let text = utc(days * 86_400 + secs);
        let next = utc((days + 1) * 86_400 + secs);
        prop_assert_ne!(&text[..10], &next[..10]);
        prop_assert_eq!(&text[11..], &next[11..]);
    }
}

#[test]
fn the_route_target_names_the_servers_port() {
    let target = |url: &'static str| {
        let enroll = url.replacen(
            "://",
            "://mse_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaasnb47oni@",
            1,
        );
        let env = BTreeMap::from([("MICROSCOPE_ENROLL", enroll.as_str())]);
        route_target(&load(Some(std::path::Path::new("/dev/null")), &env).unwrap())
    };
    assert_eq!(
        target("https://app.microscope.dev"),
        "app.microscope.dev:443"
    );
    assert_eq!(
        target("https://app.microscope.dev:8443"),
        "app.microscope.dev:8443"
    );
    assert_eq!(target("http://127.0.0.1:9000"), "127.0.0.1:9000");
    assert_eq!(target("http://localhost"), "localhost:80");
    assert_eq!(target("http://[::1]:9000"), "[::1]:9000");
    assert_eq!(target("http://[::1]"), "[::1]:80");
}

#[test]
fn boot_ids_made_up_look_like_the_kernels() {
    let id = random_boot_id();
    assert_eq!(id.len(), 36);
    assert_eq!(id.matches('-').count(), 4);
    assert_ne!(id, random_boot_id());
}

#[test]
fn status_reads_like_a_person_wrote_it() {
    let status = Status {
        server: "https://app.microscope.dev".into(),
        agent_id: Some(81723),
        fingerprint: Some("SHA256:QDAjKeQfPMdlxEbMOQLsdwVuNewLif//OD7UUhTXxbA".into()),
        state: "enrolled".into(),
        since: 1_795_000_000,
        last_refresh: Some(1_795_001_000),
        last_error: None,
        pid: 4242,
        running: true,
    };
    assert_eq!(
        describe(&status, true, 1_795_001_234),
        "server       https://app.microscope.dev
agent        81723
key          SHA256:QDAjKeQfPMdlxEbMOQLsdwVuNewLif//OD7UUhTXxbA
state        enrolled since 2026-11-18 11:06:40 UTC, 20 min ago
last refresh 2026-11-18 11:23:20 UTC, 3 min ago
last error   none
"
    );
    assert!(
        describe(&status, false, 1_795_001_234)
            .contains("not running (it was enrolled and did not stop cleanly)")
    );
    let stopped = Status {
        running: false,
        state: "stopped".into(),
        ..status
    };
    assert!(describe(&stopped, false, 1_795_001_234).contains("state        not running since"));
}
