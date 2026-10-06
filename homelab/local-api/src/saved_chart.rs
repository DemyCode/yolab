use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine as _;
use kube::Client;
use serde_json::{json, Value};

use crate::host::Host;

pub(crate) const CONFIG_MAP: &str = "yolab-chart";
const KEY: &str = "chart.tgz";
pub(crate) const SCHEMA_MAP: &str = "yolab-schema";
const SCHEMA_KEY: &str = "values.schema.json";
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

pub(crate) fn is_kept_with_app(item: &Value) -> bool {
    item["kind"].as_str() == Some("ConfigMap")
        && matches!(
            item["metadata"]["name"].as_str(),
            Some(CONFIG_MAP | SCHEMA_MAP)
        )
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
    crate::k8s::apply(client, &manifest(namespace, &tgz)).await?;
    let schema = tokio::fs::read_to_string(chart_dir.join("values.schema.json"))
        .await
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .unwrap_or(Value::Null);
    crate::k8s::apply(client, &schema_manifest(namespace, &schema)).await
}

pub(crate) fn schema_manifest(namespace: &str, schema: &Value) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "ConfigMap",
        "metadata": { "name": SCHEMA_MAP, "namespace": namespace },
        "data": { SCHEMA_KEY: schema.to_string() },
    })
}

pub(crate) fn schema_of(config_map: &Value) -> Option<Value> {
    serde_json::from_str(config_map["data"][SCHEMA_KEY].as_str()?).ok()
}

pub(crate) fn schema_in_objects(objects: &Value) -> Option<Value> {
    objects["items"]
        .as_array()?
        .iter()
        .find(|i| {
            i["kind"].as_str() == Some("ConfigMap")
                && i["metadata"]["name"].as_str() == Some(SCHEMA_MAP)
        })
        .and_then(schema_of)
}

pub(crate) async fn read_schema(client: &Client, namespace: &str) -> anyhow::Result<Option<Value>> {
    let found = crate::k8s::get(
        client,
        &crate::k8s::reference("v1", "ConfigMap", namespace, SCHEMA_MAP),
    )
    .await?;
    Ok(found.as_ref().and_then(schema_of))
}

pub(crate) async fn all_schemas(
    client: &Client,
) -> anyhow::Result<std::collections::HashMap<String, Value>> {
    let named = kube::api::ListParams::default().fields(&format!("metadata.name={SCHEMA_MAP}"));
    let maps = crate::k8s::list(client, "v1", "ConfigMap", None, &named).await?;
    Ok(maps
        .iter()
        .filter_map(|m| {
            let ns = m["metadata"]["namespace"].as_str()?.to_string();
            Some((ns, schema_of(m)?))
        })
        .collect())
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

    #[test]
    fn a_saved_schema_reads_back_as_the_same_json() {
        let schema = json!({ "properties": { "config": { "properties": {
            "admin_password": { "type": "string", "writeOnly": true }
        }}}});
        let m = schema_manifest("yolab-notes-ab12", &schema);
        assert_eq!(m["metadata"]["name"], SCHEMA_MAP);
        assert_eq!(schema_of(&m), Some(schema));
    }

    #[test]
    fn a_schema_map_that_is_missing_or_garbled_is_no_schema() {
        assert_eq!(schema_of(&json!({ "data": {} })), None);
        let garbled = json!({ "data": { SCHEMA_KEY: "{not json" } });
        assert_eq!(schema_of(&garbled), None);
    }

    #[test]
    fn a_backup_gives_back_the_schema_it_was_taken_with() {
        let schema = json!({ "type": "object" });
        let items = vec![
            manifest("yolab-x", &[1]),
            schema_manifest("yolab-x", &schema),
        ];
        let objects = json!({ "items": items });
        assert_eq!(schema_in_objects(&objects), Some(schema));
        assert_eq!(in_objects(&objects), Some(vec![1]));
    }

    #[test]
    fn only_the_chart_and_schema_kept_for_an_app_are_bookkeeping() {
        assert!(is_kept_with_app(&manifest("yolab-x", &[1])));
        assert!(is_kept_with_app(&schema_manifest("yolab-x", &json!(null))));
        let caddy = json!({ "kind": "ConfigMap", "metadata": { "name": "notes-caddy" } });
        let secret = json!({ "kind": "Secret", "metadata": { "name": "yolab-chart" } });
        assert!(!is_kept_with_app(&caddy));
        assert!(!is_kept_with_app(&secret));
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
