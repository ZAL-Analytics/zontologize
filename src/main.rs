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
    #[command(subcommand, about = "Ontology commands")]
    Ontology(OntologyCommand),
    #[command(about = "Walk the sources and update documents, text, chunks and rule facts")]
    Scan {
        #[arg(
            long = "source",
            value_name = "NAME",
            help = "Scan only this source (repeatable)"
        )]
        sources: Vec<String>,
    },
    #[command(about = "Show counts per source, kind and status, recent runs and errors")]
    Status {
        #[arg(long, help = "List every error instead of the first 20")]
        errors: bool,
    },
}

#[derive(Debug, Subcommand)]
enum OntologyCommand {
    #[command(about = "Load the ontology files and report errors, warnings and the selection")]
    Check,
}

#[derive(Debug, Subcommand)]
enum DbCommand {
    #[command(about = "Create the schema if missing and apply pending migrations")]
    Migrate,
    #[command(about = "Show server, extension, schema and migration state")]
    Check,
}

const THREAD_STACK_BYTES: usize = 8 * 1024 * 1024;

fn main() -> ExitCode {
    let cli = Cli::parse();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(THREAD_STACK_BYTES)
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("error: cannot start the runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    let config = cli.config.as_deref();
    let result = runtime.block_on(async {
        match cli.command {
            Command::Db(DbCommand::Migrate) => zontologize::db_migrate(config).await,
            Command::Db(DbCommand::Check) => zontologize::db_check(config).await,
            Command::Ontology(OntologyCommand::Check) => zontologize::ontology_check(config),
            Command::Scan { sources } => zontologize::scan(config, &sources).await,
            Command::Status { errors } => zontologize::status(config, errors).await,
        }
    });
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
        assert!(Cli::try_parse_from(["zontologize", "index"]).is_err());
    }

    #[test]
    fn parses_scan_with_sources() {
        let cli =
            Cli::try_parse_from(["zontologize", "scan", "--source", "a", "--source", "b"]).unwrap();
        assert!(matches!(cli.command, Command::Scan { sources } if sources == ["a", "b"]));
    }

    #[test]
    fn parses_ontology_check_and_status() {
        let cli = Cli::try_parse_from(["zontologize", "ontology", "check"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Ontology(OntologyCommand::Check)
        ));
        let cli = Cli::try_parse_from(["zontologize", "status", "--errors"]).unwrap();
        assert!(matches!(cli.command, Command::Status { errors: true }));
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
