use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::app::AppEvent;
use crate::seam::{Events, Machine, Method, Output, Reply, Request, Scratch};

#[derive(Default)]
pub struct State {
    pub calls: Vec<String>,
    pub requests: Vec<Request>,
    pub replies: HashMap<String, VecDeque<Result<Reply, String>>>,
    pub outputs: HashMap<String, VecDeque<Result<Output, String>>>,
    pub stdin: Vec<(String, String)>,
    pub writes_after_f: HashMap<String, Vec<(String, String)>>,
    pub streams: HashMap<String, VecDeque<Result<Vec<String>, String>>>,
    pub captures: HashMap<String, VecDeque<Result<Vec<u8>, String>>>,
    pub detached: HashMap<String, String>,
    pub files: BTreeMap<String, (Vec<u8>, Option<u32>)>,
    pub dirs: BTreeSet<String>,
    pub broken_paths: HashSet<String>,
    pub remove_errors: HashMap<String, std::io::ErrorKind>,
    pub resolves: VecDeque<bool>,
    pub efi: bool,
    pub pauses: usize,
    pub no_scratch: bool,
}

#[derive(Clone, Default)]
pub struct FakeMachine(Arc<Mutex<State>>);

pub fn key(method: Method, url: &str) -> String {
    format!("{method:?} {url}")
}

impl FakeMachine {
    pub fn state(&self) -> MutexGuard<'_, State> {
        self.0.lock().unwrap()
    }

    pub fn reply(&self, method: Method, url: &str, status: u16, text: &str) -> &Self {
        self.reply_with(
            method,
            url,
            Ok(Reply {
                status,
                text: text.to_string(),
                set_cookie: None,
            }),
        )
    }

    pub fn reply_with(&self, method: Method, url: &str, reply: Result<Reply, String>) -> &Self {
        self.state()
            .replies
            .entry(key(method, url))
            .or_default()
            .push_back(reply);
        self
    }

    pub fn output(&self, program: &str, success: bool, stdout: &str) -> &Self {
        self.output_with(
            program,
            Ok(Output {
                success,
                stdout: stdout.as_bytes().to_vec(),
                stderr: if success {
                    String::new()
                } else {
                    format!("{program} broke")
                },
            }),
        )
    }

    pub fn output_with(&self, program: &str, out: Result<Output, String>) -> &Self {
        self.state()
            .outputs
            .entry(program.to_string())
            .or_default()
            .push_back(out);
        self
    }

    pub fn stream_lines(&self, program: &str, lines: Result<&[&str], &str>) -> &Self {
        self.state()
            .streams
            .entry(program.to_string())
            .or_default()
            .push_back(
                lines
                    .map(|l| l.iter().map(|s| s.to_string()).collect())
                    .map_err(str::to_string),
            );
        self
    }

    pub fn capture_bytes(&self, program: &str, out: Result<&[u8], &str>) -> &Self {
        self.state()
            .captures
            .entry(program.to_string())
            .or_default()
            .push_back(out.map(<[u8]>::to_vec).map_err(str::to_string));
        self
    }

    pub fn file(&self, path: &str, contents: &str) -> &Self {
        self.state()
            .files
            .insert(path.to_string(), (contents.as_bytes().to_vec(), None));
        self
    }

    pub fn calls(&self) -> Vec<String> {
        self.state().calls.clone()
    }

    pub fn ran(&self, program: &str) -> bool {
        self.calls()
            .iter()
            .any(|c| c.split_whitespace().nth(1) == Some(program))
    }

    pub fn contents(&self, path: &str) -> Option<String> {
        self.state()
            .files
            .get(path)
            .map(|(b, _)| String::from_utf8(b.clone()).unwrap())
    }

    fn broken(&self, path: &str) -> anyhow::Result<()> {
        if self.state().broken_paths.contains(path) {
            anyhow::bail!("{path} is broken");
        }
        Ok(())
    }
}

pub fn events(rx: &mut tokio::sync::mpsc::UnboundedReceiver<AppEvent>) -> Vec<AppEvent> {
    let mut out = vec![];
    while let Ok(ev) = rx.try_recv() {
        out.push(ev);
    }
    out
}

pub fn logs(evs: &[AppEvent]) -> Vec<String> {
    evs.iter()
        .filter_map(|e| match e {
            AppEvent::Log(l) => Some(l.clone()),
            _ => None,
        })
        .collect()
}

impl Machine for FakeMachine {
    async fn resolves(&self, host: &str) -> bool {
        let mut s = self.state();
        s.calls.push(format!("resolve {host}"));
        s.resolves.pop_front().unwrap_or(true)
    }

    async fn pause(&self) {
        self.state().pauses += 1;
    }

    fn has_efi(&self) -> bool {
        self.state().efi
    }

    async fn http(&self, req: Request) -> anyhow::Result<Reply> {
        let k = key(req.method, &req.url);
        let mut s = self.state();
        s.calls.push(format!("http {k}"));
        s.requests.push(req);
        match s.replies.get_mut(&k).and_then(VecDeque::pop_front) {
            Some(Ok(r)) => Ok(r),
            Some(Err(e)) => Err(anyhow::anyhow!(e)),
            None => Err(anyhow::anyhow!("no reply scripted for {k}")),
        }
    }

    async fn run(
        &self,
        program: &str,
        args: &[&str],
        stdin: Option<&str>,
    ) -> anyhow::Result<Output> {
        let mut s = self.state();
        s.calls.push(format!("run {program} {}", args.join(" ")));
        if let Some(input) = stdin {
            s.stdin.push((program.to_string(), input.to_string()));
        }
        if let Some(writes) = s.writes_after_f.get(program).cloned() {
            if let Some(i) = args.iter().position(|a| *a == "-f") {
                for (suffix, contents) in writes {
                    s.files.insert(
                        format!("{}{suffix}", args[i + 1]),
                        (contents.into_bytes(), None),
                    );
                }
            }
        }
        match s.outputs.get_mut(program).and_then(VecDeque::pop_front) {
            Some(Ok(o)) => Ok(o),
            Some(Err(e)) => Err(anyhow::anyhow!(e)),
            None => Err(anyhow::anyhow!("no output scripted for {program}")),
        }
    }

    async fn stream(&self, program: &str, args: &[&str], tx: &Events) -> anyhow::Result<()> {
        let next = {
            let mut s = self.state();
            s.calls.push(format!("stream {program} {}", args.join(" ")));
            s.streams.get_mut(program).and_then(VecDeque::pop_front)
        };
        match next {
            None => Ok(()),
            Some(Ok(lines)) => {
                for l in lines {
                    let _ = tx.send(AppEvent::Log(l));
                }
                Ok(())
            }
            Some(Err(e)) => Err(anyhow::anyhow!(e)),
        }
    }

    async fn capture(&self, program: &str, args: &[&str], _tx: &Events) -> anyhow::Result<Vec<u8>> {
        let mut s = self.state();
        s.calls
            .push(format!("capture {program} {}", args.join(" ")));
        match s.captures.get_mut(program).and_then(VecDeque::pop_front) {
            None => Ok(vec![]),
            Some(r) => r.map_err(|e| anyhow::anyhow!(e)),
        }
    }

    fn spawn_detached(&self, program: &str, args: &[&str]) -> anyhow::Result<()> {
        let mut s = self.state();
        s.calls.push(format!("spawn {program} {}", args.join(" ")));
        match s.detached.get(program) {
            Some(e) => Err(anyhow::anyhow!(e.clone())),
            None => Ok(()),
        }
    }

    async fn exists(&self, path: &str) -> bool {
        let s = self.state();
        s.files.contains_key(path) || s.dirs.contains(path)
    }

    async fn read_file(&self, path: &str) -> anyhow::Result<String> {
        self.broken(path)?;
        self.contents(path)
            .ok_or_else(|| anyhow::anyhow!("read {path}: no such file"))
    }

    async fn write_file(
        &self,
        path: &str,
        contents: &[u8],
        mode: Option<u32>,
    ) -> anyhow::Result<()> {
        self.broken(path)?;
        let mut s = self.state();
        s.calls.push(format!("write {path}"));
        s.files.insert(path.to_string(), (contents.to_vec(), mode));
        Ok(())
    }

    async fn create_dir_all(&self, path: &str) -> anyhow::Result<()> {
        self.broken(path)?;
        let mut s = self.state();
        s.calls.push(format!("mkdir {path}"));
        s.dirs.insert(path.to_string());
        Ok(())
    }

    async fn remove_file(&self, path: &str) -> std::io::Result<()> {
        let mut s = self.state();
        s.calls.push(format!("rm {path}"));
        if let Some(kind) = s.remove_errors.get(path) {
            return Err(std::io::Error::new(*kind, format!("cannot remove {path}")));
        }
        s.files
            .remove(path)
            .map(|_| ())
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no such file"))
    }

    async fn remove_dir_all(&self, path: &str) -> anyhow::Result<()> {
        self.broken(path)?;
        let mut s = self.state();
        s.calls.push(format!("rmdir {path}"));
        s.dirs.remove(path);
        Ok(())
    }

    fn scratch_dir(&self) -> anyhow::Result<Scratch> {
        anyhow::ensure!(!self.state().no_scratch, "no temporary directory");
        Ok(Scratch {
            path: "/scratch".into(),
            _keep: None,
        })
    }
}
