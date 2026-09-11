//! MetaCall polyglot language server binary.

use std::io::Write;

const USAGE: &str = "\
meta-call-lsp - polyglot language server over the meta-ast index

Usage:
  meta-call-lsp [--stdio]

Options:
  --stdio        Accepted for compatibility; the server always uses stdio.
  -V, --version  Print the version and exit.
  -h, --help     Print this help and exit.

The server speaks LSP over stdin and stdout. Logs go to stderr; set RUST_LOG
to control them.";

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Serve,
    Version,
    Help,
}

fn parse_args<I, S>(args: I) -> Command
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut command = Command::Serve;
    for arg in args {
        command = match arg.as_ref() {
            "-V" | "--version" => Command::Version,
            "-h" | "--help" => Command::Help,
            _ => continue,
        };
    }
    command
}

fn print_line(text: &str) -> anyhow::Result<()> {
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    writeln!(stdout, "{text}")?;
    Ok(())
}

fn main() -> anyhow::Result<()> {
    match parse_args(std::env::args().skip(1)) {
        Command::Version => print_line(concat!("meta-call-lsp ", env!("CARGO_PKG_VERSION"))),
        Command::Help => print_line(USAGE),
        Command::Serve => meta_call_lsp::server::run(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_flags_are_recognized() {
        assert_eq!(parse_args(["-V"]), Command::Version);
        assert_eq!(parse_args(["--version"]), Command::Version);
    }

    #[test]
    fn help_flags_are_recognized() {
        assert_eq!(parse_args(["-h"]), Command::Help);
        assert_eq!(parse_args(["--help"]), Command::Help);
    }

    #[test]
    fn client_flags_keep_the_server_running() {
        assert_eq!(parse_args(["--stdio"]), Command::Serve);
        assert_eq!(parse_args(std::iter::empty::<&str>()), Command::Serve);
    }
}
