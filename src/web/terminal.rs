use std::ffi::OsString;
use std::io::{self, BufRead as _, IsTerminal as _, PipeWriter, Read as _, Write as _};
use std::process::{Command, ExitCode, Stdio};

use nix::sys::termios::{self, LocalFlags, SetArg, SpecialCharacterIndices, Termios};

pub const HELPER: &str = "web-opener";

pub fn interactive() -> bool {
    io::stdin().is_terminal() && io::stdout().is_terminal()
}

pub fn start_opener() -> io::Result<PipeWriter> {
    let (links, writer) = io::pipe()?;

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

    let _ = nix::unistd::setsid();

    for link in io::stdin().lock().lines().map_while(Result::ok) {
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
            std::thread::spawn(move || child.wait());
        }
    }

    ExitCode::SUCCESS
}

pub struct Keys {
    original: Termios,
}

impl Keys {
    pub fn listen(link: String, mut opener: PipeWriter) -> io::Result<Self> {
        let original = termios::tcgetattr(io::stdin())?;
        let mut keys = original.clone();
        keys.local_flags
            .remove(LocalFlags::ICANON | LocalFlags::ECHO);
        for (index, value) in [
            (SpecialCharacterIndices::VMIN, 1),
            (SpecialCharacterIndices::VTIME, 0),
        ] {
            *keys
                .control_chars
                .get_mut(index as usize)
                .ok_or_else(|| io::Error::other(format!("the terminal has no {index:?}")))? = value;
        }
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
