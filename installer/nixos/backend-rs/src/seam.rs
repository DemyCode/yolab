use std::future::Future;

use tokio::sync::mpsc;

use crate::app::AppEvent;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Delete,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub method: Method,
    pub url: String,
    pub bearer: Option<String>,
    pub cookie: Option<String>,
    pub json: Option<serde_json::Value>,
}

impl Request {
    pub fn new(method: Method, url: impl Into<String>) -> Self {
        Self {
            method,
            url: url.into(),
            bearer: None,
            cookie: None,
            json: None,
        }
    }

    pub fn bearer(mut self, token: &str) -> Self {
        self.bearer = Some(token.to_string());
        self
    }

    pub fn cookie(mut self, cookie: &str) -> Self {
        self.cookie = Some(cookie.to_string());
        self
    }

    pub fn json(mut self, body: serde_json::Value) -> Self {
        self.json = Some(body);
        self
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Reply {
    pub status: u16,
    pub text: String,
    pub set_cookie: Option<String>,
}

impl Reply {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Output {
    pub success: bool,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

pub type Events = mpsc::UnboundedSender<AppEvent>;

pub trait Machine: Clone + Send + Sync + 'static {
    fn resolves(&self, host: &str) -> impl Future<Output = bool> + Send;
    fn pause(&self) -> impl Future<Output = ()> + Send;
    fn has_efi(&self) -> bool;
    fn http(&self, req: Request) -> impl Future<Output = anyhow::Result<Reply>> + Send;
    fn run(
        &self,
        program: &str,
        args: &[&str],
        stdin: Option<&str>,
    ) -> impl Future<Output = anyhow::Result<Output>> + Send;
    fn stream(
        &self,
        program: &str,
        args: &[&str],
        tx: &Events,
    ) -> impl Future<Output = anyhow::Result<()>> + Send;
    fn capture(
        &self,
        program: &str,
        args: &[&str],
        tx: &Events,
    ) -> impl Future<Output = anyhow::Result<Vec<u8>>> + Send;
    fn spawn_detached(&self, program: &str, args: &[&str]) -> anyhow::Result<()>;
    fn exists(&self, path: &str) -> impl Future<Output = bool> + Send;
    fn read_file(&self, path: &str) -> impl Future<Output = anyhow::Result<String>> + Send;
    fn write_file(
        &self,
        path: &str,
        contents: &[u8],
        mode: Option<u32>,
    ) -> impl Future<Output = anyhow::Result<()>> + Send;
    fn create_dir_all(&self, path: &str) -> impl Future<Output = anyhow::Result<()>> + Send;
    fn remove_file(&self, path: &str) -> impl Future<Output = std::io::Result<()>> + Send;
    fn remove_dir_all(&self, path: &str) -> impl Future<Output = anyhow::Result<()>> + Send;
    fn scratch_dir(&self) -> anyhow::Result<Scratch>;
}

pub struct Scratch {
    pub path: String,
    pub _keep: Option<tempfile::TempDir>,
}
