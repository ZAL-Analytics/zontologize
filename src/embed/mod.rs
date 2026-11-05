pub mod eval;
pub mod local;
pub mod model;
pub mod run;

use anyhow::{Result, bail};

pub const STAGE_VERSION: u32 = 1;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Embedded {
    pub vectors: Vec<Vec<f32>>,
    pub truncated: Vec<bool>,
}

pub trait Embed: Send {
    fn revision(&self) -> &str;
    fn sha256(&mut self) -> Result<String>;
    fn dimensions(&mut self) -> Result<usize>;
    fn embed(&mut self, texts: &[String]) -> Result<Embedded>;
}

pub fn passage(model: &model::Model, text: &str) -> String {
    format!("{}{text}", model.settings.passage_prefix)
}

pub fn query(model: &model::Model, text: &str) -> String {
    format!("{}{text}", model.settings.query_prefix)
}

pub fn embed_sorted(
    embedder: &mut dyn Embed,
    texts: &[String],
    batch_size: usize,
) -> Result<Embedded> {
    let mut order: Vec<usize> = (0..texts.len()).collect();
    order.sort_by_key(|&i| texts[i].chars().count());
    let mut vectors = vec![Vec::new(); texts.len()];
    let mut truncated = vec![false; texts.len()];
    for batch in order.chunks(batch_size.max(1)) {
        let inputs: Vec<String> = batch.iter().map(|&i| texts[i].clone()).collect();
        let out = embedder.embed(&inputs)?;
        if out.vectors.len() != batch.len() || out.truncated.len() != batch.len() {
            bail!(
                "the model gave {} vectors for {} texts",
                out.vectors.len(),
                batch.len()
            );
        }
        for ((&i, vector), cut) in batch.iter().zip(out.vectors).zip(out.truncated) {
            vectors[i] = vector;
            truncated[i] = cut;
        }
    }
    Ok(Embedded { vectors, truncated })
}

#[cfg(test)]
pub mod fake {
    use super::{Embed, Embedded};
    use anyhow::Result;
    use sha2::{Digest, Sha256};

    pub struct Fake {
        pub dimensions: usize,
        pub calls: Vec<usize>,
        pub max_chars: usize,
    }

    impl Fake {
        pub fn new(dimensions: usize) -> Fake {
            Fake {
                dimensions,
                calls: Vec::new(),
                max_chars: usize::MAX,
            }
        }
    }

    pub fn vector(text: &str, dimensions: usize) -> Vec<f32> {
        let digest = Sha256::digest(text.as_bytes());
        let mut vector: Vec<f32> = (0..dimensions)
            .map(|i| digest[i % digest.len()] as f32 - 127.5)
            .collect();
        super::local::normalise(&mut vector);
        vector
    }

    impl Embed for Fake {
        fn revision(&self) -> &str {
            "fake"
        }

        fn sha256(&mut self) -> Result<String> {
            Ok("fake".to_string())
        }

        fn dimensions(&mut self) -> Result<usize> {
            Ok(self.dimensions)
        }

        fn embed(&mut self, texts: &[String]) -> Result<Embedded> {
            self.calls.push(texts.len());
            Ok(Embedded {
                vectors: texts.iter().map(|t| vector(t, self.dimensions)).collect(),
                truncated: texts
                    .iter()
                    .map(|t| t.chars().count() > self.max_chars)
                    .collect(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order_is_kept_after_the_length_sort() {
        let texts: Vec<String> = ["ccc", "a", "bb", "dddd", "e"]
            .iter()
            .map(|t| t.to_string())
            .collect();
        let mut fake = fake::Fake::new(4);
        fake.max_chars = 2;
        let out = embed_sorted(&mut fake, &texts, 2).unwrap();
        assert_eq!(fake.calls, [2, 2, 1]);
        for (text, vector) in texts.iter().zip(&out.vectors) {
            assert_eq!(vector, &fake::vector(text, 4));
        }
        assert_eq!(out.truncated, [true, false, false, true, false]);
    }

    #[test]
    fn prefix_goes_on_passages_and_queries_separately() {
        let settings: crate::config::Config = crate::config::load(None).unwrap();
        let model = model::parse("local:multilingual-e5-base", &settings.embed.models).unwrap();
        assert_eq!(passage(&model, "pump"), "passage: pump");
        assert_eq!(query(&model, "pump"), "query: pump");
    }
}
