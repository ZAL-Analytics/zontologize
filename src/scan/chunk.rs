use anyhow::{Result, anyhow};
use xberg::chunking::{ChunkerType, ChunkingConfig, chunk_text};
use xberg::types::PageBoundary;

use crate::config::{ChunkConfig, TextFormat};

pub const STAGE_VERSION: u32 = 1;
const PAGE_SEPARATOR: &str = "\n\n";

#[derive(Debug, Clone, PartialEq)]
pub struct Chunk {
    pub ord: i32,
    pub page_from: Option<i32>,
    pub page_to: Option<i32>,
    pub text: String,
}

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
            key: format!(
                "chunk-v{STAGE_VERSION} {} {} {} {}",
                chunk.max_chars,
                chunk.overlap,
                format.as_str(),
                chunk.text_search
            ),
        }
    }
}

pub fn join_pages(pages: &[(i32, String)]) -> (String, Vec<PageBoundary>) {
    let mut text = String::new();
    let mut boundaries = Vec::new();
    for (page_no, page) in pages {
        if page.trim().is_empty() {
            continue;
        }
        if !text.is_empty() {
            text.push_str(PAGE_SEPARATOR);
        }
        let start = text.len();
        text.push_str(page);
        boundaries.push(PageBoundary {
            byte_start: start,
            byte_end: text.len(),
            page_number: (*page_no).max(1) as u32,
        });
    }
    (text, boundaries)
}

pub fn chunk_pages(pages: &[(i32, String)], settings: &ChunkSettings) -> Result<Vec<Chunk>> {
    let (text, boundaries) = join_pages(pages);
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    let result = chunk_text(&text, &settings.xberg, Some(&boundaries))
        .map_err(|e| anyhow!("chunking failed: {e}"))?;
    Ok(result
        .chunks
        .into_iter()
        .filter(|c| !c.content.trim().is_empty())
        .enumerate()
        .map(|(ord, c)| Chunk {
            ord: ord as i32,
            page_from: c.metadata.first_page.map(|p| p as i32),
            page_to: c.metadata.last_page.map(|p| p as i32),
            text: c.content,
        })
        .collect())
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
        let (text, boundaries) = join_pages(&pages);
        assert_eq!(text, "first page\n\nthird");
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
        let chunks = chunk_pages(&pages, &settings(300)).unwrap();
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
        let chunks = chunk_pages(&[(1, text)], &settings(150)).unwrap();
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
                .is_empty()
        );
    }

    #[test]
    fn key_changes_with_settings() {
        assert_ne!(settings(100).key, settings(200).key);
    }
}
