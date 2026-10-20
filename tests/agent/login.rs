//! `microscope agent login`: saving the device token.

use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::process::{Output, Stdio};

use crate::{Agent, Scratch};

fn login(scratch: &Scratch, args: &[&str], stdin: &str) -> Output {
    let mut child = scratch
        .microscope()
        .args(["agent", "login"])
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn login_saves_a_private_token_and_the_server() {
    let scratch = Scratch::new("login");
    let output = login(
        &scratch,
        &["--server", "http://localhost:8080/"],
        "msd_abc\n",
    );
    assert!(output.status.success(), "{output:?}");

    let token = scratch.config().join("agent-token");
    assert_eq!(std::fs::read_to_string(&token).unwrap(), "msd_abc\n");
    let mode = std::fs::metadata(&token).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);

    let settings = std::fs::read_to_string(scratch.config().join("agent.toml")).unwrap();
    assert!(
        settings.contains(r#"server = "http://localhost:8080/""#),
        "{settings}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("microscope agent"), "{stderr}");
}

#[test]
fn login_keeps_the_other_settings() {
    let scratch = Scratch::new("login-keep");
    scratch.log_in("server = \"http://old/\"\nallow-remote = true\n");
    let output = login(&scratch, &["--server", "https://new.example/"], "msd_new\n");
    assert!(output.status.success(), "{output:?}");

    let settings = std::fs::read_to_string(scratch.config().join("agent.toml")).unwrap();
    assert!(settings.contains("allow-remote = true"), "{settings}");
    assert!(settings.contains("https://new.example/"), "{settings}");
}

#[test]
fn login_reuses_the_saved_server() {
    let scratch = Scratch::new("login-reuse");
    scratch.log_in("server = \"http://saved/\"\n");
    let output = login(&scratch, &[], "msd_again\n");
    assert!(output.status.success(), "{output:?}");
    let token = std::fs::read_to_string(scratch.config().join("agent-token")).unwrap();
    assert_eq!(token, "msd_again\n");
}

#[test]
fn login_needs_a_server() {
    let scratch = Scratch::new("login-no-server");
    let output = login(&scratch, &[], "msd_abc\n");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--server"));
    assert!(!scratch.config().join("agent-token").exists());
}

#[test]
fn login_refuses_what_is_not_a_token_or_a_server() {
    let scratch = Scratch::new("login-bad");
    for (args, stdin) in [
        (["--server", "http://localhost/"], "\n"),
        (["--server", "http://localhost/"], "two words\n"),
        (["--server", "localhost:8080"], "msd_abc\n"),
        (["--server", "ftp://localhost/"], "msd_abc\n"),
    ] {
        let output = login(&scratch, &args, stdin);
        assert!(!output.status.success(), "{args:?} {stdin:?}");
        assert!(!scratch.config().join("agent-token").exists());
    }
}

#[test]
fn the_agent_refuses_a_token_others_can_read() {
    let scratch = Scratch::new("login-shared");
    scratch.log_in("server = \"http://localhost:1/\"\n");
    let token = scratch.config().join("agent-token");
    std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o644)).unwrap();

    let (status, stderr) = Agent::start(&scratch, &[]).exit();
    assert!(!status.success());
    assert!(stderr.contains("chmod 600"), "{stderr}");
}

#[test]
fn the_agent_needs_a_login() {
    let scratch = Scratch::new("login-none");
    let (status, stderr) = Agent::start(&scratch, &[]).exit();
    assert!(!status.success());
    assert!(stderr.contains("microscope agent login"), "{stderr}");

    scratch.log_in("server = \"http://localhost:1/\"\n");
    std::fs::remove_file(scratch.config().join("agent-token")).unwrap();
    let (status, stderr) = Agent::start(&scratch, &[]).exit();
    assert!(!status.success());
    assert!(stderr.contains("no device token"), "{stderr}");
}
