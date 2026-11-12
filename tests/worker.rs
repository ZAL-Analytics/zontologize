use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use zontologize::config;
use zontologize::scan::worker::{Answer, Outcome, Request, SUBCOMMAND, Setup};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/extract")
        .join(name)
}

#[test]
fn worker_process_extracts_and_exits_when_stdin_closes() {
    let config = config::load(None).unwrap();
    let setup = Setup {
        extract: config.extract,
        ocr: config.ocr,
        gpu: config.gpu,
    };
    let mut child = Command::new(env!("CARGO_BIN_EXE_zontologize"))
        .arg(SUBCOMMAND)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    writeln!(stdin, "{}", serde_json::to_string(&setup).unwrap()).unwrap();
    for (id, name) in [(1, "note.txt"), (2, "letter.docx")] {
        let request = Request {
            id,
            path: fixture(name),
        };
        writeln!(stdin, "{}", serde_json::to_string(&request).unwrap()).unwrap();
    }
    drop(stdin);
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    let answers: Vec<Answer> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(answers.len(), 2);
    for (answer, id) in answers.iter().zip([1, 2]) {
        assert_eq!(answer.id, Some(id));
        let Outcome::Pages(pages) = &answer.result else {
            panic!("expected pages: {answer:?}");
        };
        assert!(!pages.is_empty());
    }
}
