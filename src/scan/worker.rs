use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

use super::extract::{self, ExtractSettings, Page};
use crate::config::{ExtractConfig, OcrConfig};

pub const SUBCOMMAND: &str = "extract-worker";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Setup {
    pub extract: ExtractConfig,
    pub ocr: OcrConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub id: i64,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Pages(Vec<Page>),
    Error(String),
    Panic(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Answer {
    pub id: Option<i64>,
    pub result: Outcome,
}

pub async fn serve<R, W>(input: R, mut output: W) -> Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut lines = input.lines();
    let Some(first) = lines.next_line().await? else {
        bail!("extract worker: no setup line on stdin");
    };
    let setup: Setup =
        serde_json::from_str(&first).context("extract worker: invalid setup line")?;
    let settings = Arc::new(ExtractSettings::new(&setup.extract, &setup.ocr));
    while let Some(line) = lines.next_line().await? {
        let answer = match serde_json::from_str::<Request>(&line) {
            Ok(request) => {
                let settings = settings.clone();
                let path = request.path;
                Answer {
                    id: Some(request.id),
                    result: guarded(async move { extract::extract_pages(&path, &settings).await })
                        .await,
                }
            }
            Err(error) => Answer {
                id: None,
                result: Outcome::Error(format!("invalid request line: {error}")),
            },
        };
        let mut text = serde_json::to_string(&answer)?;
        text.push('\n');
        output.write_all(text.as_bytes()).await?;
        output.flush().await?;
        if matches!(answer.result, Outcome::Panic(_)) {
            break;
        }
    }
    Ok(())
}

async fn guarded<F>(future: F) -> Outcome
where
    F: Future<Output = Result<Vec<Page>>> + Send + 'static,
{
    match tokio::spawn(future).await {
        Ok(Ok(pages)) => Outcome::Pages(pages),
        Ok(Err(error)) => Outcome::Error(format!("{error:#}")),
        Err(error) if error.is_panic() => {
            let payload = error.into_panic();
            let message = payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".to_string());
            Outcome::Panic(message)
        }
        Err(error) => Outcome::Error(format!("extraction task failed: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::extract::Method;

    fn setup_line() -> String {
        let config = crate::config::load(None).unwrap();
        let setup = Setup {
            extract: config.extract,
            ocr: config.ocr,
        };
        serde_json::to_string(&setup).unwrap()
    }

    #[test]
    fn answer_round_trips() {
        let answer = Answer {
            id: Some(3),
            result: Outcome::Pages(vec![Page {
                page_no: 1,
                text: "line one\nline two".to_string(),
                method: Method::Ocr,
                ocr_confidence: Some(0.9),
            }]),
        };
        let text = serde_json::to_string(&answer).unwrap();
        assert!(!text.contains('\n'));
        assert_eq!(serde_json::from_str::<Answer>(&text).unwrap(), answer);
        let request = Request {
            id: 4,
            path: PathBuf::from("a/b.pdf"),
        };
        let text = serde_json::to_string(&request).unwrap();
        assert_eq!(serde_json::from_str::<Request>(&text).unwrap(), request);
    }

    #[tokio::test]
    async fn panic_in_the_extractor_becomes_an_answer() {
        let outcome = guarded(async { panic!("range end index 8 out of range") }).await;
        assert_eq!(
            outcome,
            Outcome::Panic("range end index 8 out of range".to_string())
        );
        let outcome = guarded(async { Err(anyhow::anyhow!("broken file")) }).await;
        assert_eq!(outcome, Outcome::Error("broken file".to_string()));
    }

    #[tokio::test]
    async fn bad_line_gets_an_error_and_the_worker_goes_on() {
        let dir = std::env::temp_dir().join(format!("zontologize-worker-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("note.txt");
        std::fs::write(&file, "made-up note").unwrap();
        let request = serde_json::to_string(&Request { id: 9, path: file }).unwrap();
        let input = format!("{}\nnot json\n{request}\n", setup_line());
        let mut output = Vec::new();
        serve(input.as_bytes(), &mut output).await.unwrap();
        let answers: Vec<Answer> = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(answers.len(), 2);
        assert_eq!(answers[0].id, None);
        assert!(matches!(answers[0].result, Outcome::Error(_)));
        assert_eq!(answers[1].id, Some(9));
        let Outcome::Pages(pages) = &answers[1].result else {
            panic!("expected pages: {:?}", answers[1]);
        };
        assert!(pages[0].text.contains("made-up note"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
