use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use zontologize::config;
use zontologize::rules::ids::KnownKeys;
use zontologize::scan::chunk::{self, ChunkSettings};
use zontologize::scan::extract::{self, ExtractSettings};

#[derive(Parser)]
#[command(about = "Chunk and run the text rules on the text of a folder and report throughput")]
struct Args {
    #[arg(default_value = "data/benchmarks")]
    folder: PathBuf,
    #[arg(
        long,
        env = "ZONTOLOGIZE_CONFIG",
        value_name = "DIR",
        default_value = "examples/acme"
    )]
    config: PathBuf,
    #[arg(long, value_delimiter = ',', default_value = "txt,eml,html")]
    extensions: Vec<String>,
    #[arg(long)]
    limit: Option<usize>,
    #[arg(long, default_value_t = 3)]
    rounds: usize,
    #[arg(long, hide = true)]
    bench: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    if !args.folder.is_dir() {
        println!(
            "skipped: {} not found; scripts/sync_data.sh --fetch-bench fills it",
            args.folder.display()
        );
        return Ok(());
    }
    let config = config::load(Some(&args.config))?;
    let mut files = Vec::new();
    walk(&args.folder, &args.extensions, &mut files)
        .with_context(|| format!("cannot walk {}", args.folder.display()))?;
    files.sort();
    if let Some(limit) = args.limit {
        files.truncate(limit);
    }
    let settings = ExtractSettings::new(&config.extract, &config.ocr);
    let mut documents = Vec::new();
    for path in &files {
        if let Ok(pages) = extract::extract_pages(path, &settings).await {
            let pages: Vec<(i32, String)> = pages
                .into_iter()
                .map(|page| (page.page_no as i32, page.text))
                .collect();
            documents.push(pages);
        }
    }
    let chars: usize = documents.iter().flatten().map(|(_, text)| text.len()).sum();
    println!(
        "folder {}: {} files with text, {:.1} MB, config {}",
        args.folder.display(),
        documents.len(),
        chars as f64 / 1e6,
        args.config.display()
    );

    let chunking = ChunkSettings::new(&config.chunk, config.extract.format);
    let mut chunks = Vec::new();
    let chunk_time = best(args.rounds, || {
        chunks.clear();
        for pages in &documents {
            chunks.extend(chunk::chunk_pages(pages, &chunking)?);
        }
        Ok(())
    })?;
    report("chunk", chunks.len(), chars, chunk_time);

    let ontology = zontologize::ontology::load_checked(Some(&args.config), &config.ontology)?;
    let rules = zontologize::rules::load(
        Some(&args.config),
        &ontology.ontology,
        &config.ontology.document_class,
    )?;
    let known = KnownKeys::new();
    let mut hits = 0;
    let rules_time = best(args.rounds, || {
        hits = 0;
        for chunk in &chunks {
            let facts = rules.text_facts(&chunk.text, None, &known);
            hits += facts.entities.len() + facts.classes.len();
        }
        Ok(())
    })?;
    report("rules", chunks.len(), chars, rules_time);
    println!("rules hits {hits}");
    Ok(())
}

fn best(rounds: usize, mut run: impl FnMut() -> Result<()>) -> Result<Duration> {
    let mut best = Duration::MAX;
    for _ in 0..rounds.max(1) {
        let start = Instant::now();
        run()?;
        best = best.min(start.elapsed());
    }
    Ok(best)
}

fn report(stage: &str, chunks: usize, chars: usize, time: Duration) {
    let seconds = time.as_secs_f64().max(f64::EPSILON);
    println!(
        "{stage:<6} {:>8.3}s {:>10.0} chunks/s {:>8.2} MB/s",
        seconds,
        chunks as f64 / seconds,
        chars as f64 / 1e6 / seconds
    );
}

fn walk(dir: &Path, extensions: &[String], files: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            walk(&path, extensions, files)?;
            continue;
        }
        let extension = path.extension().map(|e| e.to_string_lossy().to_lowercase());
        if extension.is_some_and(|e| extensions.contains(&e)) {
            files.push(path);
        }
    }
    Ok(())
}
