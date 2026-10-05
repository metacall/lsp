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
pub enum Command {
    Serve(Option<LogLevel>),
    Version,
    Help,
}

pub fn parse_args<I>(args: I) -> anyhow::Result<Command>
where
    I: IntoIterator<Item = String>,
{
    let mut args = args.into_iter();
    let mut command: Option<Command> = None;
    let mut level = None;
    let set_command = |slot: &mut Option<Command>,
                       next: Command,
                       flag: &str,
                       level: &Option<LogLevel>|
     -> anyhow::Result<()> {
        if slot.is_some() || level.is_some() {
            anyhow::bail!("{flag} cannot be combined with another command");
        }
        *slot = Some(next);
        Ok(())
    };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-V" | "--version" => set_command(&mut command, Command::Version, &arg, &level)?,
            "-h" | "--help" => set_command(&mut command, Command::Help, &arg, &level)?,
            "--stdio" => {}
            "--log-level" => {
                let Some(value) = args.next() else {
                    anyhow::bail!("--log-level needs a value; pass --help for usage");
                };
                if command.is_some() {
                    anyhow::bail!("--log-level cannot be combined with --version or --help");
                }
                level = Some(value.parse::<LogLevel>()?);
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

    fn args(flags: &[&str]) -> Vec<String> {
        flags.iter().map(|flag| flag.to_string()).collect()
    }

    #[test]
    fn version_flags_are_recognized() {
        assert_eq!(parse_args(args(&["-V"])).unwrap(), Command::Version);
        assert_eq!(parse_args(args(&["--version"])).unwrap(), Command::Version);
    }

    #[test]
    fn help_flags_are_recognized() {
        assert_eq!(parse_args(args(&["-h"])).unwrap(), Command::Help);
        assert_eq!(parse_args(args(&["--help"])).unwrap(), Command::Help);
    }

    #[test]
    fn client_flags_keep_the_server_running() {
        assert_eq!(
            parse_args(args(&["--stdio"])).unwrap(),
            Command::Serve(None)
        );
        assert_eq!(
            parse_args(Vec::<String>::new()).unwrap(),
            Command::Serve(None),
            "no flag leaves the log level to RUST_LOG"
        );
    }

    #[test]
    fn a_log_level_is_parsed_and_an_unknown_one_is_rejected() {
        assert_eq!(
            parse_args(args(&["--log-level", "debug"])).unwrap(),
            Command::Serve(Some(LogLevel::Debug))
        );
        assert_eq!(
            parse_args(args(&["--log-level", "trace", "--stdio"])).unwrap(),
            Command::Serve(Some(LogLevel::Trace)),
            "the transport flag does not clear the level"
        );
        assert!(parse_args(args(&["--log-level", "loud"])).is_err());
        assert!(parse_args(args(&["--log-level"])).is_err());
    }

    #[test]
    fn unknown_arguments_are_rejected() {
        assert!(parse_args(args(&["--bogus"])).is_err());
    }

    #[test]
    fn conflicting_commands_are_rejected() {
        assert!(parse_args(args(&["--version", "--help"])).is_err());
        assert!(parse_args(args(&["--log-level", "debug", "--version"])).is_err());
    }
}
