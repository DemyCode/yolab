use anyhow::Result;
use serde_json::json;

const CM_NAME: &str = "yolab-disk-priority";
const CM_NS: &str = "rook-ceph";

#[derive(Clone, Debug, PartialEq)]
pub struct PriorityEntry {
    pub host: String,
    pub disk_name: String,
}

impl PriorityEntry {
    pub fn key(&self) -> String {
        format!("{}:{}", self.host, self.disk_name)
    }
}

fn parse_lines(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect()
}

async fn read_cm() -> serde_json::Map<String, serde_json::Value> {
    let Ok(out) = crate::kubectl::run(&[
        "get", "configmap", "-n", CM_NS, CM_NAME, "-o", "json",
    ])
    .await else { return Default::default() };
    serde_json::from_str::<serde_json::Value>(&out)
        .ok()
        .and_then(|v| v["data"].as_object().cloned())
        .unwrap_or_default()
}

async fn write_cm(data: &serde_json::Map<String, serde_json::Value>) -> Result<()> {
    let manifest = json!({
        "apiVersion": "v1",
        "kind": "ConfigMap",
        "metadata": {"name": CM_NAME, "namespace": CM_NS},
        "data": data,
    });
    let mut child = tokio::process::Command::new("kubectl")
        .args(["apply", "-f", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    if let Some(stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        let mut stdin = stdin;
        stdin.write_all(manifest.to_string().as_bytes()).await?;
    }
    child.wait().await?;
    Ok(())
}

pub async fn read() -> Vec<PriorityEntry> {
    let data = read_cm().await;
    let text = data.get("priority").and_then(|v| v.as_str()).unwrap_or("");
    parse_lines(text)
        .into_iter()
        .filter_map(|line| {
            let (host, disk) = line.rsplit_once(':')?;
            Some(PriorityEntry {
                host: host.to_string(),
                disk_name: disk.to_string(),
            })
        })
        .collect()
}

pub async fn write(entries: &[PriorityEntry]) -> Result<()> {
    let mut data = read_cm().await;
    data.insert(
        "priority".into(),
        entries.iter().map(PriorityEntry::key).collect::<Vec<_>>().join("\n").into(),
    );
    write_cm(&data).await
}

pub async fn prepend(host: &str, disk_name: &str) -> Result<()> {
    let key = format!("{host}:{disk_name}");
    let mut data = read_cm().await;
    if parse_lines(data.get("rejected").and_then(|v| v.as_str()).unwrap_or(""))
        .contains(&key)
    {
        return Ok(());
    }
    let mut existing = parse_lines(data.get("priority").and_then(|v| v.as_str()).unwrap_or(""));
    if existing.contains(&key) {
        return Ok(());
    }
    existing.insert(0, key);
    data.insert("priority".into(), existing.join("\n").into());
    write_cm(&data).await
}

pub async fn append(host: &str, disk_name: &str) -> Result<()> {
    let key = format!("{host}:{disk_name}");
    let mut data = read_cm().await;
    let mut existing = parse_lines(data.get("priority").and_then(|v| v.as_str()).unwrap_or(""));
    if existing.contains(&key) {
        return Ok(());
    }
    if parse_lines(data.get("rejected").and_then(|v| v.as_str()).unwrap_or(""))
        .contains(&key)
    {
        return Ok(());
    }
    existing.push(key);
    data.insert("priority".into(), existing.join("\n").into());
    write_cm(&data).await
}
