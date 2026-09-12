//! MetaCall polyglot language server binary.

use std::io::Write;

use meta_call_lsp::types::LogLevel;

const USAGE: &str = "\
meta-call-lsp - polyglot language server over the meta-ast index

Usage:
  meta-call-lsp [--stdio] [--log-level <level>]

Options:
  --stdio              Select the stdio transport (the only transport).
  --log-level <level>  Log verbosity: error, warn, info, debug, or trace.
  -V, --version        Print the version and exit.
  -h, --help           Print this help and exit.

The server speaks LSP over stdin and stdout. Logs go to stderr; RUST_LOG
overrides the level.";

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Serve(Option<LogLevel>),
    Version,
    Help,
}

fn parse_args<I, S>(args: I) -> anyhow::Result<Command>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut args = args.into_iter();
    let mut command = None;
    let mut level = None;
    while let Some(arg) = args.next() {
        match arg.as_ref() {
            "-V" | "--version" => command = Some(Command::Version),
            "-h" | "--help" => command = Some(Command::Help),
            "--stdio" => {}
            "--log-level" => {
                let Some(value) = args.next() else {
                    anyhow::bail!("--log-level needs a value; pass --help for usage");
                };
                level = Some(LogLevel::try_from(value.as_ref())?);
            }
            other => anyhow::bail!("unknown argument {other:?}; pass --help for usage"),
        }
    }
    Ok(command.unwrap_or(Command::Serve(level)))
}

fn print_line(text: &str) -> anyhow::Result<()> {
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    writeln!(stdout, "{text}")?;
    Ok(())
}

fn main() -> anyhow::Result<()> {
    match parse_args(std::env::args().skip(1))? {
        Command::Version => print_line(concat!("meta-call-lsp ", env!("CARGO_PKG_VERSION"))),
        Command::Help => print_line(USAGE),
        Command::Serve(level) => meta_call_lsp::server::run(level),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_flags_are_recognized() {
        assert_eq!(parse_args(["-V"]).unwrap(), Command::Version);
        assert_eq!(parse_args(["--version"]).unwrap(), Command::Version);
    }

    #[test]
    fn help_flags_are_recognized() {
        assert_eq!(parse_args(["-h"]).unwrap(), Command::Help);
        assert_eq!(parse_args(["--help"]).unwrap(), Command::Help);
    }

    #[test]
    fn client_flags_keep_the_server_running() {
        assert_eq!(parse_args(["--stdio"]).unwrap(), Command::Serve(None));
        assert_eq!(
            parse_args(std::iter::empty::<&str>()).unwrap(),
            Command::Serve(None),
            "no flag leaves the log level to RUST_LOG"
        );
    }

    #[test]
    fn a_log_level_is_parsed_and_an_unknown_one_is_rejected() {
        assert_eq!(
            parse_args(["--log-level", "debug"]).unwrap(),
            Command::Serve(Some(LogLevel::Debug))
        );
        assert_eq!(
            parse_args(["--log-level", "trace", "--stdio"]).unwrap(),
            Command::Serve(Some(LogLevel::Trace)),
            "the transport flag does not clear the level"
        );
        assert!(parse_args(["--log-level", "loud"]).is_err());
        assert!(parse_args(["--log-level"]).is_err());
    }

    #[test]
    fn unknown_arguments_are_rejected() {
        assert!(parse_args(["--bogus"]).is_err());
    }
}
