use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine as _;
use kube::Client;
use serde_json::{json, Value};

use crate::host::Host;

pub(crate) const CONFIG_MAP: &str = "yolab-chart";
const KEY: &str = "chart.tgz";
const MAX_BYTES: usize = 900 * 1024;
const RUN_TIMEOUT: Duration = Duration::from_secs(120);

pub(crate) fn manifest(namespace: &str, tgz: &[u8]) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "ConfigMap",
        "metadata": { "name": CONFIG_MAP, "namespace": namespace },
        "binaryData": { KEY: base64::engine::general_purpose::STANDARD.encode(tgz) },
    })
}

pub(crate) fn tgz_of(config_map: &Value) -> Option<Vec<u8>> {
    let encoded = config_map["binaryData"][KEY].as_str()?;
    base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()
        .filter(|b| !b.is_empty())
}

pub(crate) fn is_saved_chart(item: &Value) -> bool {
    item["kind"].as_str() == Some("ConfigMap")
        && item["metadata"]["name"].as_str() == Some(CONFIG_MAP)
}

pub(crate) fn in_objects(objects: &Value) -> Option<Vec<u8>> {
    objects["items"]
        .as_array()?
        .iter()
        .find(|i| is_saved_chart(i))
        .and_then(tgz_of)
}

async fn run<H: Host>(host: &H, bin: &str, args: &[&str]) -> anyhow::Result<()> {
    let out = host.run_cmd_bounded(bin, args, RUN_TIMEOUT).await?;
    if !out.success {
        anyhow::bail!("{bin}: {}", out.stderr.trim());
    }
    Ok(())
}

pub(crate) async fn package<H: Host>(host: &H, chart_dir: &Path) -> anyhow::Result<Vec<u8>> {
    let out = tempfile::tempdir()?;
    run(
        host,
        "helm",
        &[
            "package",
            &chart_dir.to_string_lossy(),
            "--destination",
            &out.path().to_string_lossy(),
        ],
    )
    .await?;
    let tgz = std::fs::read_dir(out.path())?
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "tgz"))
        .ok_or_else(|| anyhow::anyhow!("helm package produced no chart"))?;
    let bytes = tokio::fs::read(&tgz).await?;
    if bytes.len() > MAX_BYTES {
        anyhow::bail!(
            "the chart is {} KiB, more than the {} KiB a ConfigMap can hold",
            bytes.len() / 1024,
            MAX_BYTES / 1024
        );
    }
    Ok(bytes)
}

pub(crate) async fn save<H: Host>(
    host: &H,
    client: &Client,
    namespace: &str,
    chart_dir: &Path,
) -> anyhow::Result<()> {
    let tgz = package(host, chart_dir).await?;
    crate::k8s::apply(client, &manifest(namespace, &tgz)).await
}

pub(crate) async fn read(client: &Client, namespace: &str) -> anyhow::Result<Option<Vec<u8>>> {
    let found = crate::k8s::get(
        client,
        &crate::k8s::reference("v1", "ConfigMap", namespace, CONFIG_MAP),
    )
    .await?;
    Ok(found.as_ref().and_then(tgz_of))
}

pub(crate) struct Unpacked {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Unpacked {
    pub(crate) fn chart_dir(&self) -> &Path {
        &self.root
    }
}

pub(crate) async fn unpack<H: Host>(
    host: &H,
    tgz: &[u8],
    app_id: &str,
) -> anyhow::Result<Unpacked> {
    let dir = tempfile::tempdir()?;
    let archive = dir.path().join(KEY);
    tokio::fs::write(&archive, tgz).await?;
    let charts = dir.path().join("charts");
    tokio::fs::create_dir_all(&charts).await?;
    run(
        host,
        "tar",
        &[
            "-xzf",
            &archive.to_string_lossy(),
            "-C",
            &charts.to_string_lossy(),
            "--no-same-owner",
        ],
    )
    .await?;
    let root = charts.join(app_id);
    if !root.join("Chart.yaml").is_file() {
        anyhow::bail!("the chart kept with this app is not {app_id}");
    }
    Ok(Unpacked { _dir: dir, root })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::fake::FakeHost;

    #[test]
    fn a_saved_chart_reads_back_byte_for_byte() {
        let tgz = vec![0x1f, 0x8b, 0, 1, 2, 255];
        let m = manifest("yolab-immich-hd6s", &tgz);
        assert_eq!(m["kind"], "ConfigMap");
        assert_eq!(m["metadata"]["name"], CONFIG_MAP);
        assert_eq!(m["metadata"]["namespace"], "yolab-immich-hd6s");
        assert_eq!(tgz_of(&m), Some(tgz));
    }

    #[test]
    fn an_empty_or_missing_saved_chart_is_no_chart() {
        assert_eq!(tgz_of(&json!({ "binaryData": {} })), None);
        assert_eq!(tgz_of(&manifest("yolab-x", &[])), None);
    }

    #[test]
    fn a_backup_gives_back_the_chart_it_was_taken_with() {
        let tgz = vec![1, 2, 3];
        let objects = json!({ "items": [
            { "kind": "ConfigMap", "metadata": { "name": "something-else" }, "binaryData": { KEY: "BAUG" } },
            manifest("yolab-x", &tgz),
        ]});
        assert_eq!(in_objects(&objects), Some(tgz));
        assert_eq!(in_objects(&json!({ "items": [] })), None);
    }

    #[tokio::test]
    async fn a_package_helm_did_not_produce_is_an_error_not_an_empty_chart() {
        let host = FakeHost::new().ok("helm package", "");
        let e = package(&host, Path::new("/nowhere/notes"))
            .await
            .unwrap_err();
        assert!(e.to_string().contains("no chart"), "{e}");
    }

    #[tokio::test]
    async fn a_package_with_missing_dependencies_is_refused_with_helms_reason() {
        let host = FakeHost::new().fail(
            "helm package",
            "found in Chart.yaml, but missing in charts/ directory: yolab-common",
        );
        let e = package(&host, Path::new("/nowhere/notes"))
            .await
            .unwrap_err();
        assert!(e.to_string().contains("missing in charts/"), "{e}");
    }

    #[tokio::test]
    async fn an_archive_holding_another_app_is_refused() {
        let host = FakeHost::new().ok("tar -xzf", "");
        let e = unpack(&host, &[1, 2, 3], "immich").await.err().unwrap();
        assert!(e.to_string().contains("not immich"), "{e}");
    }

}
