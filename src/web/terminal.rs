//! Keys pressed in the terminal running `microscope web`: `o` opens the page in
//! a browser.
//!
//! The browser must be no child of this process. The debugger's waiter
//! reaps every child, so it would take a browser's exit for the exit of a
//! thread the program had not announced yet. A helper started before any
//! debugger exists, and detached from this process, runs the browser instead.

use std::ffi::OsString;
use std::io::{self, BufRead as _, IsTerminal as _, PipeWriter, Read as _, Write as _};
use std::process::{Command, ExitCode, Stdio};

use nix::sys::termios::{self, LocalFlags, SetArg, SpecialCharacterIndices, Termios};

/// The hidden command that runs the helper, outside the async runtime.
pub const HELPER: &str = "web-opener";

/// Whether a person at a terminal is running `microscope web`.
pub fn interactive() -> bool {
    io::stdin().is_terminal() && io::stdout().is_terminal()
}

/// Starts the helper, which must happen before any debugger exists.
pub fn start_opener() -> io::Result<PipeWriter> {
    let (links, writer) = io::pipe()?;

    // The first helper starts the one that stays and exits at once, so the
    // one that stays is no child of this process.
    let status = Command::new(std::env::current_exe()?)
        .args([HELPER, "--detach"])
        .stdin(links)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if !status.success() {
        return Err(io::Error::other(format!(
            "the browser helper failed: {status}"
        )));
    }

    Ok(writer)
}

/// Runs the helper: with `--detach`, starts the helper that stays; otherwise
/// opens each link read from stdin until this process's end of it closes.
pub fn run_helper(arguments: impl IntoIterator<Item = OsString>) -> ExitCode {
    if arguments.into_iter().any(|argument| argument == "--detach") {
        let started = std::env::current_exe().and_then(|program| {
            Command::new(program)
                .arg(HELPER)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
        });
        return if started.is_ok() {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        };
    }

    // Its own session, so Ctrl+C in the terminal reaches neither the helper
    // nor a browser it starts.
    let _ = nix::unistd::setsid();

    for link in io::stdin().lock().lines().map_while(Result::ok) {
        // `$BROWSER` names the browser, as many tools agree; otherwise the
        // desktop's choice.
        let browser = std::env::var_os("BROWSER")
            .filter(|browser| !browser.is_empty())
            .unwrap_or_else(|| "xdg-open".into());

        if let Ok(mut child) = Command::new(browser)
            .arg(link)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            // A browser may run until it is closed.
            std::thread::spawn(move || child.wait());
        }
    }

    ExitCode::SUCCESS
}

/// Lets the terminal's keys through one at a time, until dropped.
pub struct Keys {
    original: Termios,
}

impl Keys {
    /// Opens `link` through `opener` whenever `o` is pressed. Signals such
    /// as Ctrl+C still work.
    pub fn listen(link: String, mut opener: PipeWriter) -> io::Result<Self> {
        let original = termios::tcgetattr(io::stdin())?;
        let mut keys = original.clone();
        keys.local_flags
            .remove(LocalFlags::ICANON | LocalFlags::ECHO);
        keys.control_chars[SpecialCharacterIndices::VMIN as usize] = 1;
        keys.control_chars[SpecialCharacterIndices::VTIME as usize] = 0;
        termios::tcsetattr(io::stdin(), SetArg::TCSANOW, &keys)?;

        std::thread::spawn(move || {
            let mut key = [0];

            while io::stdin().lock().read_exact(&mut key).is_ok() {
                if matches!(key[0], b'o' | b'O')
                    && opener.write_all(format!("{link}\n").as_bytes()).is_err()
                {
                    eprintln!("microscope web: cannot open a browser; open the link above");
                    return;
                }
            }
        });

        Ok(Self { original })
    }
}

impl Drop for Keys {
    fn drop(&mut self) {
        let _ = termios::tcsetattr(io::stdin(), SetArg::TCSANOW, &self.original);
    }
}
