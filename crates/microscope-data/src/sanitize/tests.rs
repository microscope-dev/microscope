use proptest::prelude::*;

use super::*;

fn sanitizer(builtins: &[Builtin]) -> Sanitizer {
    Sanitizer::new(builtins, Vec::new(), HashKey::new(b"acme"))
}

fn clean(builtins: &[Builtin], class: FieldClass, value: &str) -> String {
    match sanitizer(builtins).text(class, value, &mut Tally::default()) {
        Outcome::Kept => value.to_owned(),
        Outcome::Replaced(changed) => changed,
        other => panic!("{value:?} gave {other:?}"),
    }
}

fn defaults(value: &str) -> String {
    clean(&Builtin::DEFAULT, FieldClass::ZoneText, value)
}

#[test]
fn each_default_rule_removes_its_secret_and_leaves_the_text_around_it() {
    let planted = [
        ("--password=hunter2 --port 80", "hunter2"),
        ("--api-key hunter2", "hunter2"),
        ("-db-token\0hunter2", "hunter2"),
        ("PGPASSWORD=hunter2 psql", "hunter2"),
        ("GET /cb?access_token=hunter2&x=1", "hunter2"),
        ("postgres://app:hunter2@db:5432/main", "hunter2"),
        ("key AKIAIOSFODNN7EXAMPLE used", "AKIAIOSFODNN7EXAMPLE"),
        (
            "aws_secret_access_key = wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
        ),
        (
            "push with ghp_0123456789abcdefghijABCDEFGHIJ012345",
            "ghp_0123456789abcdefghijABCDEFGHIJ012345",
        ),
        (
            "slack xoxb-1234567890-abcdefghij",
            "xoxb-1234567890-abcdefghij",
        ),
        (
            "jwt eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0In0.dozjgNryP4J3jVmNHl0w5N",
            "eyJzdWIiOiIxMjM0In0",
        ),
        (
            "-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKCAQEA\n-----END RSA PRIVATE KEY-----\nafter",
            "MIIEpAIBAAKCAQEA",
        ),
        ("Authorization: Bearer abc.def-ghi_jkl", "abc.def-ghi_jkl"),
        ("paid with 4111 1111 1111 1111 today", "4111 1111 1111"),
    ];

    for (value, secret) in planted {
        let cleaned = defaults(value);
        assert!(!cleaned.contains(secret), "{value:?} became {cleaned:?}");
        assert!(
            cleaned.len() < value.len() + 16,
            "{value:?} became {cleaned:?}"
        );
    }

    assert_eq!(
        defaults("--password=hunter2 --port 80"),
        "--password=[redacted] --port 80"
    );
    assert_eq!(
        defaults("paid with 4111 1111 1111 1111 today"),
        "paid with ***************1111 today"
    );
    assert!(
        defaults("-----BEGIN RSA PRIVATE KEY-----\nMIIE\n-----END RSA PRIVATE KEY-----\nafter")
            .ends_with("\nafter")
    );
}

#[test]
fn text_that_only_looks_like_a_secret_is_kept() {
    for value in [
        "token expired after 30 s",
        "order 4111 1111 1111 1112",
        "https://example.com/path:with@colon",
        "build 1234567890123",
        "eyJ is how a JWT starts",
        "/home/dev/src/main.rs",
    ] {
        assert_eq!(defaults(value), value, "{value:?}");
    }
}

#[test]
fn rules_off_by_default_are_switched_on_one_at_a_time() {
    let value = "mail ann@example.com from 10.1.2.3 and fe80::1 in /home/ann/x where id = 42 and n = 'O''Brien'";
    assert_eq!(defaults(value), value);

    let emails = clean(&[Builtin::Emails], FieldClass::ZoneText, value);
    assert!(
        !emails.contains("ann@example.com") && emails.contains("10.1.2.3"),
        "{emails}"
    );

    let ips = clean(&[Builtin::IpAddresses], FieldClass::ZoneText, value);
    assert!(
        !ips.contains("10.1.2.3") && !ips.contains("fe80::1"),
        "{ips}"
    );
    assert_eq!(
        clean(&[Builtin::IpAddresses], FieldClass::ZoneText, "at 12:30:45"),
        "at 12:30:45"
    );
    assert_eq!(
        clean(&[Builtin::IpAddresses], FieldClass::ZoneText, "v 1.2.3.400"),
        "v 1.2.3.400"
    );

    let homes = clean(
        &[Builtin::HomeDirectories],
        FieldClass::SyscallPath,
        "/home/ann/.ssh/config",
    );
    assert!(
        homes.starts_with("/home/[hash:") && homes.ends_with("]/.ssh/config"),
        "{homes}"
    );

    let sql = clean(&[Builtin::SqlLiterals], FieldClass::ZoneText, value);
    assert!(!sql.contains("42") && !sql.contains("Brien"), "{sql}");
}

#[test]
fn a_rule_reads_only_its_fields() {
    let all = sanitizer(&Builtin::ALL);
    let mut tally = Tally::default();
    assert_eq!(
        all.text(FieldClass::ThreadName, "--password=x", &mut tally),
        Outcome::Kept
    );
    assert_eq!(
        all.text(FieldClass::ProcessName, "ann@example.com", &mut tally),
        Outcome::Kept
    );
    assert_eq!(
        all.text(FieldClass::ModulePath, "/srv/'x'/42", &mut tally),
        Outcome::Kept
    );
    assert!(all.fired(&tally).all(|(_, times)| times == 0));
}

#[test]
fn hashing_groups_equal_values_within_an_organization_only() {
    let rule = |key: &[u8]| {
        let spec = RuleSpec {
            name: "tenant ids in paths".to_owned(),
            fields: vec!["syscall.path".to_owned(), "process.args".to_owned()],
            pattern: "/tenants/(?<value>[0-9a-f-]{36})/".to_owned(),
            action: Action::Hash,
        };
        Sanitizer::new(
            &[],
            vec![Rule::new(&spec).expect("a rule")],
            HashKey::new(key),
        )
    };
    let path = |id: &str| format!("/srv/tenants/{id}/data");
    let one = "0b6e7c1a-4f7e-4c43-9b8e-1c2d3e4f5a6b";
    let two = "9f9e7c1a-4f7e-4c43-9b8e-1c2d3e4f5a6b";
    let hashed = |sanitizer: &Sanitizer, value: &str| match sanitizer.text(
        FieldClass::SyscallPath,
        value,
        &mut Tally::default(),
    ) {
        Outcome::Replaced(changed) => changed,
        other => panic!("{other:?}"),
    };

    let acme = rule(b"acme");
    let other = rule(b"other");
    let first = hashed(&acme, &path(one));
    assert!(!first.contains(one), "{first}");
    assert!(
        first.starts_with("/srv/tenants/[hash:") && first.ends_with("]/data"),
        "{first}"
    );
    assert_eq!(first, hashed(&acme, &path(one)));
    assert_ne!(first, hashed(&acme, &path(two)));
    assert_ne!(first, hashed(&other, &path(one)));
}

#[test]
fn drop_actions_end_the_field_or_the_record() {
    let rule = |action| {
        Rule::new(&RuleSpec {
            name: "internal hosts".to_owned(),
            fields: vec!["zone.text".to_owned()],
            pattern: r"\.internal\b".to_owned(),
            action,
        })
        .expect("a rule")
    };
    for (action, outcome) in [
        (Action::DropField, Outcome::DropField),
        (Action::DropRecord, Outcome::DropRecord),
    ] {
        let sanitizer = Sanitizer::new(&Builtin::DEFAULT, vec![rule(action)], HashKey::new(b"k"));
        let mut tally = Tally::default();
        assert_eq!(
            sanitizer.text(FieldClass::ZoneText, "db.internal --token=x", &mut tally),
            outcome
        );
        let fired: Vec<_> = sanitizer
            .fired(&tally)
            .filter(|(_, times)| *times > 0)
            .collect();
        assert_eq!(fired, [("secrets in arguments", 1), ("internal hosts", 1)]);
    }
}

#[test]
fn arguments_are_read_together_so_a_flag_covers_the_next_one() {
    let args = ["psql", "--password", "hunter2", "-h", "db"].map(str::to_owned);
    let mut tally = Tally::default();
    let cleaned = sanitizer(&Builtin::DEFAULT).args(&args, &mut tally);
    assert_eq!(
        cleaned,
        Outcome::Replaced(
            ["psql", "--password", "[redacted]", "-h", "db"]
                .map(str::to_owned)
                .to_vec()
        )
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "covers multi-word, multiline, embedded shell, and cross-argument secret redaction"
)]
fn multi_word_secret_arguments_and_commands_are_fully_redacted() {
    let args = [
        "app",
        "--password",
        "correct horse battery",
        "--api-key=one two three",
        "--host",
        "db.example",
    ]
    .map(str::to_owned);
    let mut tally = Tally::default();
    let cleaned = sanitizer(&Builtin::DEFAULT).args(&args, &mut tally);
    assert_eq!(
        cleaned,
        Outcome::Replaced(
            [
                "app",
                "--password",
                "[redacted]",
                "--api-key=[redacted]",
                "--host",
                "db.example",
            ]
            .map(str::to_owned)
            .to_vec()
        )
    );

    let mut builder = crate::capture::builder::CaptureBuilder::new();
    builder.setting(
        "arguments 42",
        &serde_json::to_string(&[
            "app",
            "--password",
            "correct horse battery",
            "--host",
            "db.example",
        ])
        .expect("json args"),
    );
    builder.setting(
        "command 42",
        "app --password correct horse battery --host db.example",
    );
    let data = crate::object::ChunkData {
        capture: builder.finish(),
        chunks: Vec::new(),
        dimensions: Vec::new(),
        sanitizer_counts: Vec::new(),
    };
    let clean = sanitize_chunk(
        data,
        crate::object::TimeUnit::Us,
        &sanitizer(&Builtin::DEFAULT),
    );
    let strings = String::from_utf8_lossy(&clean.data().capture.strings);
    for word in ["correct", "horse", "battery"] {
        assert!(!strings.contains(word), "leaked {word} in {strings:?}");
    }
    assert!(strings.contains("--host db.example"));

    let multiline_and_shell = [
        "sh",
        "-c",
        "curl --token embedded_secret https://example.com",
        "--password=line1\nline2",
        "Bearer",
        "opaque_cross_arg_token_99",
    ]
    .map(str::to_owned);
    let mut tally2 = Tally::default();
    let cleaned2 = sanitizer(&Builtin::DEFAULT).args(&multiline_and_shell, &mut tally2);
    assert_eq!(
        cleaned2,
        Outcome::Replaced(
            [
                "sh",
                "-c",
                "curl --token [redacted] https://example.com",
                "--password=[redacted]",
                "Bearer",
                "[redacted]",
            ]
            .map(str::to_owned)
            .to_vec()
        )
    );

    let mut b2 = crate::capture::builder::CaptureBuilder::new();
    b2.setting(
        "arguments 7",
        &serde_json::to_string(&["curl", "-H", "Authorization:", "Bearer", "split_secret_xyz"])
            .expect("json"),
    );
    b2.setting(
        "command 7",
        "curl -H Authorization: Bearer split_secret_xyz",
    );
    let clean2 = sanitize_chunk(
        crate::object::ChunkData {
            capture: b2.finish(),
            chunks: Vec::new(),
            dimensions: Vec::new(),
            sanitizer_counts: Vec::new(),
        },
        crate::object::TimeUnit::Us,
        &sanitizer(&Builtin::DEFAULT),
    );
    let args_setting = clean2.data().capture.setting("arguments 7").expect("args");
    let cmd_setting = clean2.data().capture.setting("command 7").expect("cmd");
    assert!(!args_setting.contains("split_secret_xyz"), "{args_setting}");
    assert!(!cmd_setting.contains("split_secret_xyz"), "{cmd_setting}");
}

#[test]
fn a_bad_rule_says_what_is_wrong_with_it() {
    let spec = |name: &str, fields: &[&str], pattern: &str| RuleSpec {
        name: name.to_owned(),
        fields: fields.iter().map(|&field| field.to_owned()).collect(),
        pattern: pattern.to_owned(),
        action: Action::Redact,
    };
    assert_eq!(
        Rule::new(&spec(" ", &["zone.text"], "x")).err(),
        Some(RuleError::Unnamed)
    );
    assert_eq!(
        Rule::new(&spec("a", &[], "x")).err(),
        Some(RuleError::NoFields("a".into()))
    );
    assert_eq!(
        Rule::new(&spec("a", &["env"], "x")).err(),
        Some(RuleError::Field("env".into()))
    );
    assert!(matches!(
        Rule::new(&spec("a", &["zone.text"], "(")),
        Err(RuleError::Pattern { .. })
    ));
    assert!(matches!(
        Rule::new(&spec("a", &["zone.text"], r"\w{1000}{1000}")),
        Err(RuleError::Pattern { .. })
    ));
    assert!(Rule::new(&spec("a", &["zone.text"], r"(a+)+$")).is_ok());

    let parsed: RuleSpec = toml_like(
        r#"{"name":"tenant ids in paths","fields":["syscall.path"],"match":"x","action":"drop-record"}"#,
    );
    assert_eq!(parsed.action, Action::DropRecord);
}

fn toml_like(json: &str) -> RuleSpec {
    serde_json::from_str(json).expect("a rule spec")
}

proptest! {
    #[test]
    fn a_planted_secret_never_survives(
        before in "[ -~\n]{0,40}",
        after in "[ -~\n]{0,40}",
        secret in prop_oneof![
            "ghp_[A-Za-z0-9]{36}",
            "AKIA[0-9A-Z]{16}",
            "xoxp-[0-9]{10}-[a-z]{10}",
        ],
    ) {
        let value = format!("{before} {secret} {after}");
        for class in FieldClass::ALL {
            let fields = Builtin::Tokens.fields();
            let cleaned = clean(&Builtin::DEFAULT, class, &value);
            prop_assert_eq!(cleaned.contains(&secret), !fields.contains(class), "{:?} {:?}", class, cleaned);
        }
    }
}

#[test]
fn diagnostic_setting_keys_respect_module_path_policy() {
    use crate::capture::builder::CaptureBuilder;
    use crate::capture::file::{read as read_capture, write as write_capture};

    let secret_path = "/home/alice/secret-checkout/bin/app";
    let mut builder = CaptureBuilder::new();
    builder.setting("program", secret_path);
    builder.setting(
        &format!("debug information {secret_path}"),
        "incomplete: missing locals",
    );
    builder.setting(
        &format!("image cache {secret_path}"),
        &format!("cannot write in {secret_path}.cache"),
    );
    builder.module(secret_path, None, [1; 16], 4096);
    let capture = builder.finish();

    let rule = Rule::new(&RuleSpec {
        name: "hash-module-paths".into(),
        fields: vec!["module.path".into()],
        pattern: "/home/alice/[^ ]+".into(),
        action: Action::Hash,
    })
    .unwrap();
    let sanitizer = Sanitizer::new(&Builtin::DEFAULT, vec![rule], HashKey::new(b"k1"));

    let cleaned = sanitize_capture(capture, &sanitizer);
    let mut bytes = Vec::new();
    write_capture(&cleaned.capture, &mut bytes).unwrap();
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        !text.contains("secret-checkout"),
        "raw module path leaked into capture bytes"
    );

    let reloaded = read_capture(&bytes).unwrap().capture;
    assert!(
        reloaded.settings.iter().any(|s| reloaded
            .string(s.key.get())
            .starts_with("debug information [hash:")),
        "expected hashed debug information key"
    );
    assert!(
        reloaded.settings.iter().any(|s| reloaded
            .string(s.key.get())
            .starts_with("image cache [hash:")),
        "expected hashed image cache key"
    );
}

#[test]
fn repeated_sanitization_is_idempotent_across_actions() {
    use crate::capture::builder::CaptureBuilder;
    use crate::capture::records::MarkerKind;

    let mut builder = CaptureBuilder::new();
    builder.setting("program", "/home/alice/private/app");
    builder.setting(
        "arguments 0",
        r#"["/home/alice/private/app","--token","ghp_1234567890abcdef1234567890abcdef1234","--cust","cust_999888"]"#,
    );
    builder.setting(
        "command 0",
        "/home/alice/private/app --token ghp_1234567890abcdef1234567890abcdef1234 --cust cust_999888",
    );
    builder.module("/home/alice/private/app", None, [2; 16], 4096);
    builder.marker(100, MarkerKind::Message, "héllöwörldabcd");
    builder.marker(200, MarkerKind::Message, "résumé_secret_1234");
    builder.marker(300, MarkerKind::Message, "card=123456789012");
    builder.marker(400, MarkerKind::Message, "reference=1234567890-12");
    let raw = builder.finish();

    let rules = vec![
        Rule::new(&RuleSpec {
            name: "hash-all-module-paths".into(),
            fields: vec!["module.path".into()],
            pattern: ".+".into(),
            action: Action::Hash,
        })
        .unwrap(),
        Rule::new(&RuleSpec {
            name: "hash-cust".into(),
            fields: vec!["process.args".into()],
            pattern: "cust_[0-9]+".into(),
            action: Action::Hash,
        })
        .unwrap(),
        Rule::new(&RuleSpec {
            name: "mask-unicode-marker".into(),
            fields: vec!["marker.text".into()],
            pattern: "héllöwörldabcd|résumé_secret_\\d+|\\*{8,}[A-Za-z0-9]{4}".into(),
            action: Action::Mask,
        })
        .unwrap(),
        Rule::new(&RuleSpec {
            name: "mask-formatted-ref".into(),
            fields: vec!["marker.text".into()],
            pattern: r"\d{10}-\d{2}".into(),
            action: Action::Mask,
        })
        .unwrap(),
        Rule::new(&RuleSpec {
            name: "mask-card-digits".into(),
            fields: vec!["marker.text".into()],
            pattern: r"\d{2,}".into(),
            action: Action::Mask,
        })
        .unwrap(),
    ];
    let sanitizer = Sanitizer::new(&Builtin::DEFAULT, rules, HashKey::new(b"org-key"));

    let pass1 = sanitize_capture(raw, &sanitizer);
    assert!(pass1.total_fired() > 0);
    assert_eq!(
        pass1.capture.string(pass1.capture.markers[0].text.get()),
        "**********abcd"
    );
    assert_eq!(
        pass1.capture.string(pass1.capture.markers[1].text.get()),
        "**************1234"
    );
    assert_eq!(
        pass1.capture.string(pass1.capture.markers[2].text.get()),
        "card=********9012"
    );
    assert_eq!(
        pass1.capture.string(pass1.capture.markers[3].text.get()),
        "reference=*********0-12"
    );
    let pass2 = sanitize_capture(pass1.capture.clone(), &sanitizer);
    assert_eq!(pass2.total_fired(), 0, "second pass should not re-fire");
    assert_eq!(pass1.capture, pass2.capture);

    let mut smuggle_tally = Tally::default();
    assert_eq!(
        sanitizer.text(
            FieldClass::MarkerText,
            "card=********123456789012",
            &mut smuggle_tally,
        ),
        Outcome::Replaced("card=****************9012".into())
    );

    let mut tally = Tally::default();
    let mixed = sanitizer.text(
        FieldClass::ModulePath,
        "/home/alice/[hash:0123456789abcdef]/extra",
        &mut tally,
    );
    match mixed {
        Outcome::Replaced(out) => {
            assert!(!out.contains("/home/alice/"));
            assert!(!out.contains("/extra"));
            assert!(out.contains("[hash:0123456789abcdef]"));
        }
        other => panic!("expected mixed raw+hashed string to still sanitize raw parts: {other:?}"),
    }
}

#[test]
fn ip_builtin_preserves_rust_and_cpp_paths_while_scrubbing_ips() {
    let s = sanitizer(&[Builtin::IpAddresses]);
    for keep in [
        "std::io::Error",
        "foo::bar",
        "crate::module::Type::method",
        "a::b::c",
    ] {
        assert_eq!(
            clean(&[Builtin::IpAddresses], FieldClass::ZoneText, keep),
            keep,
            "must not corrupt {keep}"
        );
    }
    for scrub in [
        "10.0.0.1",
        "192.168.1.254",
        "::1",
        "fe80::1",
        "2001:db8::8a2e:370:7334",
    ] {
        let out = clean(
            &[Builtin::IpAddresses],
            FieldClass::ZoneText,
            &format!("host {scrub} ok"),
        );
        assert!(!out.contains(scrub), "expected {scrub} scrubbed, got {out}");
    }
    let _ = s;
}

proptest! {
    #[test]
    fn prop_sanitizer_args_preserves_arity_with_embedded_nuls_and_is_idempotent(
        args in proptest::collection::vec(
            prop_oneof![
                Just("--password".to_owned()),
                Just("hunter2".to_owned()),
                Just("arg\0with\0nul".to_owned()),
                Just("std::io::Error".to_owned()),
                Just("10.0.0.1".to_owned()),
                Just("https://u:p@host/path".to_owned()),
                "[a-zA-Z0-9_:\\./=\0-]{0,24}",
            ],
            0..12
        )
    ) {
        let s = sanitizer(&Builtin::ALL);
        let mut t1 = Tally::default();
        let after1 = match s.args(&args, &mut t1) {
            Outcome::Kept => args.clone(),
            Outcome::Replaced(v) => {
                prop_assert_eq!(v.len(), args.len());
                v
            }
            Outcome::DropField | Outcome::DropRecord => return Ok(()),
        };
        let mut t2 = Tally::default();
        let after2 = match s.args(&after1, &mut t2) {
            Outcome::Kept => after1.clone(),
            Outcome::Replaced(v) => v,
            Outcome::DropField | Outcome::DropRecord => return Ok(()),
        };
        prop_assert_eq!(after2, after1);
    }
}

#[test]
fn verbatim_resanitize_strips_debug_prefix_and_rejects_duplicate_sections() {
    use crate::capture::builder::CaptureBuilder;
    use crate::object::{
        ChunkData, ChunkRow, DEFAULT_ZSTD_LEVEL, ObjectError, RawSection, TimeUnit, decode_tables,
        encode_tables,
    };

    let mut builder = CaptureBuilder::new();
    builder.setting("debug information /home/alice/secret/app", "loaded");
    builder.setting("image cache /home/alice/secret/app", "hit");
    let data = ChunkData {
        capture: builder.finish(),
        chunks: vec![ChunkRow {
            agent: 1,
            number: 1,
            start: 1_000,
            end: 2_000,
            sampled_ns: 0,
            preset: 0,
            time_unit: TimeUnit::Ns,
            policy: 1,
            rules: String::new(),
        }],
        dimensions: Vec::new(),
        sanitizer_counts: Vec::new(),
    };
    let anchored = Rule::new(&RuleSpec {
        name: "anchored-home".into(),
        fields: vec!["module.path".into()],
        pattern: "^/home/[^/]+/".into(),
        action: Action::Hash,
    })
    .unwrap();
    let policy = Sanitizer::new(&Builtin::DEFAULT, vec![anchored], HashKey::new(b"org"));

    let encoded = encode_tables(&data, TimeUnit::Ns, DEFAULT_ZSTD_LEVEL, 0).expect("encode");
    let raw: Vec<RawSection<'_>> = encoded
        .iter()
        .map(|s| RawSection {
            kind: s.kind,
            rows: s.rows,
            key: s.key,
            checksum: 0,
            bytes: &s.bytes,
        })
        .collect();
    let (resanitized, verbatim) =
        re_sanitize_sections_verbatim(&raw, &policy, DEFAULT_ZSTD_LEVEL).expect("resanitize");
    assert!(
        !verbatim,
        "anchored ModulePath must trigger on debug information prefix"
    );
    let raw_after: Vec<RawSection<'_>> = resanitized
        .iter()
        .map(|s| RawSection {
            kind: s.kind,
            rows: s.rows,
            key: s.key,
            checksum: 0,
            bytes: &s.bytes,
        })
        .collect();
    let decoded = decode_tables(&raw_after).expect("decode");
    assert!(!String::from_utf8_lossy(&decoded.capture.strings).contains("/home/alice/"));

    let mut dup = raw;
    dup.push(dup[0]);
    assert!(matches!(
        re_sanitize_sections_verbatim(&dup, &policy, DEFAULT_ZSTD_LEVEL),
        Err(ObjectError::Layout(_))
    ));
}
