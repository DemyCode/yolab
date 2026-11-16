use std::time::Duration;

use kube::Client;
use serde_json::{json, Value};

use crate::error::Outcome;
use crate::routers::backup_common::{apply_secret, random_hex, BackupConfig};

pub(crate) const IMAGE: &str =
    "restic/restic:0.19.1@sha256:136600b6ff6843d61d355f7f71f460a166429f35de6fd11b568fece3c9a4d510";
pub(crate) const FOLDERS_TAG: &str = "yolab-folders";
pub(crate) const KEEP_TAG: &str = "yolab-keep";
pub(crate) const RUN_TAG_PREFIX: &str = "run:";
const DATA: &str = "/data";
const CACHE: &str = "/cache";
const HOST: &str = "yolab";
const SAVED_PREFIX: &str = "YOLAB_SNAPSHOT ";
const RESTORED: &str = "YOLAB_RESTORED";
const BACKUP_DEADLINE: Duration = Duration::from_secs(24 * 3600);
const RESTORE_DEADLINE: Duration = Duration::from_secs(24 * 3600);

pub(crate) fn store(namespace: &str) -> String {
    format!("services/{namespace}")
}

pub(crate) fn run_tag(run_id: &str) -> String {
    format!("{RUN_TAG_PREFIX}{run_id}")
}

pub(crate) fn run_of(tags: &[String]) -> Option<&str> {
    tags.iter().find_map(|t| t.strip_prefix(RUN_TAG_PREFIX))
}

pub(crate) fn path_in_snapshot(folder: &str) -> String {
    if folder.is_empty() {
        DATA.to_string()
    } else {
        format!("{DATA}/{folder}")
    }
}

fn as_arg(folder: &str) -> String {
    format!("/{folder}")
}

const BACKUP_SCRIPT: &str = r#"set -u
run="$1"
shift
tree="$TREE"
: > /tmp/paths
: > /tmp/mounts
: > /tmp/snaps
release() {
  while IFS= read -r m; do
    umount "$m" || echo "could not unmount $m" >&2
  done < /tmp/mounts
  : > /tmp/mounts
  while IFS= read -r s; do
    rmdir "$s" || echo "could not drop the snapshot $s" >&2
  done < /tmp/snaps
  : > /tmp/snaps
}
trap release EXIT
for arg in "$@"; do
  f="${arg#/}"
  src="$tree${f:+/$f}"
  dst="$DATA${f:+/$f}"
  if [ ! -d "$src" ]; then
    echo "the folder /$f is missing" >&2
    exit 2
  fi
  snap="$src/.snap/$SNAP"
  mkdir "$snap" || exit 2
  { echo "$snap"; cat /tmp/snaps; } > /tmp/snaps.new && mv /tmp/snaps.new /tmp/snaps
  mkdir -p "$dst" || exit 2
  mount -o bind "$snap" "$dst" || exit 2
  { echo "$dst"; cat /tmp/mounts; } > /tmp/mounts.new && mv /tmp/mounts.new /tmp/mounts
  echo "$dst" >> /tmp/paths
done
restic cat config > /dev/null 2> /tmp/opened
opened=$?
if [ "$opened" -eq 10 ]; then
  restic init || exit 2
elif [ "$opened" -ne 0 ]; then
  cat /tmp/opened >&2
  exit 2
fi
restic backup --host "$HOSTNAME_TAG" --tag "$FOLDERS_TAG" --tag "$run" --json \
  --files-from-verbatim /tmp/paths > /tmp/saved.json
saved=$?
if [ "$saved" -ne 0 ]; then
  grep -v '"message_type":"status"' /tmp/saved.json | tail -n 5 >&2
  echo "restic could not save the folders (exit $saved)" >&2
  exit 2
fi
id=$(sed -n 's/.*"message_type":"summary".*"snapshot_id":"\([0-9a-f]*\)".*/\1/p' /tmp/saved.json | tail -n 1)
if [ -z "$id" ]; then
  echo "restic finished but named no snapshot" >&2
  exit 2
fi
release
trap - EXIT
echo "YOLAB_SNAPSHOT $id"
"#;

const RESTORE_SCRIPT: &str = r#"set -u
id="$1"
shift
tree="$TREE"
while [ "$#" -ge 3 ]; do
  from="$1"
  f="${2#/}"
  mode="$3"
  shift 3
  dst="$tree${f:+/$f}"
  mkdir -p "$dst" || exit 2
  if [ "$mode" = replace ]; then
    restic restore "$id:$from" --target "$dst" --delete || exit 2
  else
    restic restore "$id:$from" --target "$dst" || exit 2
  fi
done
echo "YOLAB_RESTORED"
"#;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FolderRestore {
    pub from: String,
    pub to: String,
    pub replace: bool,
}

fn backup_args(run_id: &str, folders: &[String]) -> Vec<String> {
    std::iter::once(run_tag(run_id))
        .chain(folders.iter().map(|f| as_arg(f)))
        .collect()
}

fn restore_args(snapshot_id: &str, items: &[FolderRestore]) -> Vec<String> {
    std::iter::once(snapshot_id.to_string())
        .chain(items.iter().flat_map(|i| {
            [
                i.from.clone(),
                as_arg(&i.to),
                if i.replace { "replace" } else { "merge" }.to_string(),
            ]
        }))
        .collect()
}

fn checked(folders: &[&str]) -> anyhow::Result<()> {
    for f in folders {
        if f.contains('\n') || crate::tree::clean_path(f).as_deref() != Some(*f) {
            anyhow::bail!("{f:?} is not a folder in the file tree");
        }
    }
    Ok(())
}

fn job(name: &str, script: &str, args: Vec<String>, deadline: Duration) -> Value {
    let mut command = vec![
        "sh".to_string(),
        "-c".to_string(),
        script.to_string(),
        "backup".to_string(),
    ];
    command.extend(args);
    let mount = crate::tree::JOB_MOUNT;
    json!({
        "apiVersion": "batch/v1",
        "kind": "Job",
        "metadata": {
            "name": name,
            "namespace": crate::csi::NS,
            "labels": { "app.kubernetes.io/managed-by": "yolab" },
        },
        "spec": {
            "backoffLimit": 0,
            "activeDeadlineSeconds": deadline.as_secs(),
            "ttlSecondsAfterFinished": crate::tree::JOB_KEEP_FINISHED_SECS,
            "template": {
                "spec": {
                    "restartPolicy": "Never",
                    "securityContext": { "runAsUser": 0, "runAsGroup": 0 },
                    "containers": [{
                        "name": crate::tree::JOB_CONTAINER,
                        "image": IMAGE,
                        "command": command,
                        "envFrom": [{ "secretRef": { "name": name } }],
                        "env": [
                            { "name": "SNAP", "value": name },
                            { "name": "TREE", "value": format!("{mount}{}", crate::tree::ROOT) },
                            { "name": "DATA", "value": DATA },
                            { "name": "HOSTNAME_TAG", "value": HOST },
                            { "name": "FOLDERS_TAG", "value": FOLDERS_TAG },
                            { "name": "RESTIC_CACHE_DIR", "value": CACHE },
                        ],
                        "securityContext": { "privileged": true },
                        "volumeMounts": [
                            { "name": "tree", "mountPath": mount },
                            { "name": "cache", "mountPath": CACHE },
                        ],
                    }],
                    "volumes": [
                        {
                            "name": "tree",
                            "persistentVolumeClaim": { "claimName": crate::tree::JOB_VOLUME },
                        },
                        { "name": "cache", "emptyDir": {} },
                    ],
                },
            },
        },
    })
}

async fn with_credentials<T>(
    client: &Client,
    cfg: &BackupConfig,
    namespace: &str,
    name: &str,
    run: impl std::future::Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    let repo = cfg.restic_repo(&store(namespace));
    apply_secret(
        client,
        name,
        crate::csi::NS,
        &[
            ("RESTIC_REPOSITORY", &repo),
            ("RESTIC_PASSWORD", &cfg.restic_password),
            ("AWS_ACCESS_KEY_ID", &cfg.access_key_id),
            ("AWS_SECRET_ACCESS_KEY", &cfg.secret_access_key),
        ],
    )
    .await?;
    let result = run.await;
    let secret = crate::k8s::reference("v1", "Secret", crate::csi::NS, name);
    crate::k8s::delete_if_present(client, &secret)
        .await
        .warn_on_err(format!("{name}: delete its backup credentials"));
    result
}

pub(crate) fn saved_id(line: Option<&str>) -> Option<String> {
    let id = line?.trim().strip_prefix(SAVED_PREFIX)?.trim();
    (!id.is_empty() && id.bytes().all(|b| b.is_ascii_hexdigit())).then(|| id.to_string())
}

pub(crate) async fn back_up(
    client: &Client,
    cfg: &BackupConfig,
    namespace: &str,
    run_id: &str,
    folders: &[String],
) -> anyhow::Result<String> {
    checked(&folders.iter().map(String::as_str).collect::<Vec<_>>())?;
    if folders.is_empty() {
        anyhow::bail!("{namespace} has no folder in the file tree to back up");
    }
    let name = format!("yolab-backup-{}", random_hex(4));
    let manifest = job(
        &name,
        BACKUP_SCRIPT,
        backup_args(run_id, folders),
        BACKUP_DEADLINE,
    );
    let what = format!("back up the folders of {namespace}");
    with_credentials(client, cfg, namespace, &name, async {
        crate::tree::run_job(client, &name, &manifest, &what, BACKUP_DEADLINE).await?;
        let said = crate::tree::job_last_line(client, &name).await?;
        saved_id(said.as_deref()).ok_or_else(|| {
            anyhow::anyhow!(
                "{what}: it finished but did not say which snapshot it saved (last words: {})",
                said.as_deref().unwrap_or("none")
            )
        })
    })
    .await
}

pub(crate) async fn restore(
    client: &Client,
    cfg: &BackupConfig,
    source_namespace: &str,
    snapshot_id: &str,
    items: &[FolderRestore],
) -> anyhow::Result<()> {
    if items.is_empty() {
        return Ok(());
    }
    if !snapshot_id.bytes().all(|b| b.is_ascii_hexdigit()) || snapshot_id.is_empty() {
        anyhow::bail!("{snapshot_id:?} is not a snapshot id");
    }
    checked(&items.iter().map(|i| i.to.as_str()).collect::<Vec<_>>())?;
    if let Some(bad) = items
        .iter()
        .find(|i| i.from.contains('\n') || !i.from.starts_with(DATA))
    {
        anyhow::bail!("{:?} is not a folder of a backup", bad.from);
    }
    let name = format!("yolab-restore-{}", random_hex(4));
    let manifest = job(
        &name,
        RESTORE_SCRIPT,
        restore_args(snapshot_id, items),
        RESTORE_DEADLINE,
    );
    let what = format!("restore the folders of {source_namespace}");
    with_credentials(client, cfg, source_namespace, &name, async {
        crate::tree::run_job(client, &name, &manifest, &what, RESTORE_DEADLINE).await?;
        match crate::tree::job_last_line(client, &name).await? {
            Some(line) if line.trim() == RESTORED => Ok(()),
            other => anyhow::bail!(
                "{what}: it finished without saying everything was restored (last words: {})",
                other.as_deref().unwrap_or("none")
            ),
        }
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(job: &Value) -> Vec<String> {
        job["spec"]["template"]["spec"]["containers"][0]["command"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn each_service_has_its_own_store() {
        assert_eq!(store("yolab-immich-ab12"), "services/yolab-immich-ab12");
        assert_ne!(store("yolab-a"), store("yolab-b"));
    }

    #[test]
    fn a_folder_sits_at_the_same_place_in_every_snapshot_so_restic_finds_its_parent() {
        assert_eq!(path_in_snapshot("immich-ab12"), "/data/immich-ab12");
        assert_eq!(path_in_snapshot("photos/2024"), "/data/photos/2024");
        assert_eq!(path_in_snapshot(""), "/data");
    }

    #[test]
    fn folders_reach_the_script_as_arguments_never_as_script_text() {
        let folders = vec!["notes-1".to_string(), "".to_string(), "a b".to_string()];
        let j = job(
            "yolab-backup-x",
            BACKUP_SCRIPT,
            backup_args("bk-1", &folders),
            BACKUP_DEADLINE,
        );
        let cmd = command(&j);
        assert_eq!(&cmd[..2], &["sh", "-c"]);
        assert!(!cmd[2].contains("notes-1") && !cmd[2].contains("a b"));
        assert_eq!(&cmd[4..], &["run:bk-1", "/notes-1", "/", "/a b"]);
    }

    #[test]
    fn the_backup_reads_a_snapshot_named_after_its_job_never_the_live_folder() {
        assert!(BACKUP_SCRIPT.contains("snap=\"$src/.snap/$SNAP\""));
        assert!(BACKUP_SCRIPT.contains("mount -o bind \"$snap\" \"$dst\""));
        let j = job("yolab-backup-x", BACKUP_SCRIPT, vec![], BACKUP_DEADLINE);
        let env = &j["spec"]["template"]["spec"]["containers"][0]["env"];
        assert!(env
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["name"] == "SNAP" && e["value"] == "yolab-backup-x"));
    }

    #[test]
    fn the_snapshots_are_released_even_when_the_backup_fails() {
        assert!(BACKUP_SCRIPT.contains("trap release EXIT"));
        let last_exit = BACKUP_SCRIPT.rfind("exit 2").unwrap();
        assert!(BACKUP_SCRIPT.find("trap release EXIT").unwrap() < last_exit);
    }

    #[test]
    fn a_missing_store_is_created_but_an_unreachable_one_fails_the_backup() {
        assert!(BACKUP_SCRIPT.contains("if [ \"$opened\" -eq 10 ]; then\n  restic init"));
        assert!(BACKUP_SCRIPT.contains("elif [ \"$opened\" -ne 0 ]; then"));
    }

    #[test]
    fn the_job_says_which_snapshot_it_saved_on_its_last_line() {
        assert_eq!(
            saved_id(Some("YOLAB_SNAPSHOT 4f3a9c01")),
            Some("4f3a9c01".to_string())
        );
        assert_eq!(saved_id(Some("could not unmount /data/x")), None);
        assert_eq!(saved_id(Some("YOLAB_SNAPSHOT ")), None);
        assert_eq!(saved_id(Some("YOLAB_SNAPSHOT nothex!")), None);
        assert_eq!(saved_id(None), None);
    }

    #[test]
    fn a_restore_names_where_each_folder_comes_from_and_whether_it_replaces() {
        let items = vec![
            FolderRestore {
                from: "/data/notes-1".into(),
                to: "notes-2".into(),
                replace: true,
            },
            FolderRestore {
                from: "/data".into(),
                to: "".into(),
                replace: false,
            },
        ];
        assert_eq!(
            restore_args("abc123", &items),
            vec![
                "abc123",
                "/data/notes-1",
                "/notes-2",
                "replace",
                "/data",
                "/",
                "merge"
            ]
        );
        assert!(RESTORE_SCRIPT.contains("--delete"));
    }

    #[test]
    fn the_job_can_take_a_snapshot_but_never_reowns_the_tree() {
        let j = job("x", BACKUP_SCRIPT, vec![], BACKUP_DEADLINE);
        let spec = &j["spec"]["template"]["spec"];
        assert_eq!(spec["containers"][0]["securityContext"]["privileged"], true);
        assert!(spec["securityContext"]["fsGroup"].is_null());
        assert_eq!(j["spec"]["backoffLimit"], 0);
    }

    #[test]
    fn only_clean_folders_reach_a_job() {
        assert!(checked(&["notes", "", "a/b"]).is_ok());
        for bad in ["../etc", "a//b", "a\nb", "/abs"] {
            assert!(checked(&[bad]).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_run_is_found_again_from_its_tag() {
        let tags = vec!["yolab-folders".to_string(), "run:bk-9".to_string()];
        assert_eq!(run_of(&tags), Some("bk-9"));
        assert_eq!(run_of(&["cluster-backup".to_string()]), None);
    }
}
