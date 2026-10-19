//! `microscope web` in a terminal, where pressing `o` opens the page.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use expectrl::process::unix::WaitStatus;
use expectrl::session::Session;
use expectrl::{ControlCode, Expect, Regex};
use nix::sys::termios::LocalFlags;

use crate::support::{Scenario, ScratchDir};

const DEADLINE: Duration = Duration::from_secs(10);

/// A `$BROWSER` that records each link it is given, the process that ran
/// it, and that process's parent.
fn recording_browser(directory: &Path) -> PathBuf {
    let browser = directory.join("browser");
    std::fs::write(
        &browser,
        "#!/bin/sh\n\
         opener=$(awk '/^PPid:/ { print $2 }' /proc/$PPID/status)\n\
         echo \"$1 $PPID $opener\" >> \"$(dirname \"$0\")/opened\"\n",
    )
    .expect("write the browser");

    std::fs::set_permissions(&browser, std::fs::Permissions::from_mode(0o755))
        .expect("make the browser executable");

    browser
}

#[test]
fn pressing_o_opens_the_join_link_in_the_browser_and_exiting_restores_the_terminal() {
    let directory = ScratchDir::new("web-open");
    let browser = recording_browser(directory.path());

    // The PTY starts without echo; a person's terminal echoes lines.
    let mut command = Command::new("sh");
    command
        .args(["-c", r#"stty echo icanon && exec "$0" "$@""#])
        .arg(env!("CARGO_BIN_EXE_microscope"))
        .args(["web", "--port", "0", "--run"])
        .arg(Scenario::fixture("spin"))
        .env("BROWSER", &browser)
        .env("MICROSCOPE_FLIGHT_RECORDING", "");
    let mut session = Session::spawn(command).expect("spawn microscope web in a PTY");
    session.set_expect_timeout(Some(DEADLINE));
    let server = session.get_process().pid();

    let found = session
        .expect(Regex(r"open (http\S+)"))
        .expect("the join link");
    let link = String::from_utf8_lossy(found.get(1).expect("a link")).into_owned();
    session.expect("press o").expect("the hint");
    assert!(
        !session.get_process().get_echo().expect("terminal flags"),
        "keys act without echoing or waiting for Enter"
    );

    // The program is running, so the debugger is waiting on children.
    session.send("o").expect("press o");
    let opened = directory.path().join("opened");
    let deadline = Instant::now() + DEADLINE;

    let record = loop {
        // A whole line, once the browser has finished writing it.
        if let Ok(text) = std::fs::read_to_string(&opened)
            && let Some((first, _)) = text.split_once('\n')
        {
            break first.to_owned();
        }

        assert!(Instant::now() < deadline, "the browser never opened");
        std::thread::sleep(Duration::from_millis(10));
    };

    let [url, helper, helper_parent] = record.split(' ').collect::<Vec<_>>()[..] else {
        panic!("a link and two pids: {record}");
    };

    assert_eq!(url, link);

    // Whatever runs the browser is no child of the debugger, whose waiter
    // would otherwise reap it as one of the program's threads.
    assert_ne!(helper_parent, server.to_string());

    // The terminal's settings outlive microscope only while something has it
    // open, as the shell that ran microscope would.
    let terminal = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(std::fs::read_link(format!("/proc/{server}/fd/0")).expect("microscope's terminal"))
        .expect("open microscope's terminal");
    session.send(ControlCode::EndOfText).expect("press Ctrl+C");
    assert_eq!(
        session.get_process().wait().expect("exit status"),
        WaitStatus::Exited(server, 0)
    );
    let flags = nix::sys::termios::tcgetattr(&terminal).expect("terminal flags");
    assert!(
        flags
            .local_flags
            .contains(LocalFlags::ECHO | LocalFlags::ICANON),
        "the terminal echoes lines again"
    );

    // The helper leaves with microscope.
    crate::web::wait_gone(helper.parse().expect("a pid"));
}
