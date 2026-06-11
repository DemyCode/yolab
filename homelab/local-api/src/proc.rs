//! Subprocess streaming utilities shared across routers.

use axum::response::sse::Event;
use std::process::Stdio;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Child,
};

/// Wraps a Child process and kills it when dropped.
/// This ensures kubectl log-follow processes are cleaned up when the SSE
/// client disconnects, preventing accumulation against kubectl's concurrency limit.
pub struct KillOnDrop(pub Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.start_kill();
    }
}

/// Spawn a command and return a stream that yields one SSE Event per output line.
/// The process is killed automatically when the stream is dropped (client disconnect).
pub fn stream_cmd(
    cmd: &str,
    args: &[&str],
) -> impl futures::Stream<Item = Result<Event, std::convert::Infallible>> {
    let cmd = cmd.to_string();
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();

    async_stream::try_stream! {
        let child = tokio::process::Command::new(&cmd)
            .args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();

        let child = match child {
            Ok(c) => c,
            Err(e) => {
                yield Event::default().data(format!("[ERROR] spawn: {e}"));
                return;
            }
        };

        let mut guard = KillOnDrop(child);
        let stdout = guard.0.stdout.take().unwrap();
        let stderr = guard.0.stderr.take().unwrap();

        // Merge stdout and stderr into one stream
        let mut stdout_lines = BufReader::new(stdout).lines();
        let mut stderr_lines = BufReader::new(stderr).lines();

        loop {
            tokio::select! {
                line = stdout_lines.next_line() => {
                    match line {
                        Ok(Some(l)) => yield Event::default().data(l),
                        Ok(None) => break,
                        Err(e) => { yield Event::default().data(format!("[ERROR] {e}")); break; }
                    }
                }
                line = stderr_lines.next_line() => {
                    match line {
                        Ok(Some(l)) => yield Event::default().data(l),
                        Ok(None) => {}
                        Err(_) => {}
                    }
                }
            }
        }

        let _ = guard.0.wait().await;
    }
}

/// Like stream_cmd but returns (lines, exit_code) through a channel.
/// Used when callers need to react to the exit code.
pub async fn run_streaming<F>(cmd: &str, args: &[&str], mut on_line: F) -> i32
where
    F: FnMut(String),
{
    let Ok(mut child) = tokio::process::Command::new(cmd)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    else {
        return 1;
    };

    if let Some(stdout) = child.stdout.take() {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            on_line(line);
        }
    }

    child
        .wait()
        .await
        .map(|s| s.code().unwrap_or(1))
        .unwrap_or(1)
}
