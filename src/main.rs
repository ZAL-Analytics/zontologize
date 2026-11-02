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
    #[command(subcommand, about = "Model commands")]
    Models(ModelsCommand),
    #[command(about = "Walk the sources and update documents, text, chunks and rule facts")]
    Scan {
        #[arg(
            long = "source",
            value_name = "NAME",
            help = "Scan only this source (repeatable)"
        )]
        sources: Vec<String>,
        #[arg(
            long,
            value_name = "MINUTES",
            help = "Stop handing out files to extract after this many minutes; the next run goes on"
        )]
        extract_minutes: Option<u64>,
    },
    #[command(
        about = "Ask the classify model for the document type of parts that rules left untyped"
    )]
    Classify(JobArgs),
    #[command(about = "Send chunks to the extract model and write the facts it finds")]
    Extract(ExtractArgs),
    #[command(subcommand, about = "Fact review commands")]
    Review(ReviewCommand),
    #[command(about = "Show counts per source, kind and status, recent runs and errors")]
    Status {
        #[arg(long, help = "List every error instead of the first 20")]
        errors: bool,
    },
    #[command(name = "extract-worker", hide = true)]
    ExtractWorker,
}

#[derive(Debug, Subcommand)]
enum OntologyCommand {
    #[command(about = "Load the ontology files and report errors, warnings and the selection")]
    Check,
    #[command(about = "Ask the fit model which shortlisted classes a sample of chunks is about")]
    Fit(FitArgs),
}

#[derive(Debug, clap::Args)]
struct FitArgs {
    #[arg(
        long,
        value_name = "N",
        help = "Chunks to sample (default [fit] sample)"
    )]
    sample: Option<usize>,
    #[arg(
        long,
        value_name = "SEED",
        help = "Seed of the sample (default [fit] seed)"
    )]
    seed: Option<String>,
    #[arg(
        long,
        value_name = "USD",
        help = "Stop the job when it has spent this much"
    )]
    max_usd: Option<f64>,
    #[arg(long, value_name = "N", help = "Stop the job after this many requests")]
    max_requests: Option<u64>,
    #[arg(long, help = "Print the estimate and send nothing")]
    dry_run: bool,
    #[arg(
        long,
        help = "Exit on a usage window or credit pause instead of waiting"
    )]
    no_wait: bool,
    #[arg(long, help = "Abandon the unfinished fit job and start a new one")]
    new: bool,
}

#[derive(Debug, clap::Args)]
struct JobArgs {
    #[arg(
        long,
        value_name = "USD",
        help = "Stop the job when it has spent this much"
    )]
    max_usd: Option<f64>,
    #[arg(long, value_name = "N", help = "Stop the job after this many requests")]
    max_requests: Option<u64>,
    #[arg(long, help = "Print the estimate and send nothing")]
    dry_run: bool,
    #[arg(
        long,
        help = "Exit on a usage window or credit pause instead of waiting"
    )]
    no_wait: bool,
    #[arg(long, help = "Abandon the unfinished job and start a new one")]
    new: bool,
}

impl JobArgs {
    fn options(&self) -> zontologize::llm::job::Options {
        let mut options = zontologize::llm::job::Options::new(zontologize::llm::claude_program());
        options.max_usd = self.max_usd;
        options.max_requests = self.max_requests;
        options.dry_run = self.dry_run;
        options.no_wait = self.no_wait;
        options.new = self.new;
        options
    }
}

#[derive(Debug, clap::Args)]
struct ExtractArgs {
    #[command(flatten)]
    job: JobArgs,
    #[arg(
        long,
        help = "Send again even when more than [llm_extract] resend_share of the chunks changed"
    )]
    yes: bool,
    #[arg(
        long,
        value_name = "N",
        help = "Extract a spread sample of N chunks instead of every eligible chunk"
    )]
    sample: Option<usize>,
    #[arg(
        long,
        value_name = "SEED",
        help = "Seed of the sample (default [llm_extract] seed)"
    )]
    seed: Option<String>,
}

#[derive(Debug, Subcommand)]
enum ReviewCommand {
    #[command(about = "Show random facts with their evidence and mark each right or wrong")]
    Sample {
        #[arg(
            long,
            value_name = "N",
            help = "Facts to show (default [review] sample)"
        )]
        n: Option<usize>,
        #[arg(
            long,
            value_name = "PREFIX",
            default_value = "",
            help = "Only facts whose method starts with this, for example llm:"
        )]
        method: String,
        #[arg(
            long,
            value_name = "NAME",
            default_value = "",
            help = "Only facts of this class or property (prefixed name or IRI)"
        )]
        class: String,
        #[arg(
            long,
            value_name = "SEED",
            help = "Seed of the draw (default: the current time)"
        )]
        seed: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum ModelsCommand {
    #[command(about = "Check each configured model and show what is left to use")]
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
            Command::Ontology(OntologyCommand::Fit(args)) => {
                let mut options =
                    zontologize::fit::run::Options::new(zontologize::llm::claude_program());
                options.sample = args.sample;
                options.seed = args.seed;
                options.max_usd = args.max_usd;
                options.max_requests = args.max_requests;
                options.dry_run = args.dry_run;
                options.no_wait = args.no_wait;
                options.new = args.new;
                zontologize::ontology_fit(config, &options).await
            }
            Command::Models(ModelsCommand::Check) => {
                zontologize::models_check(config, zontologize::llm::claude_program()).await
            }
            Command::Scan {
                sources,
                extract_minutes,
            } => zontologize::scan(config, &sources, extract_minutes).await,
            Command::Classify(args) => zontologize::classify(config, &args.options()).await,
            Command::Extract(args) => {
                let mut options = args.job.options();
                options.yes = args.yes;
                options.sample = args.sample;
                options.seed = args.seed;
                zontologize::extract(config, &options).await
            }
            Command::Review(ReviewCommand::Sample {
                n,
                method,
                class,
                seed,
            }) => {
                let options = zontologize::review::Options {
                    n,
                    method,
                    class,
                    seed: seed.unwrap_or_else(|| {
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map_or(0, |d| d.as_nanos())
                            .to_string()
                    }),
                };
                zontologize::review_sample(config, &options).await
            }
            Command::Status { errors } => zontologize::status(config, errors).await,
            Command::ExtractWorker => zontologize::extract_worker().await,
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
        assert!(
            matches!(cli.command, Command::Scan { sources, extract_minutes: None } if sources == ["a", "b"])
        );
        let cli = Cli::try_parse_from(["zontologize", "scan", "--extract-minutes", "480"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Scan {
                extract_minutes: Some(480),
                ..
            }
        ));
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
    fn extract_worker_is_hidden() {
        use clap::CommandFactory;
        let cli = Cli::try_parse_from(["zontologize", "extract-worker"]).unwrap();
        assert!(matches!(cli.command, Command::ExtractWorker));
        let help = Cli::command().render_help().to_string();
        assert!(!help.contains("extract-worker"), "{help}");
    }

    #[test]
    fn parses_ontology_fit_and_models_check() {
        let cli = Cli::try_parse_from([
            "zontologize",
            "ontology",
            "fit",
            "--sample",
            "50",
            "--max-usd",
            "0.5",
            "--dry-run",
            "--no-wait",
        ])
        .unwrap();
        let Command::Ontology(OntologyCommand::Fit(args)) = cli.command else {
            panic!("not fit");
        };
        assert_eq!(args.sample, Some(50));
        assert_eq!(args.max_usd, Some(0.5));
        assert!(args.dry_run && args.no_wait && !args.new);
        let cli = Cli::try_parse_from(["zontologize", "models", "check"]).unwrap();
        assert!(matches!(cli.command, Command::Models(ModelsCommand::Check)));
    }

    #[test]
    fn parses_extract() {
        let cli = Cli::try_parse_from([
            "zontologize",
            "extract",
            "--max-requests",
            "5",
            "--yes",
            "--no-wait",
        ])
        .unwrap();
        let Command::Extract(args) = cli.command else {
            panic!("not extract");
        };
        assert_eq!(args.job.max_requests, Some(5));
        assert!(args.yes && args.job.no_wait && !args.job.dry_run);
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
