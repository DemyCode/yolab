//! JSON records kept in a ConfigMap: backup history, restore history, storage
//! recovery state.
//!
//! THE BUG THIS EXISTS TO MAKE IMPOSSIBLE
//!
//! Each of those used to be a hand-written `read_sets()` / `write_sets()` pair:
//!
//! ```text
//! let Ok(v) = kubectl get configmap … else { return Vec::new() };   // read
//! let _ = kubectl apply -f - <whole list>;                           // write
//! ```
//!
//! An API blip during the read returned `[]`; the next write then applied a
//! one-element list over the whole history. For restores that history includes
//! the replica counts the watchdog needs to bring a crashed restore's app back up
//! — so one failed read could leave an app at zero replicas for good. And two
//! writers (two tasks, or two nodes) each applied their own copy: last one wins,
//! the other's update vanishes.
//!
//! Here:
//!   - A read that fails is an error. Only NotFound (never created) is empty.
//!   - A write is a compare-and-swap on `metadata.resourceVersion`
//!     (`kubectl replace`), retried from a fresh read on conflict, so concurrent
//!     updates compose instead of overwriting each other.
//!   - Content that does not parse is never silently discarded: an update moves
//!     it aside under `<key>.corrupt` so the evidence survives, and says so.

use serde::{de::DeserializeOwned, Serialize};
use serde_json::{json, Value};

use crate::exec::CmdError;
use crate::host::Host;

/// How many times an update re-reads and retries after losing a race.
const CAS_ATTEMPTS: usize = 8;

#[derive(Clone, Copy, Debug)]
pub struct Store {
    pub name: &'static str,
    pub namespace: &'static str,
    pub key: &'static str,
}

#[derive(Debug)]
pub enum RecordError {
    /// The ConfigMap could not be read or written.
    Cluster(CmdError),
    /// It was read, but its content is not what we wrote.
    Corrupt { store: String, detail: String },
    /// Every compare-and-swap attempt lost a race.
    Contended { store: String },
}

impl std::fmt::Display for RecordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RecordError::Cluster(e) => write!(f, "{e}"),
            RecordError::Corrupt { store, detail } => {
                write!(f, "{store}: stored records are unreadable: {detail}")
            }
            RecordError::Contended { store } => write!(
                f,
                "{store}: gave up after {CAS_ATTEMPTS} conflicting concurrent updates"
            ),
        }
    }
}

impl std::error::Error for RecordError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RecordError::Cluster(e) => Some(e),
            _ => None,
        }
    }
}

impl From<CmdError> for RecordError {
    fn from(e: CmdError) -> Self {
        RecordError::Cluster(e)
    }
}

struct Loaded {
    raw: Option<String>,
    resource_version: Option<String>,
    exists: bool,
}

impl Store {
    fn label(&self) -> String {
        format!("{}/{}", self.namespace, self.name)
    }

    async fn load<H: Host>(&self, host: &H) -> Result<Loaded, CmdError> {
        let got = host
            .kubectl_get_opt(&[
                "get",
                "configmap",
                self.name,
                "-n",
                self.namespace,
                "-o",
                "json",
            ])
            .await?;
        let Some(cm) = got else {
            return Ok(Loaded {
                raw: None,
                resource_version: None,
                exists: false,
            });
        };
        if cm["kind"].as_str() != Some("ConfigMap") {
            return Err(CmdError::parse(
                format!("kubectl get configmap {}", self.name),
                "not a ConfigMap",
            ));
        }
        Ok(Loaded {
            raw: cm["data"][self.key].as_str().map(str::to_string),
            resource_version: cm["metadata"]["resourceVersion"]
                .as_str()
                .map(str::to_string),
            exists: true,
        })
    }

    /// The stored value. `T::default()` only when the ConfigMap or key has never
    /// been written.
    pub async fn read<H: Host, T: DeserializeOwned + Default>(
        &self,
        host: &H,
    ) -> Result<T, RecordError> {
        let loaded = self.load(host).await?;
        match loaded.raw {
            None => Ok(T::default()),
            Some(raw) => serde_json::from_str(&raw).map_err(|e| RecordError::Corrupt {
                store: self.label(),
                detail: e.to_string(),
            }),
        }
    }

    /// Read, apply `f`, write back — atomically with respect to every other
    /// writer. `f` may run more than once (on a lost race), so it must be a pure
    /// function of the value it is given.
    pub async fn update<H, T, R>(
        &self,
        host: &H,
        mut f: impl FnMut(&mut T) -> R,
    ) -> Result<R, RecordError>
    where
        H: Host,
        T: DeserializeOwned + Serialize + Default,
    {
        for _ in 0..CAS_ATTEMPTS {
            let loaded = self.load(host).await?;
            let mut corrupt: Option<String> = None;
            let mut value: T = match &loaded.raw {
                None => T::default(),
                Some(raw) => match serde_json::from_str(raw) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::error!(
                            "{}: stored records are unreadable ({e}) — keeping them under \
                             `{}.corrupt` and starting a fresh list",
                            self.label(),
                            self.key
                        );
                        corrupt = Some(raw.clone());
                        T::default()
                    }
                },
            };
            let encode = |v: &T| {
                serde_json::to_string(v).map_err(|e| RecordError::Corrupt {
                    store: self.label(),
                    detail: e.to_string(),
                })
            };
            let before = encode(&value)?;
            let result = f(&mut value);
            let body = encode(&value)?;
            // The closure changed nothing: no write. One that decides inside the
            // swap not to act (a watchdog that lost the race, a refused start) must
            // not bump the resourceVersion and turn every concurrent writer's swap
            // into a conflict for nothing. Compared against the value as parsed,
            // not the stored text, which another writer may have formatted
            // differently. Unreadable content is always rewritten, to move it aside.
            if corrupt.is_none() && before == body {
                return Ok(result);
            }
            let manifest = self.manifest(
                &body,
                corrupt.as_deref(),
                loaded.resource_version.as_deref(),
            );
            // `replace` without a resourceVersion is an unconditional overwrite, not a
            // compare-and-swap — exactly the lost update this module exists to prevent.
            if loaded.exists && loaded.resource_version.is_none() {
                return Err(RecordError::Cluster(CmdError::parse(
                    format!("kubectl get configmap {}", self.name),
                    "no metadata.resourceVersion",
                )));
            }
            let verb = if loaded.exists { "replace" } else { "create" };
            match host.kubectl_write(verb, &manifest.to_string()).await {
                Ok(()) => return Ok(result),
                Err(e) if e.is_conflict() || e.is_already_exists() => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Err(RecordError::Contended {
            store: self.label(),
        })
    }

    fn manifest(&self, body: &str, corrupt: Option<&str>, resource_version: Option<&str>) -> Value {
        let mut data = serde_json::Map::new();
        data.insert(self.key.to_string(), Value::String(body.to_string()));
        if let Some(c) = corrupt {
            data.insert(
                format!("{}.corrupt", self.key),
                Value::String(c.to_string()),
            );
        }
        let mut metadata = json!({
            "name": self.name,
            "namespace": self.namespace,
            "labels": { "app.kubernetes.io/managed-by": "yolab" },
        });
        if let Some(rv) = resource_version {
            metadata["resourceVersion"] = json!(rv);
        }
        json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": metadata,
            "data": data,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::fake::FakeHost;

    const STORE: Store = Store {
        name: "yolab-test",
        namespace: "kube-system",
        key: "sets",
    };

    fn cm(sets: &str, rv: &str) -> String {
        json!({
            "kind": "ConfigMap",
            "metadata": {"resourceVersion": rv},
            "data": {"sets": sets},
        })
        .to_string()
    }

    #[tokio::test]
    async fn a_failed_read_is_an_error_never_an_empty_history() {
        let host = FakeHost::new().fail(
            "kubectl get configmap yolab-test",
            "The connection to the server localhost:6443 was refused",
        );
        let r: Result<Vec<String>, _> = STORE.read(&host).await;
        assert!(r.is_err());

        let wrote = STORE
            .update(&host, |v: &mut Vec<String>| v.push("new".into()))
            .await;
        assert!(wrote.is_err());
        assert!(
            !host.ran("kubectl-replace") && !host.ran("kubectl-create"),
            "nothing may be written after a failed read: {:?}",
            host.calls()
        );
    }

    #[tokio::test]
    async fn a_missing_configmap_is_empty_and_is_created() {
        let host = FakeHost::new()
            .fail(
                "kubectl get configmap yolab-test",
                "Error from server (NotFound): configmaps \"yolab-test\" not found",
            )
            .ok("kubectl-create", "");
        let r: Vec<String> = STORE.read(&host).await.unwrap();
        assert!(r.is_empty());
        STORE
            .update(&host, |v: &mut Vec<String>| v.push("a".into()))
            .await
            .unwrap();
        assert!(host.ran("kubectl-create"));
    }

    #[tokio::test]
    async fn an_update_is_a_compare_and_swap_that_retries_from_a_fresh_read() {
        let host = FakeHost::new()
            .ok("kubectl get configmap yolab-test", &cm(r#"["a"]"#, "1"))
            .ok("kubectl get configmap yolab-test", &cm(r#"["a","b"]"#, "2"))
            .fail(
                "kubectl-replace",
                "Error from server (Conflict): the object has been modified",
            )
            .ok("kubectl-replace", "");
        let mut seen = Vec::new();
        STORE
            .update(&host, |v: &mut Vec<String>| {
                seen.push(v.clone());
                v.push("c".into());
            })
            .await
            .unwrap();
        // The second attempt saw the other writer's "b" and kept it.
        assert_eq!(
            seen.last().unwrap(),
            &vec!["a".to_string(), "b".to_string()]
        );
        let replaces: Vec<String> = host
            .calls()
            .into_iter()
            .filter(|c| c.starts_with("kubectl-replace"))
            .collect();
        assert_eq!(replaces.len(), 2);
        assert!(
            replaces[0].contains(r#"\"resourceVersion\":\"1\""#)
                || replaces[0].contains(r#""resourceVersion":"1""#)
        );
        assert!(
            replaces[1].contains(r#"["a","b","c"]"#)
                || replaces[1].contains(r#"[\"a\",\"b\",\"c\"]"#)
        );
    }

    #[tokio::test]
    async fn unreadable_content_is_kept_aside_rather_than_discarded() {
        let host = FakeHost::new()
            .ok("kubectl get configmap yolab-test", &cm("not json {", "7"))
            .ok("kubectl-replace", "");
        let r: Result<Vec<String>, _> = STORE.read(&host).await;
        assert!(matches!(r, Err(RecordError::Corrupt { .. })));
        STORE
            .update(&host, |v: &mut Vec<String>| v.push("fresh".into()))
            .await
            .unwrap();
        let write = host
            .calls()
            .into_iter()
            .find(|c| c.starts_with("kubectl-replace"))
            .unwrap();
        assert!(write.contains("sets.corrupt"));
        assert!(write.contains("not json {"));
    }

    #[tokio::test]
    async fn an_update_that_changes_nothing_writes_nothing() {
        let host = FakeHost::new()
            .ok("kubectl get configmap yolab-test", &cm(r#"["a"]"#, "1"))
            .ok("kubectl-replace", "");
        let n = STORE
            .update(&host, |v: &mut Vec<String>| v.len())
            .await
            .unwrap();
        assert_eq!(n, 1, "the closure's result is still returned");
        assert!(!host.ran("kubectl-replace"));

        let absent = FakeHost::new()
            .fail(
                "kubectl get configmap yolab-test",
                "Error from server (NotFound): configmaps \"yolab-test\" not found",
            )
            .ok("kubectl-create", "");
        STORE
            .update(&absent, |_: &mut Vec<String>| ())
            .await
            .unwrap();
        assert!(
            !absent.ran("kubectl-create"),
            "an empty record is not worth creating"
        );
    }

    #[tokio::test]
    async fn losing_the_race_to_create_retries_as_a_replace_of_the_winner() {
        let host = FakeHost::new()
            .fail(
                "kubectl get configmap yolab-test",
                "Error from server (NotFound): configmaps \"yolab-test\" not found",
            )
            .ok("kubectl get configmap yolab-test", &cm(r#"["theirs"]"#, "4"))
            .fail(
                "kubectl-create",
                "Error from server (AlreadyExists): configmaps \"yolab-test\" already exists",
            )
            .ok("kubectl-replace", "");
        STORE
            .update(&host, |v: &mut Vec<String>| v.push("ours".into()))
            .await
            .unwrap();
        let replace = host
            .calls()
            .into_iter()
            .find(|c| c.starts_with("kubectl-replace"))
            .expect("retried as a replace");
        assert!(replace.contains("theirs") && replace.contains("ours"), "{replace}");
    }

    #[tokio::test]
    async fn an_answer_that_is_not_a_configmap_is_an_error() {
        let host = FakeHost::new().ok(
            "kubectl get configmap yolab-test",
            r#"{"kind":"Status","metadata":{}}"#,
        );
        let r: Result<Vec<String>, _> = STORE.read(&host).await;
        assert!(matches!(r, Err(RecordError::Cluster(_))));
    }

    #[test]
    fn errors_name_the_store_they_are_about() {
        let corrupt = RecordError::Corrupt {
            store: "kube-system/x".into(),
            detail: "eof".into(),
        };
        assert!(corrupt.to_string().contains("kube-system/x"));
        let contended = RecordError::Contended {
            store: "kube-system/x".into(),
        };
        assert!(contended.to_string().contains("gave up"));
        assert!(std::error::Error::source(&contended).is_none());
        let cluster = RecordError::from(CmdError::parse("kubectl get", "bad"));
        assert!(std::error::Error::source(&cluster).is_some());
    }

    #[tokio::test]
    async fn an_object_without_a_resource_version_is_never_blindly_replaced() {
        let host = FakeHost::new()
            .ok(
                "kubectl get configmap yolab-test",
                r#"{"kind":"ConfigMap","metadata":{},"data":{"sets":"[]"}}"#,
            )
            .ok("kubectl-replace", "");
        let r = STORE
            .update(&host, |v: &mut Vec<String>| v.push("x".into()))
            .await;
        assert!(r.is_err());
        assert!(!host.ran("kubectl-replace"));
    }

    #[tokio::test]
    async fn endless_contention_gives_up_instead_of_overwriting() {
        let host = FakeHost::new()
            .ok("kubectl get configmap yolab-test", &cm("[]", "1"))
            .fail(
                "kubectl-replace",
                "Error from server (Conflict): the object has been modified",
            );
        let r = STORE
            .update(&host, |v: &mut Vec<String>| v.push("x".into()))
            .await;
        assert!(matches!(r, Err(RecordError::Contended { .. })));
    }
}
