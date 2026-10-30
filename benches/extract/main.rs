mod levenshtein;

use std::collections::{BTreeMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use zontologize::config::{self, ModelTier};
use zontologize::scan::extract::{self, ExtractSettings, Method, Page};

const TRUTH_SUFFIX: &str = ".truth.txt";
const DATASET_FILES: [&str; 2] = ["manifest.csv", "README.md"];

#[derive(Parser)]
#[command(about = "Extract every file in a folder and report throughput and OCR quality")]
struct Args {
    #[arg(default_value = "data/benchmarks")]
    folder: PathBuf,
    #[arg(long, env = "ZONTOLOGIZE_CONFIG", value_name = "DIR")]
    config: Option<PathBuf>,
    #[arg(long, value_enum)]
    tier: Option<ModelTier>,
    #[arg(long)]
    limit: Option<usize>,
    #[arg(long, num_args = 1.., value_name = "FILE")]
    wordlist: Vec<PathBuf>,
    #[arg(long, value_name = "DIR")]
    dump: Option<PathBuf>,
    #[arg(long, default_value_t = 20)]
    dump_count: usize,
    #[arg(long, hide = true)]
    bench: bool,
}

struct Outcome {
    path: PathBuf,
    elapsed: Duration,
    result: Result<Vec<Page>>,
}

#[derive(Default)]
struct KindStats {
    files: usize,
    errors: usize,
    timeouts: usize,
    pages: usize,
    ocr_pages: usize,
    confidence_sum: f64,
    confidence_count: usize,
    busy: Duration,
    cer_sum: f64,
    cer_count: usize,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let mut config = config::load(args.config.as_deref())?;
    if let Some(tier) = args.tier {
        config.ocr.model_tier = tier;
    }
    let mut files = Vec::new();
    walk(&args.folder, &args.folder, &mut files)
        .with_context(|| format!("cannot walk {}", args.folder.display()))?;
    files.sort();
    if let Some(limit) = args.limit {
        files.truncate(limit);
    }
    println!(
        "folder {}: {} files, backend {}, tier {}, languages {}, threads {}, timeout {}s",
        args.folder.display(),
        files.len(),
        extract::OCR_BACKEND,
        extract::tier_name(config.ocr.model_tier),
        config.ocr.languages.join("+"),
        config.extract.threads,
        config.extract.timeout_secs,
    );

    let settings = Arc::new(ExtractSettings::new(&config.extract, &config.ocr));
    let semaphore = Arc::new(Semaphore::new(config.extract.threads));
    let cpu_before = cpu_seconds();
    let start = Instant::now();
    let mut tasks = JoinSet::new();
    let mut outcomes = Vec::with_capacity(files.len());
    for path in files {
        let permit = semaphore.clone().acquire_owned().await?;
        let settings = settings.clone();
        tasks.spawn(async move {
            let _permit = permit;
            let started = Instant::now();
            let result = extract::extract_file(&path, &settings).await;
            Outcome {
                path,
                elapsed: started.elapsed(),
                result,
            }
        });
        while let Some(done) = tasks.try_join_next() {
            outcomes.push(done?);
        }
    }
    while let Some(done) = tasks.join_next().await {
        outcomes.push(done?);
    }
    let wall = start.elapsed();
    let cpu = cpu_seconds()
        .zip(cpu_before)
        .map(|(after, before)| after - before);

    outcomes.sort_by(|a, b| a.path.cmp(&b.path));
    report(&args, &outcomes, wall, cpu)
}

fn walk(root: &Path, dir: &Path, files: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || name.ends_with(TRUTH_SUFFIX) {
            continue;
        }
        if dir == root && DATASET_FILES.contains(&name.as_str()) {
            continue;
        }
        if entry.file_type()?.is_dir() {
            walk(root, &path, files)?;
        } else {
            files.push(path);
        }
    }
    Ok(())
}

fn extension(path: &Path) -> String {
    path.extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_else(|| "(none)".to_string())
}

fn kind(path: &Path, pages: &[Page]) -> String {
    let extension = extension(path);
    if pages.iter().any(|page| page.method == Method::Ocr) {
        return format!("{extension}+ocr");
    }
    extension
}

fn truth_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(TRUTH_SUFFIX);
    PathBuf::from(name)
}

fn report(args: &Args, outcomes: &[Outcome], wall: Duration, cpu: Option<f64>) -> Result<()> {
    let wordlist = load_wordlist(&args.wordlist)?;
    let mut stats: BTreeMap<String, KindStats> = BTreeMap::new();
    let mut failures = Vec::new();
    let mut ocr_pages = Vec::new();
    let mut words_total = 0usize;
    let mut words_known = 0usize;

    for outcome in outcomes {
        let pages = match &outcome.result {
            Ok(pages) => pages.as_slice(),
            Err(_) => &[],
        };
        let entry = stats.entry(kind(&outcome.path, pages)).or_default();
        entry.files += 1;
        entry.busy += outcome.elapsed;
        if let Err(error) = &outcome.result {
            if extract::is_timeout(error) {
                entry.timeouts += 1;
                failures.push(format!("timeout: {}", outcome.path.display()));
            } else {
                entry.errors += 1;
                failures.push(format!("error: {error:#}"));
            }
            continue;
        }
        entry.pages += pages.len();
        for page in pages.iter().filter(|page| page.method == Method::Ocr) {
            entry.ocr_pages += 1;
            if let Some(confidence) = page.ocr_confidence {
                entry.confidence_sum += confidence;
                entry.confidence_count += 1;
            }
            if let Some(words) = &wordlist {
                for word in words_of(&page.text) {
                    words_total += 1;
                    if words.contains(&word) {
                        words_known += 1;
                    }
                }
            }
            ocr_pages.push((&outcome.path, page));
        }
        let truth = truth_path(&outcome.path);
        if truth.exists() {
            let truth = std::fs::read_to_string(&truth)
                .with_context(|| format!("cannot read {}", truth.display()))?;
            let text: Vec<&str> = pages.iter().map(|page| page.text.as_str()).collect();
            if let Some(cer) = levenshtein::character_error_rate(&text.join("\n"), &truth) {
                entry.cer_sum += cer;
                entry.cer_count += 1;
            }
        }
    }

    println!();
    println!(
        "{:<12} {:>6} {:>6} {:>8} {:>7} {:>9} {:>9} {:>9} {:>9} {:>7}",
        "kind",
        "files",
        "errors",
        "timeouts",
        "pages",
        "ocr_pages",
        "files/s",
        "pages/s",
        "ocr_conf",
        "cer"
    );
    let mut total = KindStats::default();
    for (kind, kind_stats) in &stats {
        print_row(kind, kind_stats, kind_stats.busy);
        total.files += kind_stats.files;
        total.errors += kind_stats.errors;
        total.timeouts += kind_stats.timeouts;
        total.pages += kind_stats.pages;
        total.ocr_pages += kind_stats.ocr_pages;
        total.confidence_sum += kind_stats.confidence_sum;
        total.confidence_count += kind_stats.confidence_count;
        total.cer_sum += kind_stats.cer_sum;
        total.cer_count += kind_stats.cer_count;
    }
    print_row("total (wall)", &total, wall);
    println!();
    println!("per-kind files/s and pages/s use the summed time of that kind's files");
    println!("wall time {:.1}s", wall.as_secs_f64());
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    match cpu {
        Some(cpu) => println!(
            "cpu time {:.1}s, {:.0}% of {} cores",
            cpu,
            100.0 * cpu / wall.as_secs_f64() / cores as f64,
            cores
        ),
        None => println!("cpu time not available"),
    }
    if wordlist.is_some() {
        let share = if words_total == 0 {
            0.0
        } else {
            100.0 * words_known as f64 / words_total as f64
        };
        println!("ocr words in word lists: {words_known} of {words_total} ({share:.1}%)");
    }
    for failure in &failures {
        println!("{failure}");
    }
    if let Some(dir) = &args.dump {
        dump(dir, &ocr_pages, args.dump_count)?;
    }
    Ok(())
}

fn print_row(kind: &str, stats: &KindStats, time: Duration) {
    let seconds = time.as_secs_f64().max(f64::EPSILON);
    let confidence = mean(stats.confidence_sum, stats.confidence_count);
    let cer = mean(stats.cer_sum, stats.cer_count);
    println!(
        "{:<12} {:>6} {:>6} {:>8} {:>7} {:>9} {:>9.2} {:>9.2} {:>9} {:>7}",
        kind,
        stats.files,
        stats.errors,
        stats.timeouts,
        stats.pages,
        stats.ocr_pages,
        stats.files as f64 / seconds,
        stats.pages as f64 / seconds,
        confidence.map_or("-".to_string(), |c| format!("{c:.3}")),
        cer.map_or("-".to_string(), |c| format!("{:.2}%", 100.0 * c)),
    );
}

fn mean(sum: f64, count: usize) -> Option<f64> {
    (count > 0).then(|| sum / count as f64)
}

fn load_wordlist(paths: &[PathBuf]) -> Result<Option<HashSet<String>>> {
    if paths.is_empty() {
        return Ok(None);
    }
    let mut words = HashSet::new();
    for path in paths {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read word list {}", path.display()))?;
        words.extend(text.split_whitespace().map(str::to_lowercase));
    }
    Ok(Some(words))
}

fn words_of(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_alphabetic())
        .filter(|word| word.chars().count() >= 2)
        .map(str::to_lowercase)
}

fn dump(dir: &Path, pages: &[(&PathBuf, &Page)], count: usize) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let mut keyed: Vec<(u64, &(&PathBuf, &Page))> = pages
        .iter()
        .map(|entry| {
            let mut hasher = DefaultHasher::new();
            (entry.0, entry.1.page_no).hash(&mut hasher);
            (hasher.finish(), entry)
        })
        .collect();
    keyed.sort_by_key(|(key, _)| *key);
    for (index, (_, (path, page))) in keyed.iter().take(count).enumerate() {
        let target = dir.join(format!("{index:02}.txt"));
        let confidence = page
            .ocr_confidence
            .map_or("-".to_string(), |c| format!("{c:.3}"));
        let text = format!(
            "{} page {} confidence {}\n\n{}\n",
            path.display(),
            page.page_no,
            confidence,
            page.text
        );
        std::fs::write(&target, text)
            .with_context(|| format!("cannot write {}", target.display()))?;
    }
    println!(
        "wrote {} ocr pages to {}",
        count.min(pages.len()),
        dir.display()
    );
    Ok(())
}

fn cpu_seconds() -> Option<f64> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    let fields: Vec<&str> = stat.rsplit_once(')')?.1.split_whitespace().collect();
    let utime: f64 = fields.get(11)?.parse().ok()?;
    let stime: f64 = fields.get(12)?.parse().ok()?;
    let ticks = std::process::Command::new("getconf")
        .arg("CLK_TCK")
        .output()
        .ok()?;
    let ticks: f64 = String::from_utf8(ticks.stdout).ok()?.trim().parse().ok()?;
    Some((utime + stime) / ticks)
}
