use anyhow::{Result, anyhow};
use xberg::chunking::{ChunkerType, ChunkingConfig, chunk_text};
use xberg::types::PageBoundary;

use crate::config::{ChunkConfig, TextFormat};

pub const STAGE_VERSION: u32 = 2;
const PAGE_SEPARATOR: &str = "\n\n";

#[derive(Debug, Clone, PartialEq)]
pub struct Chunk {
    pub ord: i32,
    pub page_from: Option<i32>,
    pub page_to: Option<i32>,
    pub text: String,
    pub text_start: Option<i32>,
}

pub type PageStarts = Vec<(i32, i32)>;

pub struct ChunkSettings {
    xberg: ChunkingConfig,
    pub key: String,
}

impl ChunkSettings {
    pub fn new(chunk: &ChunkConfig, format: TextFormat) -> ChunkSettings {
        let chunker_type = match format {
            TextFormat::Markdown => ChunkerType::Markdown,
            TextFormat::Plain => ChunkerType::Text,
        };
        ChunkSettings {
            xberg: ChunkingConfig {
                max_characters: chunk.max_chars,
                overlap: chunk.overlap,
                trim: true,
                chunker_type,
                ..ChunkingConfig::default()
            },
            key: crate::hash::stage_key(
                "chunk",
                STAGE_VERSION,
                &[
                    &chunk.max_chars.to_string(),
                    &chunk.overlap.to_string(),
                    format.as_str(),
                    &chunk.text_search,
                ],
            ),
        }
    }
}

fn join_pages(pages: &[(i32, String)]) -> (String, Vec<PageBoundary>, PageStarts) {
    let mut text = String::new();
    let mut boundaries = Vec::new();
    let mut starts = Vec::new();
    let mut chars = 0;
    for (page_no, page) in pages {
        if page.trim().is_empty() {
            continue;
        }
        if !text.is_empty() {
            text.push_str(PAGE_SEPARATOR);
            chars += PAGE_SEPARATOR.chars().count() as i32;
        }
        let start = text.len();
        starts.push((*page_no, chars));
        chars += page.chars().count() as i32;
        text.push_str(page);
        boundaries.push(PageBoundary {
            byte_start: start,
            byte_end: text.len(),
            page_number: (*page_no).max(1) as u32,
        });
    }
    (text, boundaries, starts)
}

pub fn chunk_pages(
    pages: &[(i32, String)],
    settings: &ChunkSettings,
) -> Result<(Vec<Chunk>, PageStarts)> {
    let (text, boundaries, starts) = join_pages(pages);
    if text.trim().is_empty() {
        return Ok((Vec::new(), starts));
    }
    let result = chunk_text(&text, &settings.xberg, Some(&boundaries))
        .map_err(|e| anyhow!("chunking failed: {e}"))?;
    let chunks = result
        .chunks
        .into_iter()
        .filter(|c| !c.content.trim().is_empty())
        .enumerate()
        .map(|(ord, c)| Chunk {
            ord: ord as i32,
            page_from: c.metadata.first_page.map(|p| p as i32),
            page_to: c.metadata.last_page.map(|p| p as i32),
            text_start: char_start(&text, c.metadata.byte_start, &c.content),
            text: c.content,
        })
        .collect();
    Ok((chunks, starts))
}

fn char_start(text: &str, byte_start: usize, content: &str) -> Option<i32> {
    let at = byte_start + text.get(byte_start..)?.find(content)?;
    Some(text[..at].chars().count() as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(max_chars: usize) -> ChunkSettings {
        ChunkSettings::new(
            &ChunkConfig {
                max_chars,
                overlap: 0,
                text_search: "simple".to_string(),
                threads: 1,
            },
            TextFormat::Markdown,
        )
    }

    #[test]
    fn page_boundaries_are_rebuilt_from_pages() {
        let pages = vec![
            (1, "first page".to_string()),
            (2, "   ".to_string()),
            (3, "third".to_string()),
        ];
        let (text, boundaries, starts) = join_pages(&pages);
        assert_eq!(text, "first page\n\nthird");
        assert_eq!(starts, [(1, 0), (3, 12)]);
        let spans: Vec<(usize, usize, u32)> = boundaries
            .iter()
            .map(|b| (b.byte_start, b.byte_end, b.page_number))
            .collect();
        assert_eq!(spans, [(0, 10, 1), (12, 17, 3)]);
    }

    #[test]
    fn chunks_carry_page_ranges() {
        let page = |word: &str| (0..40).map(|_| word).collect::<Vec<_>>().join(" ");
        let pages = vec![(1, page("alpha")), (2, page("bravo")), (3, page("charlie"))];
        let (chunks, _) = chunk_pages(&pages, &settings(300)).unwrap();
        assert!(chunks.len() >= 3, "{chunks:?}");
        assert_eq!(chunks[0].page_from, Some(1));
        assert_eq!(chunks.last().unwrap().page_to, Some(3));
        for (i, chunk) in chunks.iter().enumerate() {
            assert_eq!(chunk.ord, i as i32);
            assert!(chunk.page_from <= chunk.page_to);
            assert!(chunk.text.len() <= 300);
        }
    }

    #[test]
    fn markdown_chunks_follow_headings() {
        let text = format!(
            "# One\n\n{}\n\n# Two\n\n{}",
            "a ".repeat(60).trim(),
            "b ".repeat(60).trim()
        );
        let (chunks, _) = chunk_pages(&[(1, text)], &settings(150)).unwrap();
        assert!(
            chunks.iter().any(|c| c.text.starts_with("# Two")),
            "{chunks:?}"
        );
    }

    #[test]
    fn empty_content_has_no_chunks() {
        assert!(
            chunk_pages(&[(1, " ".to_string())], &settings(100))
                .unwrap()
                .0
                .is_empty()
        );
    }

    fn joined(pages: &[(i32, String)]) -> Vec<char> {
        join_pages(pages).0.chars().collect()
    }

    #[test]
    fn chunks_start_where_their_text_is() {
        let page = |word: &str| (0..40).map(|_| word).collect::<Vec<_>>().join(" ");
        let pages = vec![
            (1, format!("  {}", page("ærø"))),
            (2, "\n".to_string()),
            (3, page("bravo")),
            (4, page("čapek")),
        ];
        let text = joined(&pages);
        let plain = ChunkSettings::new(
            &ChunkConfig {
                max_chars: 200,
                overlap: 20,
                text_search: "simple".to_string(),
                threads: 1,
            },
            TextFormat::Plain,
        );
        for chunked in [settings(200), plain] {
            let (chunks, starts) = chunk_pages(&pages, &chunked).unwrap();
            assert!(chunks.len() >= 3, "{chunks:?}");
            for chunk in &chunks {
                let start = chunk.text_start.unwrap() as usize;
                let at: String = text[start..]
                    .iter()
                    .take(chunk.text.chars().count())
                    .collect();
                assert_eq!(at, chunk.text);
            }
            assert_eq!(starts.iter().map(|s| s.0).collect::<Vec<_>>(), [1, 3, 4]);
            for (page_no, start) in starts {
                let page = &pages.iter().find(|p| p.0 == page_no).unwrap().1;
                let at: String = text[start as usize..]
                    .iter()
                    .take(page.chars().count())
                    .collect();
                assert_eq!(&at, page);
            }
        }
    }

    #[test]
    fn key_changes_with_settings() {
        assert_ne!(settings(100).key, settings(200).key);
    }
}
