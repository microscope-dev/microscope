use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Path, PathBuf};

use anstyle::{Ansi256Color, AnsiColor, Color, RgbColor, Style};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};

/// User-selected color behavior for terminal output.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ColorChoice {
    /// Detect color support from the output stream and environment.
    #[default]
    Auto,

    /// Emit ANSI color even when output is redirected.
    Always,

    /// Never emit ANSI color.
    Never,
}

impl ColorChoice {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Always => "always",
            Self::Never => "never",
        }
    }
}

impl From<ColorChoice> for clap::ColorChoice {
    fn from(choice: ColorChoice) -> Self {
        match choice {
            ColorChoice::Auto => Self::Auto,
            ColorChoice::Always => Self::Always,
            ColorChoice::Never => Self::Never,
        }
    }
}

#[derive(Debug)]
pub struct ColorEnvironment {
    term: Option<OsString>,
    no_color: bool,
    clicolor: Option<bool>,
    clicolor_force: bool,
}

impl ColorEnvironment {
    pub fn current() -> Self {
        Self {
            term: std::env::var_os("TERM"),
            no_color: anstyle_query::no_color(),
            clicolor: anstyle_query::clicolor(),
            clicolor_force: anstyle_query::clicolor_force(),
        }
    }

    /// The choice the environment makes, which outranks every settings
    /// file, and the variable that makes it.
    pub const fn choice(&self) -> Option<(ColorChoice, &'static str)> {
        if self.no_color {
            Some((ColorChoice::Never, "NO_COLOR"))
        } else if self.clicolor_force {
            Some((ColorChoice::Always, "CLICOLOR_FORCE"))
        } else if matches!(self.clicolor, Some(false)) {
            Some((ColorChoice::Never, "CLICOLOR"))
        } else {
            None
        }
    }
}

/// Decides whether to color a stream: an explicit choice wins, then
/// `NO_COLOR`, `CLICOLOR_FORCE`, `CLICOLOR`, batch mode, and the terminal.
pub fn color_enabled(
    choice: ColorChoice,
    environment: &ColorEnvironment,
    is_terminal: bool,
    batch: bool,
) -> bool {
    match choice {
        ColorChoice::Always => true,
        ColorChoice::Never => false,
        ColorChoice::Auto => {
            if environment.no_color {
                return false;
            }

            if environment.clicolor_force {
                return true;
            }

            if environment.clicolor == Some(false) || batch || !is_terminal {
                return false;
            }

            environment.clicolor == Some(true)
                || environment.term.as_deref().is_some_and(term_supports_color)
        }
    }
}

fn term_supports_color(term: &OsStr) -> bool {
    !term.is_empty() && term != "dumb"
}

/// Determines whether cursor-control sequences are safe for the output stream.
pub fn terminal_control_enabled(environment: &ColorEnvironment, is_terminal: bool) -> bool {
    is_terminal && environment.term.as_deref().is_some_and(term_supports_color)
}

/// Declares the presentation roles, the `[theme]` key that styles each, and
/// the struct of those keys.
macro_rules! roles {
    ($($role:ident $field:ident $key:literal: $about:literal),* $(,)?) => {
        /// Semantic presentation roles shared by all terminal formatters.
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum Role {
            $(#[doc = $about] $role),*
        }

        impl Role {
            /// Every role, in declaration order.
            pub const ALL: &[Self] = &[$(Self::$role),*];
        }

        /// Styles a settings file gives single roles, over the theme's.
        #[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
        #[serde(default, deny_unknown_fields)]
        pub struct ThemeOverrides {
            $(
                #[doc = $about]
                #[serde(rename = $key, skip_serializing_if = "Option::is_none")]
                pub $field: Option<StyleSpec>,
            )*
        }

        impl ThemeOverrides {
            const fn get(&self, role: Role) -> Option<&StyleSpec> {
                match role {
                    $(Role::$role => self.$field.as_ref()),*
                }
            }
        }
    };
}

roles! {
    Prompt prompt "prompt": "The REPL's prompt.",
    Command command "command": "Command names in help, and mnemonics.",
    Alias alias "alias": "Command aliases in help.",
    Muted muted "muted": "Secondary text.",
    Name name "name": "Names of variables, functions, and symbols.",
    Type r#type "type": "Type names, and registers in disassembly.",
    Value value "value": "Values.",
    Metadata metadata "metadata": "Ids, addresses, and paths.",
    Current current "current": "The current line and the stop.",
    Success success "success": "Confirmations.",
    Warning warning "warning": "Warnings.",
    Error error "error": "Errors.",
    Changed changed "changed": "Values that changed since the last stop.",
    Keyword keyword "keyword": "Keywords in highlighted source.",
    String string "string": "String and character literals in highlighted source.",
    Comment comment "comment": "Comments in highlighted source.",
    Number number "number": "Numbers in highlighted source.",
}

/// The built-in palette a theme starts from.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ThemeName {
    /// Bright colors, for dark backgrounds.
    #[default]
    Default,

    /// Darker colors, for light backgrounds.
    Light,
}

/// One style for each role.
#[derive(Clone, Debug)]
pub struct Palette([Style; Role::ALL.len()]);

const fn foreground(color: AnsiColor) -> Style {
    Style::new().fg_color(Some(Color::Ansi(color)))
}

impl Palette {
    const DEFAULT: Self = Self([
        Style::new().dimmed(),
        foreground(AnsiColor::BrightBlue).bold(),
        foreground(AnsiColor::BrightBlue),
        Style::new().dimmed(),
        foreground(AnsiColor::BrightCyan),
        foreground(AnsiColor::BrightMagenta),
        foreground(AnsiColor::BrightGreen),
        foreground(AnsiColor::Cyan),
        foreground(AnsiColor::BrightYellow).bold(),
        foreground(AnsiColor::BrightGreen),
        foreground(AnsiColor::BrightYellow),
        foreground(AnsiColor::BrightRed).bold(),
        foreground(AnsiColor::Yellow).bold(),
        foreground(AnsiColor::Magenta),
        foreground(AnsiColor::Green),
        Style::new().dimmed().italic(),
        foreground(AnsiColor::Cyan),
    ]);

    const LIGHT: Self = Self([
        Style::new().dimmed(),
        foreground(AnsiColor::Blue).bold(),
        foreground(AnsiColor::Blue),
        Style::new().dimmed(),
        foreground(AnsiColor::Blue),
        foreground(AnsiColor::Magenta),
        foreground(AnsiColor::Green),
        foreground(AnsiColor::Cyan),
        foreground(AnsiColor::Magenta).bold(),
        foreground(AnsiColor::Green),
        foreground(AnsiColor::Yellow),
        foreground(AnsiColor::Red).bold(),
        foreground(AnsiColor::Red).bold(),
        foreground(AnsiColor::Blue),
        foreground(AnsiColor::Green),
        Style::new().dimmed().italic(),
        foreground(AnsiColor::Magenta),
    ]);

    /// A theme's palette with single roles restyled.
    pub fn new(theme: ThemeName, overrides: &ThemeOverrides) -> Self {
        let mut palette = match theme {
            ThemeName::Default => Self::DEFAULT,
            ThemeName::Light => Self::LIGHT,
        };

        for (index, role) in Role::ALL.iter().enumerate() {
            if let Some(spec) = overrides.get(*role) {
                palette.0[index] = spec.style;
            }
        }

        palette
    }

    const fn style(&self, role: Role) -> Style {
        self.0[role as usize]
    }
}

/// How source paths are shown.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PathStyle {
    /// Relative to the project root when inside it, else absolute.
    #[default]
    Relative,

    /// As the debug information records them.
    Absolute,

    /// The file name alone.
    Name,
}

/// Everything about presentation besides whether a stream is colored.
#[derive(Debug)]
pub struct Look {
    pub palette: Palette,
    pub paths: PathStyle,

    /// The project root relative paths are shown from.
    pub root: Option<PathBuf>,

    /// Whether output may use symbols such as `●` beyond ASCII.
    pub unicode: bool,

    /// Whether `file:line` locations are OSC 8 links to their files.
    pub hyperlinks: bool,
}

/// The look of clients without settings: absolute paths and ASCII.
static DEFAULT_LOOK: Look = Look {
    palette: Palette::DEFAULT,
    paths: PathStyle::Absolute,
    root: None,
    unicode: false,
    hyperlinks: false,
};

#[derive(Clone, Copy, Debug)]
pub struct Renderer {
    color: bool,
    look: &'static Look,

    /// Whether values are drawn as having changed.
    changed: bool,
}

impl Renderer {
    pub const fn new(color: bool) -> Self {
        Self::with_look(color, &DEFAULT_LOOK)
    }

    pub const fn with_look(color: bool, look: &'static Look) -> Self {
        Self {
            color,
            look,
            changed: false,
        }
    }

    /// This renderer drawing values in the `changed` role.
    pub const fn changed(self) -> Self {
        Self {
            changed: true,
            ..self
        }
    }

    /// Whether output may use symbols beyond ASCII.
    pub const fn unicode(self) -> bool {
        self.look.unicode
    }

    pub const fn is_colored(self) -> bool {
        self.color
    }

    pub fn paint<T>(self, role: Role, value: T) -> Painted<T> {
        let role = if self.changed && role == Role::Value {
            Role::Changed
        } else {
            role
        };

        Painted {
            style: self.color.then(|| self.look.palette.style(role)),
            value,
        }
    }

    /// Shows a source path as `[ui] paths` asks.
    pub fn path(self, path: &Path) -> String {
        match self.look.paths {
            PathStyle::Absolute => path.display().to_string(),
            PathStyle::Name => path.file_name().map_or_else(
                || path.display().to_string(),
                |name| name.display().to_string(),
            ),
            // A root of `/` holds every path, so paths stay absolute there.
            PathStyle::Relative => self
                .look
                .root
                .as_deref()
                .filter(|root| root.parent().is_some())
                .and_then(|root| path.strip_prefix(root).ok())
                .filter(|relative| !relative.as_os_str().is_empty())
                .unwrap_or(path)
                .display()
                .to_string(),
        }
    }

    /// Shows `path:line`, as a link to the file when hyperlinks are on.
    pub fn location(self, path: &Path, line: impl fmt::Display) -> String {
        let text = format!("{}:{line}", self.path(path));
        if !self.look.hyperlinks || !path.is_absolute() {
            return text;
        }

        let url = file_url(path);
        format!("\x1b]8;;{url}\x1b\\{text}\x1b]8;;\x1b\\")
    }
}

/// `text` without its escape sequences: colours, and the links around
/// locations.
pub fn plain(text: &str) -> String {
    let mut plain = String::with_capacity(text.len());
    let mut rest = text;

    while let Some(start) = rest.find('\x1b') {
        plain.push_str(&rest[..start]);
        let sequence = &rest[start + 1..];

        let length = match sequence.chars().next() {
            // A control sequence ends at its first final byte.
            Some('[') => sequence[1..]
                .find(|character: char| ('@'..='~').contains(&character))
                .map_or(sequence.len(), |end| end + 2),
            // An operating system command ends at the string terminator.
            Some(']') => sequence
                .find("\x1b\\")
                .map_or(sequence.len(), |end| end + 2),
            _ => 0,
        };

        rest = &sequence[length..];
    }

    plain.push_str(rest);
    plain
}

/// A `file://` URL, percent-encoding what a URL cannot hold.
fn file_url(path: &Path) -> String {
    let mut url = String::from("file://");

    for byte in path.as_os_str().as_encoded_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => {
                url.push(char::from(*byte));
            }
            _ => {
                use fmt::Write as _;
                let _ = write!(url, "%{byte:02X}");
            }
        }
    }

    url
}

pub struct Painted<T> {
    style: Option<Style>,
    value: T,
}

impl<T: fmt::Display> fmt::Display for Painted<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(style) = self.style {
            write!(formatter, "{style}{}{style:#}", self.value)
        } else {
            self.value.fmt(formatter)
        }
    }
}

/// A style as a settings file writes it, such as `bold yellow`,
/// `bright-cyan on 236`, or `#ff8800 underline`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StyleSpec {
    text: String,
    style: Style,
}

impl std::str::FromStr for StyleSpec {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        let mut style = Style::new();
        let mut words = text.split_whitespace();

        while let Some(word) = words.next() {
            style = match word {
                "bold" => style.bold(),
                "dim" => style.dimmed(),
                "italic" => style.italic(),
                "underline" => style.underline(),
                "reverse" => style.invert(),
                "plain" | "none" => Style::new(),
                "on" => {
                    let color = words
                        .next()
                        .ok_or_else(|| format!("'{text}' ends without the color after 'on'"))?;
                    style.bg_color(Some(parse_color(color)?))
                }
                color => style.fg_color(Some(parse_color(color).map_err(|_| {
                    format!(
                        "unknown style word '{color}'; use a color (red, bright-red, 0-255, \
                         #rrggbb), bold, dim, italic, underline, reverse, or on COLOR"
                    )
                })?)),
            };
        }

        Ok(Self {
            text: text.to_owned(),
            style,
        })
    }
}

fn parse_color(word: &str) -> Result<Color, String> {
    const NAMES: [(&str, AnsiColor, AnsiColor); 8] = [
        ("black", AnsiColor::Black, AnsiColor::BrightBlack),
        ("red", AnsiColor::Red, AnsiColor::BrightRed),
        ("green", AnsiColor::Green, AnsiColor::BrightGreen),
        ("yellow", AnsiColor::Yellow, AnsiColor::BrightYellow),
        ("blue", AnsiColor::Blue, AnsiColor::BrightBlue),
        ("magenta", AnsiColor::Magenta, AnsiColor::BrightMagenta),
        ("cyan", AnsiColor::Cyan, AnsiColor::BrightCyan),
        ("white", AnsiColor::White, AnsiColor::BrightWhite),
    ];

    let (bright, name) = word
        .strip_prefix("bright-")
        .map_or((false, word), |name| (true, name));
    if let Some((_, normal, brighter)) = NAMES.iter().find(|(known, ..)| *known == name) {
        return Ok(Color::Ansi(if bright { *brighter } else { *normal }));
    }

    if !bright {
        if let Ok(index) = word.parse::<u8>() {
            return Ok(Color::Ansi256(Ansi256Color(index)));
        }

        if let Some(hex) = word.strip_prefix('#')
            && hex.len() == 6
            && let Ok(rgb) = u32::from_str_radix(hex, 16)
        {
            let [_, red, green, blue] = rgb.to_be_bytes();
            return Ok(Color::Rgb(RgbColor(red, green, blue)));
        }
    }

    Err(format!("unknown color '{word}'"))
}

impl<'de> Deserialize<'de> for StyleSpec {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

impl Serialize for StyleSpec {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn environment(
        term: Option<&str>,
        no_color: bool,
        clicolor: Option<bool>,
        clicolor_force: bool,
    ) -> ColorEnvironment {
        ColorEnvironment {
            term: term.map(OsString::from),
            no_color,
            clicolor,
            clicolor_force,
        }
    }

    #[test]
    fn color_policy_has_deterministic_precedence() {
        let capable = environment(Some("xterm-256color"), false, None, false);
        assert!(color_enabled(ColorChoice::Auto, &capable, true, false));
        assert!(!color_enabled(ColorChoice::Auto, &capable, false, false));
        assert!(!color_enabled(ColorChoice::Auto, &capable, true, true));

        let dumb = environment(Some("dumb"), false, None, false);
        assert!(!color_enabled(ColorChoice::Auto, &dumb, true, false));
        let unknown = environment(None, false, None, false);
        assert!(!color_enabled(ColorChoice::Auto, &unknown, true, false));

        let disabled = environment(Some("xterm"), false, Some(false), false);
        assert!(!color_enabled(ColorChoice::Auto, &disabled, true, false));
        let requested = environment(None, false, Some(true), false);
        assert!(color_enabled(ColorChoice::Auto, &requested, true, false));

        let forced = environment(None, false, None, true);
        assert!(color_enabled(ColorChoice::Auto, &forced, false, true));
        let no_color = environment(Some("xterm"), true, Some(true), true);
        assert!(!color_enabled(ColorChoice::Auto, &no_color, true, false));

        assert!(color_enabled(ColorChoice::Always, &no_color, false, true));
        assert!(!color_enabled(ColorChoice::Never, &capable, true, false));
    }

    #[test]
    fn styles_combine_colors_and_effects_and_name_what_they_refuse() {
        let style = |text: &str| text.parse::<StyleSpec>().map(|spec| spec.style);
        assert_eq!(
            style("bold yellow"),
            Ok(foreground(AnsiColor::Yellow).bold())
        );
        assert_eq!(
            style("bright-cyan on 236 underline"),
            Ok(foreground(AnsiColor::BrightCyan)
                .bg_color(Some(Color::Ansi256(Ansi256Color(236))))
                .underline())
        );
        assert_eq!(
            style("#ff8800"),
            Ok(Style::new().fg_color(Some(Color::Rgb(RgbColor(0xff, 0x88, 0)))))
        );
        assert!(style("blod").is_err_and(|error| error.contains("'blod'")));
        assert!(style("on").is_err());
        assert!(style("bright-256").is_err());
    }
}
