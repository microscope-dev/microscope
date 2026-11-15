use std::ffi::{OsStr, OsString};

use clap::builder::styling::{AnsiColor, Color, Style, Styles};
use clap::{ColorChoice, Command, ValueEnum as _};

use super::terminal::ColorChoice as OutputColorChoice;

const MAX_WIDTH: usize = 100;

const STYLES: Styles = Styles::styled()
    .header(foreground(AnsiColor::BrightBlue).bold())
    .usage(foreground(AnsiColor::BrightBlue).bold())
    .literal(foreground(AnsiColor::BrightCyan).bold())
    .placeholder(foreground(AnsiColor::Cyan))
    .context(Style::new().dimmed())
    .context_value(foreground(AnsiColor::BrightGreen))
    .error(foreground(AnsiColor::BrightRed).bold())
    .valid(foreground(AnsiColor::BrightGreen))
    .invalid(foreground(AnsiColor::BrightYellow));

const fn foreground(color: AnsiColor) -> Style {
    Style::new().fg_color(Some(Color::Ansi(color)))
}

pub fn configure(command: Command, color: ColorChoice) -> Command {
    command
        .color(color)
        .styles(STYLES)
        .max_term_width(MAX_WIDTH)
        .disable_help_subcommand(true)
}

pub fn color_choice(arguments: &[OsString]) -> ColorChoice {
    let mut arguments = arguments.iter().skip(1);
    while let Some(argument) = arguments.next() {
        if argument == "--" {
            break;
        }

        if argument == "--color" {
            return arguments
                .next()
                .and_then(|value| parse_color(value))
                .unwrap_or(ColorChoice::Auto);
        }

        if let Some(value) = argument
            .to_str()
            .and_then(|argument| argument.strip_prefix("--color="))
        {
            return parse_color(OsStr::new(value)).unwrap_or(ColorChoice::Auto);
        }
    }

    ColorChoice::Auto
}

fn parse_color(value: &OsStr) -> Option<ColorChoice> {
    OutputColorChoice::from_str(value.to_str()?, false)
        .ok()
        .map(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_choice_stops_at_the_argument_escape() {
        let arguments = ["microscope", "--color=always", "program"].map(OsString::from);
        assert_eq!(color_choice(&arguments), ColorChoice::Always);

        let escaped = ["microscope", "program", "--", "--color", "always"].map(OsString::from);
        assert_eq!(color_choice(&escaped), ColorChoice::Auto);
    }
}
