mod support;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use expectrl::session::{OsSession, Session};
use expectrl::{ControlCode, Eof, Expect};

/// The harness's deadline for anything a test waits to observe.
const TIMEOUT: Duration = Duration::from_secs(5);

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("build/test-programs")
        .join(name)
}

/// A microscope command for a terminal that supports color, with history kept
/// under `state` and no color settings in the environment. It runs in
/// `state`, whose project an interactive session keeps its breakpoints in.
fn command(executable: &Path, state: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_microscope"));
    command
        .arg(executable)
        .current_dir(state)
        .env("XDG_STATE_HOME", state)
        .env("MICROSCOPE_CONFIG", "")
        .env("TERM", "xterm-256color")
        // Output taller than the terminal goes through the pager.
        .env("PAGER", "cat")
        .env_remove("NO_COLOR")
        .env_remove("CLICOLOR")
        .env_remove("CLICOLOR_FORCE");
    command
}

fn spawn(command: Command) -> OsSession {
    let mut session = Session::spawn(command).expect("spawn microscope in a PTY");
    session.set_expect_timeout(Some(TIMEOUT));
    session
}

/// Starts an uncolored REPL and waits for its first prompt.
fn repl(executable: &Path, state: &Path) -> OsSession {
    let mut command = command(executable, state);
    command.args(["--color", "never"]);
    let mut session = spawn(command);
    session.expect("(microscope) ").expect("initial prompt");
    session
}

fn contains_sgr(bytes: &[u8]) -> bool {
    let mut offset = 0;
    while let Some(start) = bytes[offset..].windows(2).position(|pair| pair == b"\x1b[") {
        let sequence = &bytes[offset + start + 2..];
        if let Some(final_byte) = sequence.iter().find(|&&byte| (0x40..=0x7e).contains(&byte))
            && *final_byte == b'm'
        {
            return true;
        }

        offset += start + 2;
    }

    false
}

#[test]
fn interactive_named_prompt_preserves_rustyline_cursor_width() {
    let state = support::ScratchDir::new("repl-color");
    let mut session = spawn(command(&fixture("basic"), state.path()));
    session
        .expect("\x1b[2m(microscope) \x1b[0m")
        .expect("dimmed prompt");

    session.send("quit").expect("type command");
    session.send([0x01]).expect("send Ctrl-A");
    session.send(b"help ").expect("insert at prompt start");
    session.send_line("").expect("execute edited command");
    session.expect("aliases").expect("edited command output");
    session.expect("(microscope) ").expect("redrawn prompt");
    session.send_line("quit").expect("quit repl");
    session.expect(Eof).expect("repl exited");
}

#[test]
fn interactive_prompt_honors_no_color() {
    let state = support::ScratchDir::new("repl-no-color");
    let mut command = command(&fixture("basic"), state.path());
    command.env("NO_COLOR", "1");
    let mut session = spawn(command);
    let prompt = session.expect("(microscope) ").expect("plain prompt");
    assert!(
        !contains_sgr(prompt.as_bytes()),
        "NO_COLOR prompt contained color: {:?}",
        prompt.as_bytes()
    );

    session.send_line("quit").expect("quit repl");
    session.expect(Eof).expect("repl exited");
}

#[test]
fn interactive_control_c_cancels_the_line_and_control_l_redraws() {
    let state = support::ScratchDir::new("repl-editing");
    let mut session = repl(&fixture("basic"), state.path());

    session.send("invalid-command").expect("type invalid line");
    session.send(ControlCode::EndOfText).expect("send Ctrl-C");
    session
        .expect("(microscope) ")
        .expect("Ctrl-C redrew prompt");

    session.send(ControlCode::FormFeed).expect("send Ctrl-L");
    session
        .expect("(microscope) ")
        .expect("Ctrl-L redrew prompt");

    session
        .send(ControlCode::EndOfTransmission)
        .expect("send Ctrl-D");
    session.expect(Eof).expect("repl exited");
}

#[test]
fn interactive_errors_omit_repl_context() {
    let state = support::ScratchDir::new("repl-errors");
    let mut session = repl(&fixture("basic"), state.path());

    session
        .send_line("break asdf")
        .expect("send invalid breakpoint");
    session
        .expect("error: no function named 'asdf' was found")
        .expect("concise interactive error");
    session.expect("(microscope) ").expect("prompt after error");

    session.send_line("quit").expect("quit repl");
    session.expect(Eof).expect("repl exited");
}

#[test]
fn interactive_clear_command_and_cls_alias_clear_the_terminal() {
    let state = support::ScratchDir::new("repl-clear");
    let mut session = repl(&fixture("basic"), state.path());

    for command in ["clear", "cls"] {
        session.send_line(command).expect("send clear command");
        session
            .expect("\x1b[2J\x1b[H")
            .expect("terminal clear sequence");
        session.expect("(microscope) ").expect("prompt after clear");
    }

    session.send_line("quit").expect("quit repl");
    session.expect(Eof).expect("repl exited");
}

#[test]
fn interactive_empty_lines_repeat_the_last_session_command() {
    let state = support::ScratchDir::new("repl-repeat");
    let mut session = repl(&fixture("basic"), state.path());

    session.send_line("").expect("send initial empty line");
    session
        .expect("(microscope) ")
        .expect("empty line before any command is a no-op");

    session.send_line("break main").expect("set breakpoint");
    session
        .expect("breakpoint 1 set")
        .expect("breakpoint reply");
    session
        .expect("(microscope) ")
        .expect("prompt after breakpoint");

    session.send_line("run").expect("run inferior");
    session.expect("=> 10 | ").expect("main stop");
    session.expect("(microscope) ").expect("prompt after run");

    session.send_line("next").expect("send next");
    session.expect("=> 11 | ").expect("first source step");
    session
        .expect("(microscope) ")
        .expect("prompt after first source step");

    for line in [12, 13] {
        session.send_line("").expect("repeat next");
        session
            .expect(format!("=> {line} | ").as_str())
            .expect("repeated source step");
        session
            .expect("(microscope) ")
            .expect("prompt after repeated source step");
    }

    quit_killing(session);
}

#[test]
fn interactive_history_persists_across_sessions() {
    let state = support::ScratchDir::new("repl-history");
    {
        let mut session = repl(&fixture("basic"), state.path());
        session.send_line("help quit").expect("record command");
        session
            .expect("(microscope) ")
            .expect("prompt after command");
        session.send_line("quit").expect("quit first repl");
        session.expect(Eof).expect("first repl exited");
    }

    let mut session = repl(&fixture("basic"), state.path());
    session.send(b"\x1b[A").expect("send up arrow");
    session
        .send(b"\x1b[A")
        .expect("skip persisted quit command");
    session.send_line("").expect("execute persisted command");
    session
        .expect("aliases: q")
        .expect("history was loaded in second session");
    session.send_line("quit").expect("quit second repl");
    session.expect(Eof).expect("second repl exited");
}

/// A project in `scratch` with its own copy of `hit-counts.c`, which a
/// session maps the program's sources to so that a test may edit it.
fn project_with_sources(scratch: &Path) -> PathBuf {
    let project = scratch.join("project");
    std::fs::create_dir_all(project.join("src")).expect("create the project");
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/c/hit-counts.c"),
        project.join("src/hit-counts.c"),
    )
    .expect("copy the source");
    project
}

/// An interactive session's command in `project`, which keeps its
/// breakpoints there, with the program's sources mapped to the project's.
fn kept_command(project: &Path, state: &Path) -> Command {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut command = command(&fixture("hit-counts-gcc-o0"), state);
    command
        .current_dir(project)
        .env(
            "MICROSCOPE_CONFIG",
            manifest.join("tests/support/settings/ascii.toml"),
        )
        .args(["--color", "never", "--source-map"])
        .arg(manifest.join("tests/fixtures/c"))
        .arg(project.join("src"));
    command
}

/// Starts a session in `project`, waiting for what it says before its
/// first prompt.
fn kept_session(project: &Path, state: &Path, said: &[&str]) -> OsSession {
    let mut session = spawn(kept_command(project, state));
    for text in said {
        session
            .expect(*text)
            .unwrap_or_else(|error| panic!("expected {text:?}: {error}"));
    }

    session.expect("(microscope) ").expect("initial prompt");
    session
}

fn run(session: &mut OsSession, line: &str, expected: &str) {
    session.send_line(line).expect("type a command");
    session
        .expect(expected)
        .unwrap_or_else(|error| panic!("{line}: expected {expected:?}: {error}"));
    session.expect("(microscope) ").expect("next prompt");
}

fn quit(mut session: OsSession) {
    session.send_line("quit").expect("quit");
    session.expect(Eof).expect("the session ended");
}

/// Quits a session whose program is alive, which asks first.
fn quit_killing(mut session: OsSession) {
    session.send_line("quit").expect("quit");
    session
        .expect("kill it and quit? (y or n) ")
        .expect("the question");
    session.send_line("y").expect("answer yes");
    session.expect(Eof).expect("the session ended");
}

#[test]
fn an_interactive_session_keeps_its_breakpoints_for_the_next_one() {
    let scratch = support::ScratchDir::new("kept-breakpoints");
    let project = project_with_sources(scratch.path());
    let state = project.join(".microscope/state");
    let saved = state.join("breakpoints.toml");

    let mut session = kept_session(&project, scratch.path(), &[]);
    run(
        &mut session,
        "break hit-counts.c:11 if call > 2 hits >=2",
        "tests/fixtures/c/hit-counts.c:11, stops at hits >=2 where call > 2",
    );
    run(&mut session, "disable 1", "disabled breakpoint 1");
    run(
        &mut session,
        "display/x last_call",
        "display 1: /x last_call",
    );

    // Temporary breakpoints belong to one stop, and are not kept.
    run(&mut session, "tbreak caller", "temporary breakpoint 2 set");
    quit(session);

    let text = std::fs::read_to_string(&saved).expect("the breakpoints were saved");
    assert_eq!(
        text,
        "version = 1\n\n[[breakpoint]]\nlocation = \"hit-counts.c:11\"\n\
         condition = \"call > 2\"\nhits = \">=2\"\nenabled = false\n\
         line-text = \"    last_call = call;\"\n\n\
         [[display]]\nexpression = \"last_call\"\nformat = \"x\"\n",
        "{text}"
    );

    assert_eq!(
        std::fs::read_to_string(state.join(".gitignore")).expect("state is ignored"),
        "*\n"
    );

    // The next session restores it, disabled and conditional as it was,
    // and the display.
    let mut session = kept_session(
        &project,
        scratch.path(),
        &["restored 1 breakpoint and 1 display"],
    );

    run(&mut session, "display", "1: /x last_call");
    run(
        &mut session,
        "breakpoints",
        "/tests/fixtures/c/hit-counts.c:11  hits >=2  if call > 2",
    );
    quit(session);

    // A line that reads differently since is restored where it was, and
    // said to have changed.
    let source = project.join("src/hit-counts.c");
    let edited = std::fs::read_to_string(&source)
        .expect("read the source")
        .replace("    last_call = call;", "    last_call = call + 0;");
    std::fs::write(&source, edited).expect("edit the source");

    quit(kept_session(
        &project,
        scratch.path(),
        &["hit-counts.c:11 changed since it was saved"],
    ));

    // A file that does not parse is reported and never overwritten.
    std::fs::write(&saved, "version = 1\n[[breakpoint]\n").expect("break the file");

    let mut session = kept_session(
        &project,
        scratch.path(),
        &["none are saved until it is fixed or deleted"],
    );
    run(&mut session, "break counted", "breakpoint 1 set");
    quit(session);

    assert_eq!(
        std::fs::read_to_string(&saved).expect("the file is kept"),
        "version = 1\n[[breakpoint]\n"
    );

    // Batch sessions neither restore nor save.
    std::fs::remove_file(&saved).expect("remove the file");

    let output = kept_command(&project, scratch.path())
        .args(["--batch", "-e", "break counted"])
        .output()
        .expect("run a batch session");
    assert!(output.status.success(), "{output:?}");
    assert!(!saved.exists());
}

#[test]
fn interactive_pp_lays_values_out_to_the_terminal_width() {
    let state = support::ScratchDir::new("repl-pp-width");
    let mut session = repl(&fixture("records-c-gcc-o0"), state.path());
    run(&mut session, "break inspect_records", "breakpoint 1 set");
    run(&mut session, "run", "stopped at breakpoint 1");

    session
        .get_process_mut()
        .set_window_size(40, 24)
        .expect("narrow the terminal");

    run(
        &mut session,
        "pp record->inner",
        "(inner_record) record->inner = {\r\n  signed_value = -7,\r\n  unsigned_value = 9,\r\n}\r\n",
    );

    // The width is measured again for each line.
    session
        .get_process_mut()
        .set_window_size(100, 24)
        .expect("widen the terminal");

    run(
        &mut session,
        "pp record->inner",
        "(inner_record) record->inner = {signed_value = -7, unsigned_value = 9}\r\n",
    );
    quit_killing(session);
}

#[test]
fn interactive_quit_asks_before_killing_a_launched_program() {
    let state = support::ScratchDir::new("repl-confirm-quit");
    let mut session = repl(&fixture("basic"), state.path());
    run(&mut session, "break main", "breakpoint 1 set");
    run(&mut session, "run", "stopped at breakpoint 1");

    session.send_line("quit").expect("type quit");
    session
        .expect("kill it and quit? (y or n) ")
        .expect("the question");
    session.send_line("n").expect("answer no");
    session
        .expect("(microscope) ")
        .expect("the session goes on");

    run(&mut session, "where", "main at ");
    session.send_line("q").expect("type quit's alias");
    session
        .expect("kill it and quit? (y or n) ")
        .expect("the question again");
    session.send_line("y").expect("answer yes");
    session.expect(Eof).expect("the session ended");
}

#[test]
fn interactive_output_taller_than_the_terminal_goes_through_the_pager() {
    let state = support::ScratchDir::new("repl-pager");
    let settings = state.path().join("config.toml");
    std::fs::write(&settings, "[ui]\npager = \"sed 's/^/paged: /'\"\n")
        .expect("write the settings");

    let mut command = command(&fixture("basic"), state.path());
    command
        .env("MICROSCOPE_CONFIG", &settings)
        .args(["--color", "never"]);

    let mut session = spawn(command);
    session.expect("(microscope) ").expect("initial prompt");

    session
        .get_process_mut()
        .set_window_size(80, 10)
        .expect("shorten the terminal");

    run(&mut session, "help", "paged:   quit");

    // Output that fits is printed as it is.
    run(&mut session, "breakpoints", "\r\nno breakpoints\r\n");
    quit(session);
}

#[test]
fn interactive_tab_completes_commands_locations_and_members() {
    let state = support::ScratchDir::new("repl-complete");
    let mut session = repl(&fixture("records-c-gcc-o0"), state.path());

    // A command, then a function, each the only candidate.
    session.send("tbrea").expect("type a prefix");
    session.send("\t").expect("complete the command");
    session.send("inspect_rec").expect("type a function prefix");
    session.send("\t").expect("complete the function");
    session.send_line("").expect("run the completed line");

    session
        .expect("temporary breakpoint 1 set at inspect_records")
        .expect("the completed breakpoint");
    session.expect("(microscope) ").expect("prompt");

    run(&mut session, "run", "stopped at breakpoint 1");

    // A member of what a pointer points to, which the debugger reads.
    session.send("p record->in").expect("type a member prefix");
    session.send("\t").expect("complete the member");
    session.send_line("").expect("run the completed line");

    session
        .expect("(inner_record) record->inner = {signed_value = -7, unsigned_value = 9}")
        .expect("the completed member");
    session.expect("(microscope) ").expect("prompt");
    quit_killing(session);
}
