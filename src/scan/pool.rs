use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::Serialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinSet;

use super::extract::Page;
use super::memory;
use super::worker::{Answer, Outcome, Request};

const CHECK_EVERY: Duration = Duration::from_millis(250);
const STOP_GRACE: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct WorkerCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub timeout: Duration,
    pub memory_mb: u64,
    pub files: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lost {
    Memory,
    Crash,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Done {
    Pages(Vec<Page>),
    Error(String),
    Panic(String),
    Timeout,
    Lost(Lost),
}

#[derive(Debug)]
pub struct Job {
    pub id: i64,
    pub path: PathBuf,
}

#[derive(Debug)]
pub struct Finished {
    pub id: i64,
    pub path: PathBuf,
    pub done: Done,
    pub seconds: f64,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Restarts {
    pub timeout: u64,
    pub memory: u64,
    pub crash: u64,
    pub panic: u64,
    pub files: u64,
}

impl Restarts {
    pub fn add(&mut self, other: Restarts) {
        self.timeout += other.timeout;
        self.memory += other.memory;
        self.crash += other.crash;
        self.panic += other.panic;
        self.files += other.files;
    }
}

pub struct Pool {
    jobs: Option<mpsc::Sender<Job>>,
    finished: mpsc::UnboundedReceiver<Finished>,
    tasks: JoinSet<Result<Restarts>>,
}

impl Pool {
    pub fn start(command: &WorkerCommand, setup: String, workers: usize, limits: Limits) -> Pool {
        let workers = workers.max(1);
        let (jobs, receiver) = mpsc::channel(workers);
        let receiver = Arc::new(Mutex::new(receiver));
        let (sender, finished) = mpsc::unbounded_channel();
        let mut tasks = JoinSet::new();
        for _ in 0..workers {
            tasks.spawn(work(
                command.clone(),
                setup.clone(),
                receiver.clone(),
                sender.clone(),
                limits,
            ));
        }
        Pool {
            jobs: Some(jobs),
            finished,
            tasks,
        }
    }

    pub async fn submit(&mut self, job: Job) -> Result<bool> {
        let Some(jobs) = &self.jobs else {
            return Ok(false);
        };
        Ok(jobs.send(job).await.is_ok())
    }

    pub fn try_finished(&mut self) -> Option<Finished> {
        self.finished.try_recv().ok()
    }

    pub async fn next_finished(&mut self) -> Option<Finished> {
        self.jobs = None;
        self.finished.recv().await
    }

    pub async fn join(mut self) -> Result<Restarts> {
        self.jobs = None;
        let mut total = Restarts::default();
        while let Some(result) = self.tasks.join_next().await {
            total.add(result.context("extract worker task failed")??);
        }
        Ok(total)
    }
}

async fn work(
    command: WorkerCommand,
    setup: String,
    jobs: Arc<Mutex<mpsc::Receiver<Job>>>,
    finished: mpsc::UnboundedSender<Finished>,
    limits: Limits,
) -> Result<Restarts> {
    let mut restarts = Restarts::default();
    let mut process: Option<Process> = None;
    let mut served = 0;
    loop {
        let job = jobs.lock().await.recv().await;
        let Some(job) = job else {
            break;
        };
        if served >= limits.files.max(1)
            && let Some(old) = process.take()
        {
            old.stop().await;
            restarts.files += 1;
            served = 0;
        }
        let running = match process.as_mut() {
            Some(running) => running,
            None => process.insert(Process::spawn(&command, &setup).await?),
        };
        let started = Instant::now();
        let done = running.run(&job, &limits).await;
        let seconds = started.elapsed().as_secs_f64();
        served += 1;
        let killed = match done {
            Done::Timeout => Some(&mut restarts.timeout),
            Done::Lost(Lost::Memory) => Some(&mut restarts.memory),
            Done::Lost(Lost::Crash) => Some(&mut restarts.crash),
            Done::Panic(_) => Some(&mut restarts.panic),
            Done::Pages(_) | Done::Error(_) => None,
        };
        if let Some(counter) = killed {
            *counter += 1;
            if let Some(old) = process.take() {
                old.kill().await;
            }
            served = 0;
        }
        let item = Finished {
            id: job.id,
            path: job.path,
            done,
            seconds,
        };
        if finished.send(item).is_err() {
            break;
        }
    }
    if let Some(old) = process {
        old.stop().await;
    }
    Ok(restarts)
}

struct Process {
    child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
    pid: Option<u32>,
}

impl Process {
    async fn spawn(command: &WorkerCommand, setup: &str) -> Result<Process> {
        let mut child = Command::new(&command.program)
            .args(&command.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| {
                format!("cannot start extract worker {}", command.program.display())
            })?;
        let pid = child.id();
        let mut stdin = child.stdin.take().context("extract worker has no stdin")?;
        let stdout = child
            .stdout
            .take()
            .context("extract worker has no stdout")?;
        let line = format!("{setup}\n");
        stdin
            .write_all(line.as_bytes())
            .await
            .context("cannot send the setup to an extract worker")?;
        Ok(Process {
            child,
            stdin,
            stdout: BufReader::new(stdout).lines(),
            pid,
        })
    }

    async fn run(&mut self, job: &Job, limits: &Limits) -> Done {
        let request = Request {
            id: job.id,
            path: job.path.clone(),
        };
        let Ok(mut line) = serde_json::to_string(&request) else {
            return Done::Error(format!("cannot encode request for {}", job.path.display()));
        };
        line.push('\n');
        if self.stdin.write_all(line.as_bytes()).await.is_err() || self.stdin.flush().await.is_err()
        {
            return Done::Lost(Lost::Crash);
        }
        let deadline = tokio::time::sleep(limits.timeout);
        tokio::pin!(deadline);
        let mut tick = tokio::time::interval(CHECK_EVERY);
        loop {
            tokio::select! {
                read = self.stdout.next_line() => {
                    let Ok(Some(text)) = read else {
                        return Done::Lost(Lost::Crash);
                    };
                    match serde_json::from_str::<Answer>(&text) {
                        Ok(Answer { id: Some(id), result }) if id == job.id => {
                            return match result {
                                Outcome::Pages(pages) => Done::Pages(pages),
                                Outcome::Error(message) => Done::Error(message),
                                Outcome::Panic(message) => Done::Panic(message),
                            };
                        }
                        _ => eprintln!("extract worker: {text}"),
                    }
                }
                _ = &mut deadline => return Done::Timeout,
                _ = tick.tick() => {
                    let used = self.pid.and_then(memory::pid_mb);
                    if used.is_some_and(|mb| mb > limits.memory_mb) {
                        return Done::Lost(Lost::Memory);
                    }
                }
            }
        }
    }

    async fn kill(mut self) {
        let _ = self.child.kill().await;
    }

    async fn stop(mut self) {
        drop(self.stdin);
        if tokio::time::timeout(STOP_GRACE, self.child.wait())
            .await
            .is_err()
        {
            let _ = self.child.kill().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shell(script: &str) -> WorkerCommand {
        WorkerCommand {
            program: PathBuf::from("sh"),
            args: vec!["-c".to_string(), script.to_string()],
        }
    }

    fn limits(timeout_ms: u64, files: usize) -> Limits {
        Limits {
            timeout: Duration::from_millis(timeout_ms),
            memory_mb: u64::MAX,
            files,
        }
    }

    const ECHO_PAGES: &str = r#"read setup; while read line; do id=$(echo "$line" | sed 's/.*"id":\([0-9]*\).*/\1/'); echo "{\"id\":$id,\"result\":{\"pages\":[]}}"; done"#;

    async fn run_all(pool: &mut Pool, ids: &[i64]) -> Vec<Finished> {
        for id in ids {
            let job = Job {
                id: *id,
                path: PathBuf::from(format!("file-{id}")),
            };
            assert!(pool.submit(job).await.unwrap());
        }
        let mut done = Vec::new();
        while let Some(item) = pool.next_finished().await {
            done.push(item);
        }
        done.sort_by_key(|f| f.id);
        done
    }

    #[tokio::test]
    async fn answers_come_back_per_job() {
        let mut pool = Pool::start(&shell(ECHO_PAGES), "{}".to_string(), 2, limits(5000, 100));
        let done = run_all(&mut pool, &[1, 2, 3]).await;
        assert_eq!(done.len(), 3);
        assert!(done.iter().all(|f| f.done == Done::Pages(Vec::new())));
        assert_eq!(pool.join().await.unwrap(), Restarts::default());
    }

    #[tokio::test]
    async fn slow_worker_is_killed_on_timeout() {
        let mut pool = Pool::start(
            &shell("read setup; read line; sleep 30"),
            "{}".to_string(),
            1,
            limits(200, 100),
        );
        let started = Instant::now();
        let done = run_all(&mut pool, &[1]).await;
        assert_eq!(done[0].done, Done::Timeout);
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(pool.join().await.unwrap().timeout, 1);
    }

    #[tokio::test]
    async fn crashed_worker_is_replaced() {
        let mut pool = Pool::start(
            &shell("read setup; read line; exit 1"),
            "{}".to_string(),
            1,
            limits(5000, 100),
        );
        let done = run_all(&mut pool, &[1, 2]).await;
        assert!(done.iter().all(|f| f.done == Done::Lost(Lost::Crash)));
        assert_eq!(pool.join().await.unwrap().crash, 2);
    }

    #[tokio::test]
    async fn worker_over_memory_is_killed() {
        let mut pool = Pool::start(
            &shell("read setup; read line; sleep 30"),
            "{}".to_string(),
            1,
            Limits {
                timeout: Duration::from_secs(20),
                memory_mb: 0,
                files: 100,
            },
        );
        let done = run_all(&mut pool, &[1]).await;
        assert_eq!(done[0].done, Done::Lost(Lost::Memory));
        assert_eq!(pool.join().await.unwrap().memory, 1);
    }

    #[tokio::test]
    async fn panic_answer_is_an_error_and_replaces_the_worker() {
        const PANICS: &str = r#"read setup; read line; id=$(echo "$line" | sed 's/.*"id":\([0-9]*\).*/\1/'); echo "{\"id\":$id,\"result\":{\"panic\":\"bad sheet\"}}""#;
        let mut pool = Pool::start(&shell(PANICS), "{}".to_string(), 1, limits(5000, 100));
        let done = run_all(&mut pool, &[1, 2]).await;
        assert!(
            done.iter()
                .all(|f| f.done == Done::Panic("bad sheet".to_string()))
        );
        let restarts = pool.join().await.unwrap();
        assert_eq!((restarts.panic, restarts.crash), (2, 0));
    }

    #[tokio::test]
    async fn worker_is_replaced_after_its_file_limit() {
        let mut pool = Pool::start(&shell(ECHO_PAGES), "{}".to_string(), 1, limits(5000, 2));
        let done = run_all(&mut pool, &[1, 2, 3, 4, 5]).await;
        assert_eq!(done.len(), 5);
        assert_eq!(pool.join().await.unwrap().files, 2);
    }

    #[tokio::test]
    async fn missing_program_is_an_error() {
        let command = WorkerCommand {
            program: PathBuf::from("/nonexistent/zontologize-worker"),
            args: Vec::new(),
        };
        let mut pool = Pool::start(&command, "{}".to_string(), 1, limits(5000, 100));
        let _ = pool
            .submit(Job {
                id: 1,
                path: PathBuf::from("a"),
            })
            .await;
        while pool.next_finished().await.is_some() {}
        let error = pool.join().await.unwrap_err();
        assert!(format!("{error:#}").contains("cannot start extract worker"));
    }
}
