use std::sync::LazyLock;

use anyhow::{Result, anyhow, bail};
use ort::session::Session;
use ort::session::builder::GraphOptimizationLevel;
use ort::value::Tensor;
use regex::Regex;
use tokenizers::Tokenizer;

use crate::embed::local::Files;

const ENT_TOKEN: &str = "<<ENT>>";
const SEP_TOKEN: &str = "<<SEP>>";
const CLS_TOKEN: &str = "[CLS]";
const END_TOKEN: &str = "[SEP]";
const LOGITS: &str = "logits";

static WORDS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\w+(?:[-_]\w+)*|\S").unwrap());

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Word {
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Span {
    pub first: usize,
    pub last: usize,
    pub label: usize,
    pub score: f32,
}

pub fn words(text: &str) -> Vec<Word> {
    WORDS
        .find_iter(text)
        .map(|m| Word {
            start: m.start(),
            end: m.end(),
        })
        .collect()
}

pub fn span_inputs(words: usize, max_width: usize, columns: usize) -> (Vec<i64>, Vec<bool>) {
    let mut index = Vec::with_capacity(columns * max_width * 2);
    let mut mask = Vec::with_capacity(columns * max_width);
    for start in 0..columns {
        for width in 0..max_width {
            index.push(start as i64);
            index.push((start + width) as i64);
            mask.push(start + width < words);
        }
    }
    (index, mask)
}

fn sigmoid(x: f32) -> f32 {
    1.0 / (1.0 + (-x).exp())
}

pub fn decode(
    logits: &[f32],
    words: usize,
    max_width: usize,
    labels: usize,
    threshold: f32,
) -> Vec<Span> {
    let mut found = Vec::new();
    for first in 0..words {
        for width in 0..max_width {
            let last = first + width;
            if last >= words {
                break;
            }
            for label in 0..labels {
                let at = (first * max_width + width) * labels + label;
                let Some(&logit) = logits.get(at) else {
                    continue;
                };
                let score = sigmoid(logit);
                if score > threshold {
                    found.push(Span {
                        first,
                        last,
                        label,
                        score,
                    });
                }
            }
        }
    }
    found.sort_by(|a, b| b.score.total_cmp(&a.score));
    let mut kept: Vec<Span> = Vec::new();
    for span in found {
        let free = kept
            .iter()
            .all(|k| span.last < k.first || span.first > k.last);
        if free {
            kept.push(span);
        }
    }
    kept.sort_by_key(|s| s.first);
    kept
}

pub struct Gliner {
    session: Session,
    tokenizer: Tokenizer,
    prompt: Vec<u32>,
    cls: u32,
    end: u32,
    max_width: usize,
    threshold: f32,
}

struct Row {
    ids: Vec<i64>,
    mask: Vec<i64>,
    words: usize,
}

impl Gliner {
    pub fn load(
        files: &Files,
        labels: &[String],
        max_width: usize,
        threshold: f32,
        threads: usize,
    ) -> Result<Gliner> {
        let path = files.model.display().to_string();
        let session = Session::builder()
            .map_err(|e| anyhow!("{path}: {e}"))?
            .with_optimization_level(GraphOptimizationLevel::All)
            .map_err(|e| anyhow!("{path}: {e}"))?
            .with_intra_threads(threads)
            .map_err(|e| anyhow!("{path}: {e}"))?
            .commit_from_file(&files.model)
            .map_err(|e| anyhow!("cannot load {path}: {e}"))?;
        let tokenizer = Tokenizer::from_file(&files.tokenizer)
            .map_err(|e| anyhow!("cannot load {}: {e}", files.tokenizer.display()))?;
        let id = |token: &str| -> Result<u32> {
            tokenizer
                .token_to_id(token)
                .ok_or_else(|| anyhow!("{}: no token {token}", files.tokenizer.display()))
        };
        let (ent, sep, cls, end) = (
            id(ENT_TOKEN)?,
            id(SEP_TOKEN)?,
            id(CLS_TOKEN)?,
            id(END_TOKEN)?,
        );
        if labels.is_empty() {
            bail!("persons.labels is empty");
        }
        let mut prompt = Vec::new();
        for label in labels {
            prompt.push(ent);
            prompt.extend(encode_word(&tokenizer, label)?);
        }
        prompt.push(sep);
        Ok(Gliner {
            session,
            tokenizer,
            prompt,
            cls,
            end,
            max_width,
            threshold,
        })
    }

    fn row(&self, text: &str, words: &[Word]) -> Result<Row> {
        let mut ids = vec![i64::from(self.cls)];
        ids.extend(self.prompt.iter().map(|&t| i64::from(t)));
        let mut mask = vec![0; ids.len()];
        for (index, word) in words.iter().enumerate() {
            for (n, token) in encode_word(&self.tokenizer, &text[word.start..word.end])?
                .into_iter()
                .enumerate()
            {
                ids.push(i64::from(token));
                mask.push(if n == 0 { index as i64 + 1 } else { 0 });
            }
        }
        ids.push(i64::from(self.end));
        mask.push(0);
        Ok(Row {
            ids,
            mask,
            words: words.len(),
        })
    }

    pub fn predict(&mut self, items: &[(&str, &[Word])]) -> Result<Vec<Vec<Span>>> {
        if items.is_empty() {
            return Ok(Vec::new());
        }
        let rows: Vec<Row> = items
            .iter()
            .map(|(text, words)| self.row(text, words))
            .collect::<Result<_>>()?;
        let batch = rows.len();
        let tokens = rows.iter().map(|r| r.ids.len()).max().unwrap_or(1);
        let columns = rows.iter().map(|r| r.words).max().unwrap_or(1).max(1);
        let mut ids = vec![0i64; batch * tokens];
        let mut attention = vec![0i64; batch * tokens];
        let mut words_mask = vec![0i64; batch * tokens];
        let mut lengths = Vec::with_capacity(batch);
        let mut span_index = Vec::new();
        let mut span_mask = Vec::new();
        for (r, row) in rows.iter().enumerate() {
            for (c, (&id, &word)) in row.ids.iter().zip(&row.mask).enumerate() {
                ids[r * tokens + c] = id;
                attention[r * tokens + c] = 1;
                words_mask[r * tokens + c] = word;
            }
            lengths.push(row.words as i64);
            let (index, mask) = span_inputs(row.words, self.max_width, columns);
            span_index.extend(index);
            span_mask.extend(mask);
        }
        let spans = columns * self.max_width;
        let inputs = ort::inputs![
            "input_ids" => Tensor::from_array(([batch, tokens], ids))?,
            "attention_mask" => Tensor::from_array(([batch, tokens], attention))?,
            "words_mask" => Tensor::from_array(([batch, tokens], words_mask))?,
            "text_lengths" => Tensor::from_array(([batch, 1], lengths))?,
            "span_idx" => Tensor::from_array(([batch, spans, 2], span_index))?,
            "span_mask" => Tensor::from_array(([batch, spans], span_mask))?,
        ];
        let outputs = self.session.run(inputs)?;
        let (dims, values) = outputs[LOGITS].try_extract_tensor::<f32>()?;
        let dims: Vec<usize> = dims.iter().map(|&d| d.max(0) as usize).collect();
        let [_, width, depth, labels] = dims[..] else {
            bail!("GLiNER logits have shape {dims:?}, not [batch, words, width, labels]");
        };
        let per_row = width * depth * labels;
        Ok(rows
            .iter()
            .enumerate()
            .map(|(r, row)| {
                let slice = &values[r * per_row..(r + 1) * per_row];
                decode(slice, row.words.min(width), depth, labels, self.threshold)
            })
            .collect())
    }
}

fn encode_word(tokenizer: &Tokenizer, word: &str) -> Result<Vec<u32>> {
    let encoding = tokenizer
        .encode(word, false)
        .map_err(|e| anyhow!("cannot tokenize {word:?}: {e}"))?;
    Ok(encoding.get_ids().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_split_on_spaces_and_symbols() {
        let text = "Jan de Vries, test-bank 3.";
        let found: Vec<&str> = words(text).iter().map(|w| &text[w.start..w.end]).collect();
        assert_eq!(found, ["Jan", "de", "Vries", ",", "test-bank", "3", "."]);
    }

    #[test]
    fn spans_cover_every_start_and_width_and_mask_the_overflow() {
        let (index, mask) = span_inputs(2, 3, 2);
        assert_eq!(index, [0, 0, 0, 1, 0, 2, 1, 1, 1, 2, 1, 3]);
        assert_eq!(mask, [true, true, false, true, false, false]);
    }

    fn logit(p: f32) -> f32 {
        (p / (1.0 - p)).ln()
    }

    #[test]
    fn decoding_keeps_the_best_spans_without_overlap() {
        let (words, width) = (4, 3);
        let mut logits = vec![logit(0.01); words * width];
        logits[0] = logit(0.6);
        logits[1] = logit(0.9);
        logits[2 * width + 1] = logit(0.7);
        logits[3 * width] = logit(0.4);
        logits[3 * width + 1] = logit(0.99);
        let spans = decode(&logits, words, width, 1, 0.5);
        let pairs: Vec<(usize, usize)> = spans.iter().map(|s| (s.first, s.last)).collect();
        assert_eq!(pairs, [(0, 1), (2, 3)]);
        assert!((spans[0].score - 0.9).abs() < 1e-4);
    }
}
