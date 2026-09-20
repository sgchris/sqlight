//! CLI argument parsing: `sqlight <DB_PATH>`.

use clap::Parser;
use std::path::PathBuf;

/// Convenient SQLite terminal client.
#[derive(Debug, Parser)]
#[command(name = "sqlight", version, about = "Convenient SQLite terminal client")]
pub struct Cli {
    /// Path to an existing SQLite database file.
    pub db_path: PathBuf,
}

impl Cli {
    /// Parse from process args. Exactly one positional is enforced by clap.
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
        assert_eq!(cli.db_path, PathBuf::from("demo.db"));
    }

    #[test]
    fn rejects_missing_arg() {
        assert!(Cli::try_parse_from(["sqlight"]).is_err());
    }
}
