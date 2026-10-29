use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    #[arg(
        long,
        global = true,
        env = "ZONTOLOGIZE_CONFIG",
        value_name = "DIR",
        help = "Config directory"
    )]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    #[command(subcommand, about = "Database commands")]
    Db(DbCommand),
}

#[derive(Debug, Subcommand)]
enum DbCommand {
    #[command(about = "Create the schema if missing and apply pending migrations")]
    Migrate,
    #[command(about = "Show server, extension, schema and migration state")]
    Check,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let config = cli.config.as_deref();
    let result = match cli.command {
        Command::Db(DbCommand::Migrate) => zontologize::db_migrate(config).await,
        Command::Db(DbCommand::Check) => zontologize::db_check(config).await,
    };
    match result {
        Ok(code) => code,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_db_migrate_with_config() {
        let cli =
            Cli::try_parse_from(["zontologize", "--config", "/tmp/cfg", "db", "migrate"]).unwrap();
        assert_eq!(cli.config, Some(PathBuf::from("/tmp/cfg")));
        assert!(matches!(cli.command, Command::Db(DbCommand::Migrate)));
    }

    #[test]
    fn parses_config_after_subcommand() {
        let cli = Cli::try_parse_from(["zontologize", "db", "check", "--config", "cfg"]).unwrap();
        assert_eq!(cli.config, Some(PathBuf::from("cfg")));
        assert!(matches!(cli.command, Command::Db(DbCommand::Check)));
    }

    #[test]
    fn rejects_unknown_command() {
        assert!(Cli::try_parse_from(["zontologize", "scan"]).is_err());
    }

    #[test]
    fn requires_a_command() {
        assert!(Cli::try_parse_from(["zontologize"]).is_err());
    }

    #[test]
    fn verifies_cli() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }
}
