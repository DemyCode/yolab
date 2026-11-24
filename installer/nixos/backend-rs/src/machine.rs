use std::process::Stdio;

use anyhow::Context;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

use crate::lines::forward_lines;
use crate::seam::{Events, Machine, Method, Output, Reply, Request, Scratch};

#[derive(Clone, Default)]
pub struct RealMachine {
    client: reqwest::Client,
}

impl Machine for RealMachine {
    async fn resolves(&self, host: &str) -> bool {
        tokio::net::lookup_host(host).await.is_ok()
    }

    async fn pause(&self) {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }

    fn has_efi(&self) -> bool {
        std::path::Path::new("/sys/firmware/efi/efivars").exists()
    }

    async fn http(&self, req: Request) -> anyhow::Result<Reply> {
        let mut b = match req.method {
            Method::Get => self.client.get(&req.url),
            Method::Post => self.client.post(&req.url),
            Method::Delete => self.client.delete(&req.url),
        };
        if let Some(t) = &req.bearer {
            b = b.bearer_auth(t);
        }
        if let Some(c) = &req.cookie {
            b = b.header(reqwest::header::COOKIE, c);
        }
        if let Some(j) = &req.json {
            b = b.json(j);
        }
        let resp = b
            .send()
            .await
            .with_context(|| format!("{:?} {}", req.method, req.url))?;
        let status = resp.status().as_u16();
        let set_cookie = resp
            .headers()
            .get(reqwest::header::SET_COOKIE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let text = resp.text().await?;
        Ok(Reply {
            status,
            text,
            set_cookie,
        })
    }

    async fn run(
        &self,
        program: &str,
        args: &[&str],
        stdin: Option<&str>,
    ) -> anyhow::Result<Output> {
        let mut child = tokio::process::Command::new(program)
            .args(args)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("spawn {program}"))?;
        if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
            pipe.write_all(input.as_bytes()).await?;
        }
        let out = child.wait_with_output().await?;
        Ok(Output {
            success: out.status.success(),
            stdout: out.stdout,
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    async fn stream(&self, program: &str, args: &[&str], tx: &Events) -> anyhow::Result<()> {
        let mut child = tokio::process::Command::new(program)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("spawn {program}"))?;
        let stdout = child.stdout.take().context("child stdout")?;
        let stderr = child.stderr.take().context("child stderr")?;
        let (tx1, tx2) = (tx.clone(), tx.clone());
        let t1 =
            tokio::spawn(async move { forward_lines(BufReader::new(stdout).lines(), &tx1).await });
        let t2 =
            tokio::spawn(async move { forward_lines(BufReader::new(stderr).lines(), &tx2).await });
        let (out, err) = tokio::join!(t1, t2);
        out.context("read the command's output")?;
        err.context("read the command's errors")?;
        let status = child.wait().await?;
        anyhow::ensure!(status.success(), "{program} failed with {status}");
        Ok(())
    }

    async fn capture(&self, program: &str, args: &[&str], tx: &Events) -> anyhow::Result<Vec<u8>> {
        let mut child = tokio::process::Command::new(program)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("spawn {program}"))?;
        let mut stdout = child.stdout.take().context("child stdout")?;
        let stderr = child.stderr.take().context("child stderr")?;
        let tx2 = tx.clone();
        let t =
            tokio::spawn(async move { forward_lines(BufReader::new(stderr).lines(), &tx2).await });
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).await?;
        t.await.context("read the command's errors")?;
        let status = child.wait().await?;
        anyhow::ensure!(status.success(), "{program} failed with {status}");
        Ok(bytes)
    }

    fn spawn_detached(&self, program: &str, args: &[&str]) -> anyhow::Result<()> {
        std::process::Command::new(program)
            .args(args)
            .spawn()
            .with_context(|| format!("spawn {program}"))?;
        Ok(())
    }

    async fn exists(&self, path: &str) -> bool {
        tokio::fs::try_exists(path).await.unwrap_or(false)
    }

    async fn read_file(&self, path: &str) -> anyhow::Result<String> {
        tokio::fs::read_to_string(path)
            .await
            .with_context(|| format!("read {path}"))
    }

    async fn write_file(
        &self,
        path: &str,
        contents: &[u8],
        mode: Option<u32>,
    ) -> anyhow::Result<()> {
        tokio::fs::write(path, contents)
            .await
            .with_context(|| format!("write {path}"))?;
        if let Some(m) = mode {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(m)).await?;
        }
        Ok(())
    }

    async fn create_dir_all(&self, path: &str) -> anyhow::Result<()> {
        tokio::fs::create_dir_all(path)
            .await
            .with_context(|| format!("create {path}"))
    }

    async fn remove_file(&self, path: &str) -> std::io::Result<()> {
        tokio::fs::remove_file(path).await
    }

    async fn remove_dir_all(&self, path: &str) -> anyhow::Result<()> {
        tokio::fs::remove_dir_all(path)
            .await
            .with_context(|| format!("remove {path}"))
    }

    fn scratch_dir(&self) -> anyhow::Result<Scratch> {
        let dir = tempfile::tempdir()?;
        let path = dir
            .path()
            .to_str()
            .ok_or_else(|| {
                anyhow::anyhow!("temp path is not valid UTF-8: {}", dir.path().display())
            })?
            .to_string();
        Ok(Scratch {
            path,
            _keep: Some(dir),
        })
    }
}
