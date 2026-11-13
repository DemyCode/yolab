use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context as _;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::runtime::{Controller, Ctx, Scope, Tick};
use crate::store::SshKey;

pub const LOGIN_USER: &str = "homelab";
pub const MAX_KEY_LEN: usize = 16 * 1024;
pub const MAX_LABEL_CHARS: usize = 60;
pub const MAX_KEYS: usize = 50;

pub const KEY_TYPES: &[&str] = &[
    "ssh-ed25519",
    "sk-ssh-ed25519@openssh.com",
    "ecdsa-sha2-nistp256",
    "ecdsa-sha2-nistp384",
    "ecdsa-sha2-nistp521",
    "sk-ecdsa-sha2-nistp256@openssh.com",
    "ssh-rsa",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedKey {
    pub kind: String,
    pub blob: Vec<u8>,
    pub comment: String,
}

impl ParsedKey {
    pub fn line(&self) -> String {
        let b64 = base64::engine::general_purpose::STANDARD.encode(&self.blob);
        if self.comment.is_empty() {
            format!("{} {b64}", self.kind)
        } else {
            format!("{} {b64} {}", self.kind, self.comment)
        }
    }

    pub fn id(&self) -> String {
        hex::encode(Sha256::digest(&self.blob))
    }

    pub fn fingerprint(&self) -> String {
        let digest = Sha256::digest(&self.blob);
        format!(
            "SHA256:{}",
            base64::engine::general_purpose::STANDARD_NO_PAD.encode(digest)
        )
    }
}

fn embedded_type(blob: &[u8]) -> Option<&[u8]> {
    let len = u32::from_be_bytes(blob.get(..4)?.try_into().ok()?) as usize;
    blob.get(4..4usize.checked_add(len)?)
}

pub fn parse_key(text: &str) -> Result<ParsedKey, String> {
    let text = text.trim();
    if text.contains("PRIVATE KEY") {
        return Err(
            "This is a private key: keep it secret and never paste it anywhere. Paste the \
             matching public key instead, the file that ends in .pub"
                .into(),
        );
    }
    if text.is_empty() {
        return Err("Paste a public key: it starts with ssh-ed25519, ecdsa-sha2 or ssh-rsa".into());
    }
    if text.len() > MAX_KEY_LEN {
        return Err("This is too long to be one public key".into());
    }
    if text.chars().any(|c| c.is_control() && c != '\t') {
        return Err("Paste one key at a time, on a single line".into());
    }
    let mut parts = text.split_whitespace();
    let kind = parts.next().unwrap_or_default();
    if !KEY_TYPES.contains(&kind) {
        return Err(format!(
            "A public key starts with its type ({}), not {kind:?}",
            KEY_TYPES.join(", ")
        ));
    }
    let blob = parts
        .next()
        .and_then(|b64| base64::engine::general_purpose::STANDARD.decode(b64).ok())
        .ok_or_else(|| "The key itself is missing or damaged; copy the whole line again".to_string())?;
    if embedded_type(&blob) != Some(kind.as_bytes()) {
        return Err(format!(
            "The key does not match its type {kind}; copy the whole line again"
        ));
    }
    let comment = parts.collect::<Vec<_>>().join(" ");
    Ok(ParsedKey {
        kind: kind.to_string(),
        blob,
        comment,
    })
}

pub fn clean_label(label: &str) -> Result<String, String> {
    let label = label.trim();
    if label.chars().count() > MAX_LABEL_CHARS {
        return Err(format!("A name is at most {MAX_LABEL_CHARS} characters"));
    }
    if label.chars().any(char::is_control) {
        return Err("A name is a single line of text".into());
    }
    Ok(label.to_string())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct KeyView {
    pub id: String,
    pub kind: String,
    pub fingerprint: String,
    pub comment: String,
    pub label: String,
}

pub fn active(keys: &std::collections::BTreeMap<String, crate::store::entry::Entry<SshKey>>) -> Vec<(String, ParsedKey, String)> {
    keys.iter()
        .filter(|(_, e)| !e.value.removed)
        .filter_map(|(id, e)| match parse_key(&e.value.line) {
            Ok(parsed) => Some((id.clone(), parsed, e.value.label.clone())),
            Err(why) => {
                tracing::warn!("ssh-keys: stored key {id} is not a public key ({why}); left out");
                None
            }
        })
        .collect()
}

pub fn render(keys: &[(String, ParsedKey, String)]) -> String {
    keys.iter().map(|(_, k, _)| format!("{}\n", k.line())).collect()
}

pub fn views(keys: &[(String, ParsedKey, String)]) -> Vec<KeyView> {
    keys.iter()
        .map(|(id, k, label)| KeyView {
            id: id.clone(),
            kind: k.kind.clone(),
            fingerprint: k.fingerprint(),
            comment: k.comment.clone(),
            label: label.clone(),
        })
        .collect()
}

pub fn keys_file() -> PathBuf {
    std::env::var("YOLAB_SSH_KEYS_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(format!("/var/lib/yolab-ssh/{LOGIN_USER}.keys")))
}

pub fn sync_file(path: &Path, wanted: &str) -> anyhow::Result<bool> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    match std::fs::read_to_string(path) {
        Ok(current) if current == wanted => return Ok(false),
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    }
    let dir = path.parent().context("a keys file without a directory")?;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let tmp = path.with_extension("tmp");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o644)
        .open(&tmp)
        .with_context(|| format!("open {}", tmp.display()))?;
    file.write_all(wanted.as_bytes())?;
    file.sync_all()?;
    std::fs::rename(&tmp, path).with_context(|| format!("replace {}", path.display()))?;
    Ok(true)
}

fn current_keys() -> anyhow::Result<Vec<(String, ParsedKey, String)>> {
    let keys = crate::store::locked().ssh_keys()?;
    Ok(active(&keys))
}

fn apply() -> anyhow::Result<bool> {
    let wanted = render(&current_keys()?);
    sync_file(&keys_file(), &wanted)
}

pub struct SshKeysController;

impl Controller for SshKeysController {
    fn name(&self) -> &'static str {
        "ssh-keys"
    }

    fn scope(&self) -> Scope {
        Scope::Node
    }

    fn interval(&self) -> Duration {
        Duration::from_secs(30)
    }

    async fn reconcile(&self, _ctx: &Ctx) -> anyhow::Result<Tick> {
        if apply()? {
            Ok(Tick::Done)
        } else {
            Ok(Tick::Idle("the SSH keys on this machine are current".into()))
        }
    }
}

#[derive(Serialize)]
pub struct KeysResponse {
    pub user: &'static str,
    pub port: Option<i64>,
    pub keys: Vec<KeyView>,
}

fn ssh_port(cfg: &crate::config::Config) -> Option<i64> {
    cfg.toml()?.get("homelab")?.get("ssh_port")?.as_integer()
}

fn listing(cfg: &crate::config::Config) -> crate::error::Result<Json<KeysResponse>> {
    Ok(Json(KeysResponse {
        user: LOGIN_USER,
        port: ssh_port(cfg),
        keys: views(&current_keys()?),
    }))
}

fn refused(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(serde_json::json!({ "error": message.into() }))).into_response()
}

fn saved(store: &mut crate::store::Store) -> anyhow::Result<()> {
    store.persist(&crate::store::default_path())?;
    crate::runtime::wake("store-sync");
    Ok(())
}

pub async fn list(State(state): State<crate::AppState>) -> crate::error::Result<Json<KeysResponse>> {
    listing(&state.config)
}

#[derive(Deserialize)]
pub struct AddKey {
    pub key: String,
    #[serde(default)]
    pub label: String,
}

pub async fn add(State(state): State<crate::AppState>, Json(body): Json<AddKey>) -> Response {
    let parsed = match parse_key(&body.key) {
        Ok(p) => p,
        Err(why) => return refused(StatusCode::BAD_REQUEST, why),
    };
    let label = match clean_label(&body.label) {
        Ok(l) => l,
        Err(why) => return refused(StatusCode::BAD_REQUEST, why),
    };
    let stored = (|| -> anyhow::Result<Option<Response>> {
        let mut store = crate::store::locked();
        let held = active(&store.ssh_keys()?);
        if held.len() >= MAX_KEYS && !held.iter().any(|(id, _, _)| *id == parsed.id()) {
            return Ok(Some(refused(
                StatusCode::CONFLICT,
                format!("At most {MAX_KEYS} keys can be added; remove one you no longer use"),
            )));
        }
        store.set_ssh_key(
            &parsed.id(),
            &SshKey {
                line: parsed.line(),
                label,
                removed: false,
            },
        )?;
        saved(&mut store)?;
        Ok(None)
    })();
    match stored {
        Ok(Some(refusal)) => return refusal,
        Ok(None) => {}
        Err(e) => return crate::error::AppError(e).into_response(),
    }
    if let Err(e) = apply() {
        return crate::error::AppError(e).into_response();
    }
    listing(&state.config).into_response()
}

#[derive(Deserialize)]
pub struct RemoveKey {
    pub id: String,
}

pub async fn remove(State(state): State<crate::AppState>, Json(body): Json<RemoveKey>) -> Response {
    let removed = (|| -> anyhow::Result<bool> {
        let mut store = crate::store::locked();
        let Some(entry) = store.ssh_keys()?.remove(&body.id) else {
            return Ok(false);
        };
        if entry.value.removed {
            return Ok(false);
        }
        store.set_ssh_key(
            &body.id,
            &SshKey {
                removed: true,
                ..entry.value
            },
        )?;
        saved(&mut store)?;
        Ok(true)
    })();
    match removed {
        Ok(true) => {}
        Ok(false) => return refused(StatusCode::NOT_FOUND, "No such key on these machines"),
        Err(e) => return crate::error::AppError(e).into_response(),
    }
    if let Err(e) = apply() {
        return crate::error::AppError(e).into_response();
    }
    listing(&state.config).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    fn ed25519() -> String {
        format!("{} me@laptop", key_of("ssh-ed25519", &[5; 32]))
    }

    fn key_of(kind: &str, payload: &[u8]) -> String {
        let mut blob = (kind.len() as u32).to_be_bytes().to_vec();
        blob.extend_from_slice(kind.as_bytes());
        blob.extend_from_slice(payload);
        format!(
            "{kind} {}",
            base64::engine::general_purpose::STANDARD.encode(blob)
        )
    }

    #[test]
    fn an_ed25519_public_key_is_accepted_with_its_comment() {
        let k = parse_key(&ed25519()).unwrap();
        assert_eq!(k.kind, "ssh-ed25519");
        assert_eq!(k.comment, "me@laptop");
        assert_eq!(k.line(), ed25519());
        assert!(k.fingerprint().starts_with("SHA256:"));
        assert_eq!(k.id().len(), 64);
    }

    #[test]
    fn surrounding_space_and_a_trailing_newline_are_forgiven() {
        assert_eq!(parse_key(&format!("  {}\n", ed25519())).unwrap().line(), ed25519());
    }

    #[test]
    fn a_private_key_is_refused_with_a_warning_to_keep_it_secret() {
        let why = parse_key("-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEA\n-----END OPENSSH PRIVATE KEY-----").unwrap_err();
        assert!(why.contains("private key"), "{why}");
    }

    #[test]
    fn two_keys_at_once_are_refused() {
        let why = parse_key(&format!("{}\n{}", ed25519(), ed25519())).unwrap_err();
        assert!(why.contains("one key"), "{why}");
    }

    #[test]
    fn options_in_front_of_the_key_are_refused() {
        assert!(parse_key(&format!("command=\"/bin/sh\" {}", ed25519())).is_err());
        assert!(parse_key(&format!("from=\"*\" {}", ed25519())).is_err());
    }

    #[test]
    fn a_key_whose_body_does_not_match_its_type_is_refused() {
        let rsa_body = key_of("ssh-rsa", &[1, 2, 3]);
        let lying = rsa_body.replacen("ssh-rsa", "ssh-ed25519", 1);
        assert!(parse_key(&lying).is_err());
        assert!(parse_key(&rsa_body).is_ok());
    }

    #[test]
    fn junk_is_refused() {
        for junk in ["", "   ", "hello", "ssh-ed25519", "ssh-ed25519 !!!notbase64", "ssh-dss AAAA"] {
            assert!(parse_key(junk).is_err(), "{junk:?}");
        }
        assert!(parse_key(&"a".repeat(MAX_KEY_LEN + 1)).is_err());
    }

    #[test]
    fn the_file_line_is_rebuilt_so_a_comment_cannot_add_a_second_key() {
        let k = parse_key(&format!("{}  first   second", key_of("ssh-ed25519", &[7; 32]))).unwrap();
        assert_eq!(k.comment, "first second");
        assert_eq!(k.line().lines().count(), 1);
    }

    #[test]
    fn a_label_is_a_short_single_line() {
        assert_eq!(clean_label("  My laptop ").unwrap(), "My laptop");
        assert!(clean_label(&"x".repeat(MAX_LABEL_CHARS + 1)).is_err());
        assert!(clean_label("a\nb").is_err());
    }

    #[test]
    fn a_removed_key_leaves_the_file_and_the_others_stay() {
        let mut store = Store::new("node1");
        let a = parse_key(&ed25519()).unwrap();
        let b = parse_key(&key_of("ssh-ed25519", &[9; 32])).unwrap();
        for k in [&a, &b] {
            store
                .set_ssh_key(&k.id(), &SshKey { line: k.line(), label: String::new(), removed: false })
                .unwrap();
        }
        store
            .set_ssh_key(&a.id(), &SshKey { line: a.line(), label: String::new(), removed: true })
            .unwrap();
        let file = render(&active(&store.ssh_keys().unwrap()));
        assert_eq!(file, format!("{}\n", b.line()));
    }

    #[test]
    fn a_key_added_on_one_machine_reaches_the_file_on_another() {
        let mut one = Store::new("node1");
        let k = parse_key(&ed25519()).unwrap();
        one.set_ssh_key(&k.id(), &SshKey { line: k.line(), label: "laptop".into(), removed: false })
            .unwrap();
        let mut two = Store::new("node2");
        let mut copy = Store::load("node1", &one.save()).unwrap();
        two.merge(&mut copy).unwrap();
        assert_eq!(render(&active(&two.ssh_keys().unwrap())), format!("{}\n", ed25519()));
        assert_eq!(views(&active(&two.ssh_keys().unwrap()))[0].label, "laptop");
    }

    #[test]
    fn the_keys_file_is_written_readable_by_sshd_and_only_when_it_changed() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ssh/homelab.keys");
        assert!(sync_file(&path, "a\n").unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\n");
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o644);
        assert!(!sync_file(&path, "a\n").unwrap());
        assert!(sync_file(&path, "").unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
    }
}
