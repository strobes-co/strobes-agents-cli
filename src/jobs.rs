//! Local command jobs: live output, interrupt, and "run in background".
//!
//! Every local `execute_command` / `execute_code` (and the user's `!cmd`)
//! runs through [`run_streaming`], which
//!   * streams stdout/stderr line by line to the UI (the live tail under a
//!     running `⏺ Shell(…)` call),
//!   * can be **backgrounded** (Ctrl-B): the caller gets the output so far plus
//!     a note naming the job's log file right away, while the process keeps
//!     running and writes the rest to that log,
//!   * can be **killed** (Esc / Ctrl-C interrupt, `/jobs` → x), together with
//!     every child it spawned (it runs in its own process group).
//!
//! The registry is process-global so the UI can list and control jobs that
//! were started from a WebSocket task.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::oneshot;

/// What the UI hears about jobs.
#[derive(Debug, Clone)]
pub enum JobEvent {
    /// A new output line from a running job (keyed by the tool call's id).
    Line { call_id: String, line: String },
    /// A backgrounded job finished.
    Done { job: u32, exit: Option<i32>, command: String },
}

pub type Sink = Arc<dyn Fn(JobEvent) + Send + Sync>;

/// How a run is reported: which tool call it belongs to and where events go.
#[derive(Clone)]
pub struct RunCtx {
    pub call_id: String,
    pub label: String,
    pub sink: Sink,
}

#[derive(Debug, Clone)]
pub struct JobInfo {
    pub id: u32,
    pub call_id: String,
    pub command: String,
    pub started: Instant,
    pub finished: Option<Instant>,
    pub exit: Option<i32>,
    pub background: bool,
    pub stopped: bool,
    pub log: Option<PathBuf>,
}

impl JobInfo {
    pub fn running(&self) -> bool {
        self.finished.is_none()
    }
}

struct Ctl {
    detach: Option<oneshot::Sender<()>>,
    kill: Option<oneshot::Sender<()>>,
}

#[derive(Default)]
struct Registry {
    next: u32,
    jobs: Vec<JobInfo>,
    ctl: HashMap<u32, Ctl>,
}

fn reg() -> &'static Mutex<Registry> {
    static R: std::sync::OnceLock<Mutex<Registry>> = std::sync::OnceLock::new();
    R.get_or_init(|| Mutex::new(Registry::default()))
}

/// Snapshot of all jobs, newest first.
pub fn list() -> Vec<JobInfo> {
    let mut v = reg().lock().unwrap().jobs.clone();
    v.reverse();
    v
}

/// Running jobs the agent is still waiting on (not backgrounded).
pub fn foreground_running() -> usize {
    reg().lock().unwrap().jobs.iter().filter(|j| j.running() && !j.background).count()
}

pub fn background_running() -> usize {
    reg().lock().unwrap().jobs.iter().filter(|j| j.running() && j.background).count()
}

/// Ctrl-B: move the newest foreground job to the background. Returns its id.
pub fn detach_latest() -> Option<u32> {
    let mut r = reg().lock().unwrap();
    let id = r.jobs.iter().rev().find(|j| j.running() && !j.background).map(|j| j.id)?;
    let tx = r.ctl.get_mut(&id).and_then(|c| c.detach.take())?;
    let _ = tx.send(());
    Some(id)
}

/// Stop one job (and its children).
pub fn kill(id: u32) -> bool {
    let mut r = reg().lock().unwrap();
    match r.ctl.get_mut(&id).and_then(|c| c.kill.take()) {
        Some(tx) => tx.send(()).is_ok(),
        None => false,
    }
}

/// Interrupt: stop every job the agent is waiting on.
pub fn kill_foreground() -> usize {
    let ids: Vec<u32> = reg().lock().unwrap().jobs.iter().filter(|j| j.running() && !j.background).map(|j| j.id).collect();
    ids.into_iter().filter(|id| kill(*id)).count()
}

/// Quit: stop everything still running.
pub fn kill_all() -> usize {
    let ids: Vec<u32> = reg().lock().unwrap().jobs.iter().filter(|j| j.running()).map(|j| j.id).collect();
    ids.into_iter().filter(|id| kill(*id)).count()
}

/// Last `n` lines of a job's log (backgrounded jobs), for `/jobs`.
pub fn tail(id: u32, n: usize) -> Vec<String> {
    let log = reg().lock().unwrap().jobs.iter().find(|j| j.id == id).and_then(|j| j.log.clone());
    let Some(log) = log else { return Vec::new() };
    let text = std::fs::read_to_string(log).unwrap_or_default();
    let lines: Vec<String> = text.lines().map(String::from).collect();
    lines[lines.len().saturating_sub(n)..].to_vec()
}

fn update(id: u32, f: impl FnOnce(&mut JobInfo)) {
    if let Some(j) = reg().lock().unwrap().jobs.iter_mut().find(|j| j.id == id) {
        f(j);
    }
}

/// Kill a process group (the job and everything it started).
fn kill_group(pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = pid {
        unsafe {
            libc::kill(-(pid as i32), libc::SIGTERM);
        }
        let p = pid as i32;
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(1500));
            unsafe {
                libc::kill(-p, libc::SIGKILL);
            }
        });
    }
    #[cfg(not(unix))]
    let _ = pid;
}

/// Result of a streamed run.
pub struct RunOutput {
    pub output: String,
    pub exit_code: Option<i32>,
    pub error: Option<String>,
}

/// Run `cmd` with live output, background and kill support.
pub async fn run_streaming(mut cmd: Command, ctx: Option<RunCtx>, sandbox: &Path) -> RunOutput {
    use std::process::Stdio;
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).stdin(Stdio::null());
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return RunOutput { output: String::new(), exit_code: None, error: Some(format!("spawn failed: {e}")) },
    };
    let pid = child.id();
    let (detach_tx, mut detach_rx) = oneshot::channel::<()>();
    let (kill_tx, mut kill_rx) = oneshot::channel::<()>();
    let label = ctx.as_ref().map(|c| c.label.clone()).unwrap_or_default();
    let call_id = ctx.as_ref().map(|c| c.call_id.clone()).unwrap_or_default();
    let id = {
        let mut r = reg().lock().unwrap();
        r.next += 1;
        let id = r.next;
        r.jobs.push(JobInfo {
            id,
            call_id: call_id.clone(),
            command: label.clone(),
            started: Instant::now(),
            finished: None,
            exit: None,
            background: false,
            stopped: false,
            log: None,
        });
        r.ctl.insert(id, Ctl { detach: Some(detach_tx), kill: Some(kill_tx) });
        // Keep the registry from growing without bound in long sessions.
        if r.jobs.len() > 200 {
            let drop_ids: Vec<u32> = r.jobs.iter().filter(|j| !j.running()).take(50).map(|j| j.id).collect();
            r.jobs.retain(|j| !drop_ids.contains(&j.id));
            for d in drop_ids {
                r.ctl.remove(&d);
            }
        }
        id
    };

    // Both streams feed one channel so lines keep their arrival order.
    let (line_tx, mut line_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    if let Some(out) = child.stdout.take() {
        let tx = line_tx.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(out).lines();
            while let Ok(Some(l)) = lines.next_line().await {
                if tx.send(l).is_err() {
                    break;
                }
            }
        });
    }
    if let Some(err) = child.stderr.take() {
        let tx = line_tx.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(err).lines();
            while let Ok(Some(l)) = lines.next_line().await {
                if tx.send(l).is_err() {
                    break;
                }
            }
        });
    }
    drop(line_tx);

    let mut buf = String::new();
    let emit = |line: &str| {
        if let Some(c) = &ctx {
            (c.sink)(JobEvent::Line { call_id: c.call_id.clone(), line: line.to_string() });
        }
    };
    let mut streams_open = true;
    loop {
        tokio::select! {
            l = line_rx.recv(), if streams_open => match l {
                Some(l) => {
                    emit(&l);
                    buf.push_str(&l);
                    buf.push('\n');
                }
                None => streams_open = false,
            },
            status = child.wait(), if !streams_open => {
                let exit = status.ok().and_then(|s| s.code());
                update(id, |j| { j.finished = Some(Instant::now()); j.exit = exit; });
                reg().lock().unwrap().ctl.remove(&id);
                return RunOutput { output: buf.trim_end().to_string(), exit_code: exit, error: None };
            }
            _ = &mut kill_rx => {
                kill_group(pid);
                let exit = child.wait().await.ok().and_then(|s| s.code());
                // Drain what was already buffered.
                while let Ok(l) = line_rx.try_recv() {
                    buf.push_str(&l);
                    buf.push('\n');
                }
                update(id, |j| { j.finished = Some(Instant::now()); j.exit = exit; j.stopped = true; });
                reg().lock().unwrap().ctl.remove(&id);
                let mut out = buf.trim_end().to_string();
                out.push_str("\n[stopped by the user]");
                return RunOutput { output: out, exit_code: exit, error: None };
            }
            _ = &mut detach_rx => {
                // Background: hand back what we have, keep running into a log.
                let dir = sandbox.join(".strobes-jobs");
                let _ = std::fs::create_dir_all(&dir);
                let log = dir.join(format!("job-{id}.log"));
                let mut file = match tokio::fs::File::create(&log).await {
                    Ok(f) => Some(f),
                    Err(_) => None,
                };
                if let Some(f) = file.as_mut() {
                    let _ = f.write_all(buf.as_bytes()).await;
                }
                update(id, |j| { j.background = true; j.log = Some(log.clone()); });
                let sink = ctx.as_ref().map(|c| c.sink.clone());
                let command = label.clone();
                tokio::spawn(async move {
                    let mut child = child;
                    let mut streams_open = true;
                    loop {
                        tokio::select! {
                            l = line_rx.recv(), if streams_open => match l {
                                Some(l) => {
                                    if let Some(f) = file.as_mut() {
                                        let _ = f.write_all(format!("{l}\n").as_bytes()).await;
                                    }
                                }
                                None => streams_open = false,
                            },
                            status = child.wait(), if !streams_open => {
                                let exit = status.ok().and_then(|s| s.code());
                                update(id, |j| { j.finished = Some(Instant::now()); j.exit = exit; });
                                reg().lock().unwrap().ctl.remove(&id);
                                if let Some(s) = &sink { s(JobEvent::Done { job: id, exit, command: command.clone() }); }
                                break;
                            }
                            _ = &mut kill_rx => {
                                kill_group(pid);
                                let exit = child.wait().await.ok().and_then(|s| s.code());
                                if let Some(f) = file.as_mut() {
                                    let _ = f.write_all(b"[stopped by the user]\n").await;
                                }
                                update(id, |j| { j.finished = Some(Instant::now()); j.exit = exit; j.stopped = true; });
                                reg().lock().unwrap().ctl.remove(&id);
                                if let Some(s) = &sink { s(JobEvent::Done { job: id, exit, command: command.clone() }); }
                                break;
                            }
                        }
                    }
                });
                let mut out = buf.trim_end().to_string();
                if !out.is_empty() {
                    out.push_str("\n\n");
                }
                out.push_str(&format!(
                    "[The user moved this command to the background — it is still running as job #{id}. \
                     Its full output is being written to {}. Continue with other work; read that file \
                     (e.g. tail it) to check progress or results later.]",
                    log.display()
                ));
                return RunOutput { output: out, exit_code: None, error: None };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(cmd: &str) -> Command {
        let mut c = Command::new("/bin/sh");
        c.arg("-c").arg(cmd);
        c
    }

    fn ctx(lines: Arc<Mutex<Vec<String>>>) -> RunCtx {
        RunCtx {
            call_id: "c1".into(),
            label: "test".into(),
            sink: Arc::new(move |e| {
                if let JobEvent::Line { line, .. } = e {
                    lines.lock().unwrap().push(line);
                }
            }),
        }
    }

    #[tokio::test]
    async fn streams_lines_and_returns_output() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let dir = std::env::temp_dir();
        let r = run_streaming(sh("echo one; echo two 1>&2; exit 3"), Some(ctx(seen.clone())), &dir).await;
        assert_eq!(r.exit_code, Some(3));
        assert!(r.output.contains("one") && r.output.contains("two"));
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn kill_stops_the_whole_group() {
        let dir = std::env::temp_dir();
        let h = tokio::spawn(async move {
            let c = RunCtx { call_id: "k".into(), label: "killtest".into(), sink: Arc::new(|_| {}) };
            run_streaming(sh("echo started; sleep 30 & sleep 30; echo never"), Some(c), &dir).await
        });
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let id = list().into_iter().find(|j| j.running() && j.command == "killtest").map(|j| j.id).unwrap();
        assert!(kill(id));
        let r = tokio::time::timeout(std::time::Duration::from_secs(5), h).await.unwrap().unwrap();
        assert!(r.output.contains("started") && r.output.contains("[stopped by the user]"));
        assert!(!r.output.contains("never"));
    }

    #[tokio::test]
    async fn detach_returns_early_and_keeps_logging() {
        let dir = std::env::temp_dir().join(format!("jobs-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let d2 = dir.clone();
        let h = tokio::spawn(async move {
            let c = RunCtx { call_id: "bg".into(), label: "bgtest".into(), sink: Arc::new(|_| {}) };
            run_streaming(sh("echo first; sleep 1; echo second"), Some(c), &d2).await
        });
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        // Detach this test's job specifically (other tests may run jobs too).
        let id = list().into_iter().find(|j| j.running() && j.command == "bgtest").map(|j| j.id).expect("bg job");
        {
            let mut r = reg().lock().unwrap();
            let tx = r.ctl.get_mut(&id).and_then(|c| c.detach.take()).unwrap();
            let _ = tx.send(());
        }
        let r = tokio::time::timeout(std::time::Duration::from_secs(2), h).await.unwrap().unwrap();
        assert!(r.output.contains("first") && r.output.contains(&format!("job #{id}")));
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        let job = list().into_iter().find(|j| j.id == id).unwrap();
        assert!(!job.running() && job.background);
        assert!(tail(id, 10).iter().any(|l| l == "second"));
    }
}
