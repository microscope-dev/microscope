use std::path::{Path, PathBuf};

use serde_json::json;

use crate::dap::{Configuration, Dap, Profile, fixture, line_of, source};

#[test]
fn the_filter_for_every_panic_stops_on_one_the_server_recovers_from() {
    let mut dap = Dap::start("go panics");
    let started = dap.launch(
        Profile::VsCode,
        &fixture("server-go-o0"),
        json!({}),
        &Configuration {
            exceptions: Some(json!({"filters": ["unhandled", "runtime-fatal", "raised"]})),
            ..Configuration::default()
        },
    );
    let stop = dap.stopped(started.mark);
    assert_eq!(stop.reason, "exception", "{stop:?}");
    let info = dap.request("exceptionInfo", json!({"threadId": stop.thread}));
    assert!(
        info["description"]
            .as_str()
            .is_some_and(|text| text.contains("assignment to entry in nil map")),
        "{info}"
    );

    let frames = dap.inspect_as(Profile::VsCode, &stop);
    let handler = frames
        .iter()
        .position(|frame| frame.get("presentationHint").is_none())
        .unwrap_or_else(|| panic!("{frames:#?}"));
    assert_eq!(frames[handler]["name"], "main.broken", "{frames:#?}");

    let main = source("go/server/main.go");
    assert_eq!(
        frames[handler]["line"],
        line_of(&main, "// SERVER: broken"),
        "{frames:#?}"
    );

    dap.finish();
}

#[test]
fn a_build_without_its_source_paths_shows_them_through_a_source_map() {
    let local = source("go/server/main.go");
    let line = line_of(&local, "// SERVER: greet");
    let configuration = Configuration {
        sources: vec![(local.clone(), vec![line])],
        ..Configuration::default()
    };

    let shown = |arguments| {
        let mut dap = Dap::start("trimmed paths");
        let started = dap.launch(
            Profile::VsCode,
            &fixture("server-go-trimpath"),
            arguments,
            &configuration,
        );
        let stop = dap.stopped(started.mark);
        assert_eq!(stop.reason, "breakpoint", "{stop:?}");
        let frames = dap.inspect_as(Profile::VsCode, &stop);
        assert_eq!(frames[0]["line"], line, "{frames:#?}");
        let path = frames[0]["source"]["path"]
            .as_str()
            .unwrap_or_else(|| panic!("{frames:#?}"))
            .to_owned();

        dap.finish();

        PathBuf::from(path)
    };

    let recorded = shown(json!({}));
    assert!(!recorded.exists(), "{}", recorded.display());

    let (from, to) = prefixes(&recorded, &local);
    let mapped = shown(json!({"sourceMap": {from.to_str().expect("a path"): to}}));
    assert_eq!(mapped, local);
}

fn prefixes(recorded: &Path, local: &Path) -> (PathBuf, PathBuf) {
    let recorded = recorded.components().collect::<Vec<_>>();
    let local = local.components().collect::<Vec<_>>();
    let shared = recorded
        .iter()
        .rev()
        .zip(local.iter().rev())
        .take_while(|(left, right)| left == right)
        .count()
        .min(recorded.len() - 1);

    (
        recorded[..recorded.len() - shared].iter().collect(),
        local[..local.len() - shared].iter().collect(),
    )
}
