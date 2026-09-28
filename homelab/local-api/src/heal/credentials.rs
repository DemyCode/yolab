use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};

use kube::Client;

use crate::routers::backup_common::{apply_secret, MASTER_NS, MASTER_SECRET};
use crate::runtime::{Controller, Ctx, Requirement, Scope, Tick};

const NAME: &str = "backup-credentials";
const COPY: &str = "var/lib/yolab/backup-config.json";
const PASSWORD: &str = "restic_password";

type Credentials = BTreeMap<String, String>;

#[derive(Debug, PartialEq)]
enum Action {
    Nothing,
    Save(Credentials),
    Restore(Credentials),
}

fn usable(c: &Credentials) -> bool {
    c.get(PASSWORD).is_some_and(|p| !p.is_empty())
}

fn decide(secret: Option<Credentials>, copy: Option<Credentials>) -> Action {
    match (secret, copy) {
        (Some(secret), copy) if usable(&secret) => {
            if copy.as_ref() == Some(&secret) {
                Action::Nothing
            } else {
                Action::Save(secret)
            }
        }
        (Some(_), _) => Action::Nothing,
        (None, Some(copy)) if usable(&copy) => Action::Restore(copy),
        (None, _) => Action::Nothing,
    }
}

fn copy_path(root: &Path) -> PathBuf {
    root.join(COPY)
}

fn read_copy(root: &Path) -> Result<Option<Credentials>> {
    let path = copy_path(root);
    match std::fs::read(&path) {
        Ok(raw) => serde_json::from_slice(&raw)
            .map(Some)
            .with_context(|| format!("{} is unreadable", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

fn write_copy(root: &Path, credentials: &Credentials) -> Result<()> {
    crate::config::write_private_file(&copy_path(root), &serde_json::to_vec(credentials)?)
}

pub struct BackupCredentialsController;

impl Controller for BackupCredentialsController {
    fn name(&self) -> &'static str {
        NAME
    }
    fn scope(&self) -> Scope {
        Scope::Node
    }
    fn interval(&self) -> Duration {
        Duration::from_secs(300)
    }
    fn requires(&self) -> &'static [Requirement] {
        &[Requirement::KubeApi]
    }
    async fn reconcile(&self, _ctx: &Ctx) -> Result<Tick> {
        sync(&crate::k8s::client().await?, Path::new("/")).await
    }
}

async fn sync(client: &Client, root: &Path) -> Result<Tick> {
    let secret = crate::k8s::secret_data(client, MASTER_NS, MASTER_SECRET)
        .await?
        .map(|s| s.into_iter().collect::<Credentials>());
    match decide(secret, read_copy(root)?) {
        Action::Nothing => Ok(Tick::Idle("the copy is up to date".into())),
        Action::Save(credentials) => {
            write_copy(root, &credentials)?;
            tracing::info!("backup credentials copied to {COPY}");
            Ok(Tick::Done)
        }
        Action::Restore(credentials) => {
            let data: Vec<(&str, &str)> = credentials
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            apply_secret(client, MASTER_SECRET, MASTER_NS, &data).await?;
            tracing::warn!(
                "{MASTER_NS}/{MASTER_SECRET} was missing — put back from this machine's copy"
            );
            Ok(Tick::Done)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn creds(password: &str, key: &str) -> Credentials {
        Credentials::from([
            (PASSWORD.to_string(), password.to_string()),
            ("access_key_id".to_string(), key.to_string()),
        ])
    }

    #[test]
    fn the_secret_is_copied_whenever_it_changes() {
        assert_eq!(
            decide(Some(creds("p", "k")), None),
            Action::Save(creds("p", "k"))
        );
        assert_eq!(
            decide(Some(creds("p", "rotated")), Some(creds("p", "k"))),
            Action::Save(creds("p", "rotated"))
        );
        assert_eq!(
            decide(Some(creds("p", "k")), Some(creds("p", "k"))),
            Action::Nothing
        );
    }

    #[test]
    fn a_missing_secret_comes_back_from_the_copy() {
        assert_eq!(
            decide(None, Some(creds("p", "k"))),
            Action::Restore(creds("p", "k"))
        );
        assert_eq!(
            decide(None, None),
            Action::Nothing,
            "backups were never enabled"
        );
    }

    #[test]
    fn nothing_without_a_password_is_ever_copied_or_restored() {
        assert_eq!(
            decide(Some(creds("", "k")), Some(creds("p", "k"))),
            Action::Nothing
        );
        assert_eq!(decide(None, Some(creds("", "k"))), Action::Nothing);
    }

    #[test]
    fn the_copy_round_trips_and_is_root_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_copy(dir.path()).unwrap(), None);
        write_copy(dir.path(), &creds("p", "k")).unwrap();
        assert_eq!(read_copy(dir.path()).unwrap(), Some(creds("p", "k")));
        let mode = std::fs::metadata(copy_path(dir.path()))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    mod against_the_cluster {
        use super::*;
        use crate::k8s::testing::{api_server, status};
        use wiremock::matchers::{body_partial_json, method, path};
        use wiremock::{Mock, ResponseTemplate};

        const SECRET_PATH: &str = "/api/v1/namespaces/kube-system/secrets/yolab-backup-config";

        #[tokio::test]
        async fn a_deleted_secret_is_put_back_from_the_copy_on_disk() {
            let dir = tempfile::tempdir().unwrap();
            write_copy(dir.path(), &creds("p", "k")).unwrap();
            let (server, client) = api_server().await;
            Mock::given(method("GET"))
                .and(path(SECRET_PATH))
                .respond_with(ResponseTemplate::new(404).set_body_json(status(404, "NotFound")))
                .mount(&server)
                .await;
            Mock::given(method("PATCH"))
                .and(path(SECRET_PATH))
                .and(body_partial_json(serde_json::json!({
                    "stringData": { "restic_password": "p", "access_key_id": "k" }
                })))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "apiVersion": "v1", "kind": "Secret",
                    "metadata": { "name": "yolab-backup-config", "namespace": "kube-system" }
                })))
                .expect(1)
                .mount(&server)
                .await;

            assert!(matches!(
                sync(&client, dir.path()).await.unwrap(),
                Tick::Done
            ));
        }

        #[tokio::test]
        async fn an_unreachable_cluster_is_not_mistaken_for_a_missing_secret() {
            let dir = tempfile::tempdir().unwrap();
            write_copy(dir.path(), &creds("p", "k")).unwrap();
            let (server, client) = api_server().await;
            Mock::given(method("GET"))
                .respond_with(
                    ResponseTemplate::new(503).set_body_json(status(503, "ServiceUnavailable")),
                )
                .mount(&server)
                .await;
            Mock::given(method("PATCH"))
                .respond_with(ResponseTemplate::new(200))
                .expect(0)
                .mount(&server)
                .await;

            assert!(sync(&client, dir.path()).await.is_err());
        }

        #[tokio::test]
        async fn the_cluster_secret_is_saved_to_disk_when_it_changes() {
            use base64::Engine as _;
            let b = |s: &str| base64::engine::general_purpose::STANDARD.encode(s);
            let dir = tempfile::tempdir().unwrap();
            let (server, client) = api_server().await;
            Mock::given(method("GET"))
                .and(path(SECRET_PATH))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "apiVersion": "v1", "kind": "Secret",
                    "metadata": { "name": "yolab-backup-config", "namespace": "kube-system" },
                    "data": { "restic_password": b("p"), "access_key_id": b("k") }
                })))
                .mount(&server)
                .await;

            assert!(matches!(
                sync(&client, dir.path()).await.unwrap(),
                Tick::Done
            ));
            assert_eq!(read_copy(dir.path()).unwrap(), Some(creds("p", "k")));
        }
    }
}
