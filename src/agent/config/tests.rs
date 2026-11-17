use std::{collections::BTreeMap, path::Path};

use super::{ConfigError, Limits, load, state_dir};
use crate::agent::scratch::Scratch;

const KEY: &str = "mse_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaasnb47oni";
const ENROLL: &str = "https://mse_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaasnb47oni@a.test";

struct Written {
    _scratch: Scratch,
    path: std::path::PathBuf,
}

impl Written {
    fn path(&self) -> &Path {
        &self.path
    }
}

fn file(text: &str) -> Written {
    let scratch = Scratch::new();
    let path = scratch.file("agent.toml", text);
    Written {
        _scratch: scratch,
        path,
    }
}

fn env(pairs: &[(&'static str, &'static str)]) -> BTreeMap<&'static str, &'static str> {
    pairs.iter().copied().collect()
}

#[test]
fn a_whole_file_reads_with_its_defaults() {
    let config = file(&format!(
        r#"
enroll = "https://{KEY}@app.microscope.dev"
environment = "prod"
cluster = "checkout-us1"
state_dir = "/tmp/agent"
report_network = false

[labels]
team = "payments"

[limits]
cpu_percent = 5
memory_mib = 256
"#
    ));
    let loaded = load(Some(config.path()), &env(&[])).unwrap();
    assert_eq!(loaded.server.origin(), "https://app.microscope.dev");
    assert_eq!(loaded.enroll_key.expose(), KEY);
    assert_eq!(loaded.environment.as_deref(), Some("prod"));
    assert_eq!(loaded.cluster.as_deref(), Some("checkout-us1"));
    assert_eq!(loaded.labels["team"], "payments");
    assert_eq!(loaded.state_dir, Path::new("/tmp/agent"));
    assert!(loaded.cloud_metadata);
    assert!(!loaded.report_network);
    assert_eq!(
        loaded.limits,
        Limits {
            cpu_percent: Some(5),
            memory_mib: Some(256)
        }
    );
    assert_eq!(loaded.path.as_deref(), Some(config.path()));
}

#[test]
fn the_environment_wins_over_the_file() {
    let config = file(&format!(
        "enroll = \"https://{KEY}@a.test\"\nenvironment = \"prod\"\n[labels]\nteam = \"a\"\ntier = \"web\"\n"
    ));
    let loaded = load(
        Some(config.path()),
        &env(&[
            ("MICROSCOPE_ENVIRONMENT", "staging"),
            ("MICROSCOPE_LABELS", "team=b, zone = 2"),
            ("MICROSCOPE_STATE_DIR", "/run/agent"),
            ("MICROSCOPE_NODE_NAME", "ip-10-0-3-17"),
            ("MICROSCOPE_NODE_IPS", "10.0.3.17,fd00::17"),
            ("MICROSCOPE_CLOUD_METADATA", "false"),
        ]),
    )
    .unwrap();
    assert_eq!(loaded.environment.as_deref(), Some("staging"));
    assert_eq!(
        loaded.labels,
        BTreeMap::from([
            ("team".into(), "b".into()),
            ("tier".into(), "web".into()),
            ("zone".into(), "2".into())
        ])
    );
    assert_eq!(loaded.state_dir, Path::new("/run/agent"));
    assert_eq!(loaded.node.as_deref(), Some("ip-10-0-3-17"));
    assert_eq!(loaded.node_ips.len(), 2);
    assert!(!loaded.cloud_metadata);
}

#[test]
fn a_container_needs_no_file_at_all() {
    let loaded = load(None, &env(&[("MICROSCOPE_ENROLL", ENROLL)])).unwrap();
    assert_eq!(loaded.server.origin(), "https://a.test");
    assert!(loaded.path.is_none() || Path::new(super::DEFAULT_PATH).exists());
}

type Case = (
    &'static str,
    &'static [(&'static str, &'static str)],
    fn(&ConfigError) -> bool,
);

#[test]
fn mistakes_are_named_before_anything_is_sent() {
    let cases: &[Case] = &[
        ("", &[], |e| matches!(e, ConfigError::NoServer)),
        ("enroll = \"https://a.test\"", &[], |e| {
            matches!(e, ConfigError::NoKey)
        }),
        ("enroll = \"http://a.test\"", &[], |e| {
            matches!(e, ConfigError::Server(_))
        }),
        ("enroll = \"https://mse_nope@a.test\"", &[], |e| {
            matches!(e, ConfigError::Key(_))
        }),
        ("url = \"https://a.test\"", &[], |e| {
            matches!(e, ConfigError::Parse { .. })
        }),
        ("[limits]\ncpu = 5", &[], |e| {
            matches!(e, ConfigError::Parse { .. })
        }),
        (
            "[limits]\ncpu_percent = 0",
            &[("MICROSCOPE_ENROLL", ENROLL)],
            |e| matches!(e, ConfigError::Cpu),
        ),
        (
            "[limits]\nmemory_mib = 1",
            &[("MICROSCOPE_ENROLL", ENROLL)],
            |e| matches!(e, ConfigError::Memory),
        ),
        (
            "environment = \" prod\"",
            &[("MICROSCOPE_ENROLL", ENROLL)],
            |e| matches!(e, ConfigError::Name { .. }),
        ),
        (
            "[labels]\n\"bad key\" = \"x\"",
            &[("MICROSCOPE_ENROLL", ENROLL)],
            |e| matches!(e, ConfigError::Labels(_)),
        ),
        (
            "",
            &[
                ("MICROSCOPE_ENROLL", ENROLL),
                ("MICROSCOPE_LABELS", "novalue"),
            ],
            |e| matches!(e, ConfigError::Labels(_)),
        ),
        (
            "",
            &[
                ("MICROSCOPE_ENROLL", ENROLL),
                ("MICROSCOPE_REPORT_NETWORK", "yes"),
            ],
            |e| matches!(e, ConfigError::Bool { .. }),
        ),
        (
            "",
            &[
                ("MICROSCOPE_ENROLL", ENROLL),
                ("MICROSCOPE_NODE_IPS", "10.0.0.300"),
            ],
            |e| matches!(e, ConfigError::NodeIp(_)),
        ),
    ];
    for (text, pairs, expected) in cases {
        let config = file(text);
        let error = load(
            Some(config.path()),
            &pairs.iter().copied().collect::<BTreeMap<_, _>>(),
        )
        .unwrap_err();
        assert!(expected(&error), "{text:?} {pairs:?}: {error}");
    }

    let missing = Path::new("/nonexistent/agent.toml");
    assert!(matches!(
        load(Some(missing), &env(&[])),
        Err(ConfigError::Read { .. })
    ));
}

#[test]
fn the_environment_names_another_server_and_key() {
    let config = file(&format!("enroll = \"https://{KEY}@a.test\""));
    let loaded = load(
        Some(config.path()),
        &env(&[(
            "MICROSCOPE_ENROLL",
            "https://mse_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbblgterh7w@b.test",
        )]),
    )
    .unwrap();
    assert_eq!(loaded.server.origin(), "https://b.test");
    assert!(loaded.enroll_key.expose().starts_with("mse_bbbb"));
}

#[test]
fn status_finds_the_state_directory_without_a_server() {
    let config = file("state_dir = \"/srv/agent\"");
    assert_eq!(
        state_dir(Some(config.path()), &env(&[])).unwrap(),
        Path::new("/srv/agent")
    );
    assert_eq!(
        state_dir(Some(config.path()), &env(&[("MICROSCOPE_STATE_DIR", "/x")])).unwrap(),
        Path::new("/x")
    );
}
