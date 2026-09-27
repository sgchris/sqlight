//! CLI argument parsing: `sqlight <TARGET>`.

use clap::Parser;

/// Convenient SQLite / PostgreSQL terminal client.
#[derive(Debug, Parser)]
#[command(
    name = "sqlight",
    version,
    about = "Convenient SQLite / PostgreSQL terminal client"
)]
pub struct Cli {
    // Optional for clap so a missing target gets our own usage message.
    /// SQLite file path or connection name from ~/.config/sqlight/connections.json.
    pub target: Option<String>,
}

impl Cli {
    /// Parse from process args. At most one positional is enforced by clap.
    pub fn parse_args() -> Self {
        Self::parse()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parses_single_positional() {
        let cli = Cli::try_parse_from(["sqlight", "demo.db"]).expect("parse");
        assert_eq!(cli.target.as_deref(), Some("demo.db"));
    }

    #[test]
    fn missing_arg_parses_as_none() {
        let cli = Cli::try_parse_from(["sqlight"]).expect("parse");
        assert!(cli.target.is_none());
    }

    #[test]
    fn rejects_extra_args() {
        assert!(Cli::try_parse_from(["sqlight", "a", "b"]).is_err());
    }
}
