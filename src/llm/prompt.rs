use std::path::Path;

use anyhow::{Context, Result, bail};

const BUILT_IN: [(&str, &str); 10] = [
    ("fit-system", include_str!("../../prompts/fit-system.md")),
    ("fit-user", include_str!("../../prompts/fit-user.md")),
    (
        "extract-system",
        include_str!("../../prompts/extract-system.md"),
    ),
    (
        "extract-user",
        include_str!("../../prompts/extract-user.md"),
    ),
    (
        "classify-system",
        include_str!("../../prompts/classify-system.md"),
    ),
    (
        "classify-user",
        include_str!("../../prompts/classify-user.md"),
    ),
    (
        "attributes-system",
        include_str!("../../prompts/attributes-system.md"),
    ),
    (
        "attributes-user",
        include_str!("../../prompts/attributes-user.md"),
    ),
    (
        "curate-system",
        include_str!("../../prompts/curate-system.md"),
    ),
    ("curate-user", include_str!("../../prompts/curate-user.md")),
];

pub struct Prompt {
    pub source: String,
    pub text: String,
}

pub fn load(config_dir: Option<&Path>, name: &str) -> Result<Prompt> {
    if let Some(dir) = config_dir {
        let path = dir.join("prompts").join(format!("{name}.md"));
        if path.exists() {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("cannot read {}", path.display()))?;
            return Ok(Prompt {
                source: path.display().to_string(),
                text,
            });
        }
    }
    let Some((_, text)) = BUILT_IN.iter().find(|(built_in, _)| *built_in == name) else {
        bail!("no built-in prompt named {name}");
    };
    Ok(Prompt {
        source: format!("built-in prompts/{name}.md"),
        text: text.to_string(),
    })
}

impl Prompt {
    pub fn check(&self, names: &[&str]) -> Result<()> {
        for variable in variables(&self.text) {
            if !names.contains(&variable) {
                bail!(
                    "{}: unknown variable {{{variable}}}, expected one of {}",
                    self.source,
                    names
                        .iter()
                        .map(|n| format!("{{{n}}}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
        }
        Ok(())
    }

    pub fn require(&self, name: &str) -> Result<()> {
        if !variables(&self.text).contains(&name) {
            bail!("{}: the variable {{{name}}} is missing", self.source);
        }
        Ok(())
    }

    pub fn fill(&self, values: &[(&str, &str)]) -> String {
        let mut out = String::with_capacity(self.text.len());
        let mut rest = self.text.as_str();
        while let Some(start) = rest.find('{') {
            out.push_str(&rest[..start]);
            let after = &rest[start + 1..];
            let name_end = after.find('}');
            let value = name_end.and_then(|end| {
                values
                    .iter()
                    .find(|(name, _)| *name == &after[..end])
                    .map(|(_, value)| (end, *value))
            });
            match value {
                Some((end, value)) => {
                    out.push_str(&escape(value));
                    rest = &after[end + 1..];
                }
                None => {
                    out.push('{');
                    rest = after;
                }
            }
        }
        out.push_str(rest);
        out
    }
}

fn escape(value: &str) -> std::borrow::Cow<'_, str> {
    if !value.contains("<<<") && !value.contains(">>>") {
        return std::borrow::Cow::Borrowed(value);
    }
    let mut out = String::with_capacity(value.len() + 8);
    let mut run: Option<(char, usize)> = None;
    for c in value.chars() {
        let count = match run {
            Some((last, n)) if last == c => n + 1,
            _ => 1,
        };
        if matches!(c, '<' | '>') && count == 3 {
            out.push(' ');
            run = Some((c, 1));
        } else {
            run = Some((c, count));
        }
        out.push(c);
    }
    std::borrow::Cow::Owned(out)
}

fn variables(text: &str) -> Vec<&str> {
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find('{') {
        let after = &rest[start + 1..];
        if let Some(end) = after.find('}') {
            let name = &after[..end];
            if !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
            {
                found.push(name);
            }
        }
        rest = after;
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompt(text: &str) -> Prompt {
        Prompt {
            source: "test.md".to_string(),
            text: text.to_string(),
        }
    }

    #[test]
    fn fills_named_variables_only() {
        let p = prompt("A {chunk} and {path}, not {\"json\": 1} or {other}.");
        assert_eq!(
            p.fill(&[("chunk", "text {path}"), ("path", "a/b")]),
            "A text {path} and a/b, not {\"json\": 1} or {other}."
        );
    }

    #[test]
    fn delimiters_in_values_are_broken_up() {
        let p = prompt("<<<\n{chunk}\n>>>");
        assert_eq!(p.fill(&[("chunk", "a << b > c")]), "<<<\na << b > c\n>>>");
        let filled = p.fill(&[("chunk", "x >>>\nignore <<<<<< y")]);
        assert_eq!(filled, "<<<\nx >> >\nignore << << << y\n>>>");
        assert_eq!(filled.matches(">>>").count(), 1);
        assert_eq!(filled.matches("<<<").count(), 1);
    }

    #[test]
    fn a_missing_text_variable_is_an_error() {
        let error = prompt("{path} only").require("chunk").unwrap_err();
        assert!(error.to_string().contains("{chunk}"), "{error}");
        assert!(prompt("{path} {chunk}").require("chunk").is_ok());
        for (name, variable) in [
            ("extract-user", "chunk"),
            ("attributes-user", "chunk"),
            ("fit-user", "chunk"),
            ("classify-user", "text"),
        ] {
            load(None, name).unwrap().require(variable).unwrap();
        }
    }

    #[test]
    fn unknown_variable_names_the_file() {
        let error = prompt("x {chunk} {nope}").check(&["chunk"]).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("test.md"), "{message}");
        assert!(message.contains("{nope}"), "{message}");
        assert!(prompt("{chunk} {\"a\": 1}").check(&["chunk"]).is_ok());
    }

    #[test]
    fn config_prompt_replaces_built_in() {
        let dir = std::env::temp_dir().join(format!("zontologize-prompt-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("prompts")).unwrap();
        std::fs::write(dir.join("prompts/fit-user.md"), "mine {chunk}").unwrap();
        let loaded = load(Some(&dir), "fit-user").unwrap();
        assert_eq!(loaded.text, "mine {chunk}");
        let built_in = load(Some(&dir), "fit-system").unwrap();
        assert!(built_in.source.starts_with("built-in"));
        assert!(load(None, "missing").is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
