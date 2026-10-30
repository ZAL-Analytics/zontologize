use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use sha2::{Digest, Sha256};
use sqlx::types::chrono::{DateTime, Utc};
use tokio::sync::mpsc;

use crate::config::KindConfig;
use crate::ontology::hex;

const HASH_BUFFER: usize = 1 << 20;

#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Read(String),
    Skip {
        kind: Option<String>,
        reason: String,
    },
}

pub struct Kinds {
    by_extension: HashMap<String, (String, u64)>,
}

impl Kinds {
    pub fn new(kinds: &std::collections::BTreeMap<String, KindConfig>) -> Kinds {
        let mut by_extension = HashMap::new();
        for (kind, settings) in kinds {
            for extension in &settings.extensions {
                let extension = extension.trim_start_matches('.').to_lowercase();
                by_extension.insert(extension, (kind.clone(), settings.max_mb));
            }
        }
        Kinds { by_extension }
    }

    pub fn decide(&self, path: &str, size: u64) -> Decision {
        let name = path.rsplit('/').next().unwrap_or(path);
        let extension = match name.rsplit_once('.') {
            Some((stem, extension)) if !stem.is_empty() => extension.to_lowercase(),
            _ => {
                return Decision::Skip {
                    kind: None,
                    reason: "no extension".to_string(),
                };
            }
        };
        let Some((kind, max_mb)) = self.by_extension.get(&extension) else {
            return Decision::Skip {
                kind: None,
                reason: format!("extension .{extension}"),
            };
        };
        if *max_mb > 0 && size > max_mb * 1024 * 1024 {
            return Decision::Skip {
                kind: Some(kind.clone()),
                reason: format!("over {max_mb} MB"),
            };
        }
        Decision::Read(kind.clone())
    }
}

pub fn globs(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = Glob::new(pattern).with_context(|| format!("invalid glob {pattern:?}"))?;
        builder.add(glob);
    }
    Ok(builder.build()?)
}

#[derive(Debug, Clone)]
pub struct FileEntry {
    pub path: String,
    pub size: u64,
    pub mtime: DateTime<Utc>,
}

pub enum WalkItem {
    Files(Vec<FileEntry>),
    Error(String),
}

pub fn mtime(time: SystemTime) -> DateTime<Utc> {
    let time: DateTime<Utc> = time.into();
    let micros = time.timestamp_micros();
    DateTime::from_timestamp_micros(micros).unwrap_or(time)
}

pub fn relative(root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?;
    let parts: Vec<String> = relative
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    Some(parts.join("/"))
}

pub fn walk(
    root: PathBuf,
    ignore: GlobSet,
    batch_size: usize,
    sender: mpsc::Sender<WalkItem>,
) -> Result<bool> {
    let mut complete = true;
    let mut batch = Vec::with_capacity(batch_size);
    let walker = walkdir::WalkDir::new(&root)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|entry| {
            relative(&root, entry.path()).is_none_or(|p| p.is_empty() || !ignore.is_match(&p))
        });
    for entry in walker {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                complete = false;
                let path = error
                    .path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                if sender
                    .blocking_send(WalkItem::Error(format!("{path}: {error}")))
                    .is_err()
                {
                    return Ok(false);
                }
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let Some(path) = relative(&root, entry.path()) else {
            continue;
        };
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(error) => {
                complete = false;
                if sender
                    .blocking_send(WalkItem::Error(format!("{path}: {error}")))
                    .is_err()
                {
                    return Ok(false);
                }
                continue;
            }
        };
        let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        batch.push(FileEntry {
            path,
            size: metadata.len(),
            mtime: mtime(modified),
        });
        if batch.len() >= batch_size {
            let full = std::mem::replace(&mut batch, Vec::with_capacity(batch_size));
            if sender.blocking_send(WalkItem::Files(full)).is_err() {
                return Ok(false);
            }
        }
    }
    if !batch.is_empty() && sender.blocking_send(WalkItem::Files(batch)).is_err() {
        return Ok(false);
    }
    Ok(complete)
}

pub fn hash_file(path: &Path) -> Result<String> {
    let mut file =
        std::fs::File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; HASH_BUFFER];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("cannot read {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex(&hasher.finalize()))
}

pub fn hash_all(paths: &[PathBuf], threads: usize) -> Vec<Result<String>> {
    super::parallel_map(paths, threads, |path| hash_file(path))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn kinds() -> Kinds {
        let mut map = BTreeMap::new();
        map.insert(
            "pdf".to_string(),
            KindConfig {
                extensions: vec!["PDF".to_string()],
                max_mb: 1,
            },
        );
        map.insert(
            "text".to_string(),
            KindConfig {
                extensions: vec![".txt".to_string()],
                max_mb: 0,
            },
        );
        Kinds::new(&map)
    }

    #[test]
    fn kind_lookup_by_extension_ignores_case() {
        assert_eq!(
            kinds().decide("a/B.Pdf", 10),
            Decision::Read("pdf".to_string())
        );
        assert_eq!(
            kinds().decide("a/b.txt", 10),
            Decision::Read("text".to_string())
        );
    }

    #[test]
    fn unknown_extension_and_no_extension_are_skipped() {
        assert_eq!(
            kinds().decide("a/b.zip", 10),
            Decision::Skip {
                kind: None,
                reason: "extension .zip".to_string()
            }
        );
        assert_eq!(
            kinds().decide("a/.profile", 10),
            Decision::Skip {
                kind: None,
                reason: "no extension".to_string()
            }
        );
    }

    #[test]
    fn size_cap_skips_with_kind() {
        assert_eq!(
            kinds().decide("big.pdf", 2 * 1024 * 1024),
            Decision::Skip {
                kind: Some("pdf".to_string()),
                reason: "over 1 MB".to_string()
            }
        );
        assert_eq!(
            kinds().decide("huge.txt", u64::MAX / 2),
            Decision::Read("text".to_string())
        );
    }

    #[test]
    fn glob_filter_matches_relative_paths() {
        let set = globs(&[
            "**/~$*".to_string(),
            "**/Thumbs.db".to_string(),
            "tmp/**".to_string(),
        ])
        .unwrap();
        assert!(set.is_match("a/b/~$report.docx"));
        assert!(set.is_match("~$x.xlsx"));
        assert!(set.is_match("x/Thumbs.db"));
        assert!(set.is_match("tmp/a.txt"));
        assert!(!set.is_match("a/report.docx"));
    }

    #[test]
    fn mtime_is_truncated_to_microseconds() {
        let time = SystemTime::UNIX_EPOCH + std::time::Duration::from_nanos(1_500_000_789);
        assert_eq!(mtime(time).timestamp_subsec_nanos(), 500_000_000);
    }

    #[test]
    fn hashes_files_in_parallel() {
        let dir = std::env::temp_dir().join(format!("zontologize-hash-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a");
        let b = dir.join("b");
        std::fs::write(&a, b"abc").unwrap();
        std::fs::write(&b, b"").unwrap();
        let hashes = hash_all(&[a, b, dir.join("missing")], 4);
        assert_eq!(
            hashes[0].as_ref().unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hashes[1].as_ref().unwrap(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert!(hashes[2].is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
