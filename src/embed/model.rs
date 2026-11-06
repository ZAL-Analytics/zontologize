use std::collections::BTreeMap;

use sha2::{Digest, Sha256};

use crate::config::EmbedModelConfig;
use crate::hash::hex;

pub const PROVIDER: &str = "local";

#[derive(Debug, Clone, PartialEq)]
pub struct Model {
    pub name: String,
    pub settings: EmbedModelConfig,
}

impl Model {
    pub fn full_name(&self) -> String {
        format!("{PROVIDER}:{}", self.name)
    }

    pub fn fingerprint(&self, revision: &str) -> String {
        let s = &self.settings;
        let mut hasher = Sha256::new();
        let max_tokens = s.max_tokens.to_string();
        let normalize = s.normalize.to_string();
        let data_files = s.data_files.join("\n");
        for part in [
            self.name.as_str(),
            &s.repo,
            revision,
            &s.file,
            &data_files,
            &s.tokenizer,
            s.pooling.as_str(),
            &s.output,
            &normalize,
            &s.passage_prefix,
            &s.query_prefix,
            &max_tokens,
        ] {
            hasher.update(part.as_bytes());
            hasher.update([0]);
        }
        hex(&hasher.finalize())
    }
}

pub fn parse(name: &str, models: &BTreeMap<String, EmbedModelConfig>) -> Result<Model, String> {
    let Some((provider, model)) = name.split_once(':') else {
        return Err(format!("{name:?} must be {PROVIDER}:<name>"));
    };
    if provider != PROVIDER {
        return Err(format!(
            "{name:?}: embeddings run locally only, the provider must be {PROVIDER}"
        ));
    }
    let Some(settings) = models.get(model) else {
        return Err(format!(
            "{name:?}: no [embed.models.\"{model}\"] entry; known models: {}",
            models.keys().cloned().collect::<Vec<_>>().join(", ")
        ));
    };
    Ok(Model {
        name: model.to_string(),
        settings: settings.clone(),
    })
}

pub fn text_key(fingerprint: &str, stage_version: u32, text: &str) -> String {
    crate::hash::stage_hash(
        &[fingerprint.as_bytes(), &[0]],
        stage_version,
        text.as_bytes(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Pooling;

    fn settings() -> EmbedModelConfig {
        EmbedModelConfig {
            repo: "example/model".to_string(),
            revision: "abc".to_string(),
            file: "onnx/model.onnx".to_string(),
            data_files: Vec::new(),
            tokenizer: "tokenizer.json".to_string(),
            pooling: Pooling::Mean,
            output: String::new(),
            normalize: true,
            passage_prefix: "passage: ".to_string(),
            query_prefix: "query: ".to_string(),
            max_tokens: 512,
        }
    }

    fn models() -> BTreeMap<String, EmbedModelConfig> {
        BTreeMap::from([("small".to_string(), settings())])
    }

    #[test]
    fn names_parse() {
        let model = parse("local:small", &models()).unwrap();
        assert_eq!(model.name, "small");
        assert_eq!(model.full_name(), "local:small");
        assert!(
            parse("small", &models())
                .unwrap_err()
                .contains("local:<name>")
        );
        assert!(
            parse("ollama:small", &models())
                .unwrap_err()
                .contains("locally only")
        );
        let error = parse("local:big", &models()).unwrap_err();
        assert!(error.contains("big") && error.contains("small"), "{error}");
    }

    #[test]
    fn text_key_changes_with_every_part() {
        let model = parse("local:small", &models()).unwrap();
        let base = model.fingerprint("abc");
        let key = text_key(&base, 1, "text");
        assert_eq!(key, text_key(&base, 1, "text"));
        assert_ne!(key, text_key(&base, 2, "text"));
        assert_ne!(key, text_key(&base, 1, "other"));
        assert_ne!(key, text_key(&model.fingerprint("def"), 1, "text"));
        let changes: [fn(&mut EmbedModelConfig); 4] = [
            |s| s.passage_prefix = "p: ".to_string(),
            |s| s.pooling = Pooling::Cls,
            |s| s.max_tokens = 256,
            |s| s.file = "onnx/other.onnx".to_string(),
        ];
        for change in changes {
            let mut other = model.clone();
            change(&mut other.settings);
            assert_ne!(key, text_key(&other.fingerprint("abc"), 1, "text"));
        }
        let mut renamed = model.clone();
        renamed.name = "other".to_string();
        assert_ne!(base, renamed.fingerprint("abc"));
    }
}
