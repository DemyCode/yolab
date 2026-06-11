use std::convert::Infallible;

use axum::response::{sse::Event, Sse};
use serde::Deserialize;

use crate::proc::KillOnDrop;

#[derive(Deserialize)]
pub struct ExecRequest {
    pub command: String,
}

pub async fn exec(
    axum::Json(req): axum::Json<ExecRequest>,
) -> Sse<impl futures::Stream<Item = Result<Event, Infallible>>> {
    let stream = async_stream::stream! {
        let child = tokio::process::Command::new("bash")
            .args(["-c", &req.command])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn();

        let mut guard = match child {
            Ok(c) => KillOnDrop(c),
            Err(e) => {
                yield Ok(Event::default().data(format!("[ERROR] {e}")));
                yield Ok(Event::default().data("[EXIT:1]"));
                return;
            }
        };

        use tokio::io::AsyncBufReadExt;
        let stdout = guard.0.stdout.take().unwrap();
        let stderr = guard.0.stderr.take().unwrap();
        let mut out = tokio::io::BufReader::new(stdout).lines();
        let mut err = tokio::io::BufReader::new(stderr).lines();
        let mut out_done = false;
        let mut err_done = false;

        while !out_done || !err_done {
            tokio::select! {
                line = out.next_line(), if !out_done => match line {
                    Ok(Some(l)) => yield Ok(Event::default().data(l)),
                    _ => out_done = true,
                },
                line = err.next_line(), if !err_done => match line {
                    Ok(Some(l)) => yield Ok(Event::default().data(l)),
                    _ => err_done = true,
                },
            }
        }

        let code = guard.0.wait().await.map(|s| s.code().unwrap_or(1)).unwrap_or(1);
        yield Ok(Event::default().data(format!("[EXIT:{code}]")));
    };

    Sse::new(stream)
}
