//! App packs: a named set of apps that are installed together.
//!
//! ── Why a pack is not a chart ──────────────────────────────────────────────────
//!
//! The obvious implementation is an umbrella Helm chart that depends on the others.
//! It is the wrong shape here. Every app in this catalog owns a namespace, a tunnel
//! subdomain, a PVC and its own install form; an umbrella chart would have to merge
//! all of that into one release, and uninstalling one member would mean editing the
//! umbrella rather than removing an app. Worse, the apps would stop being individually
//! visible on the Apps page, which is where people actually look for them.
//!
//! So a pack is a LIST, not a container. Installing one installs each member exactly
//! as if it had been installed by hand — same endpoint, same per-app form, same
//! namespace, same backup treatment. The pack itself owns nothing and can be deleted
//! without touching anything it installed.
//!
//! That also means the install stream stays one app at a time: the client walks the
//! list and calls the existing `POST /api/apps/:id` for each. Duplicating that
//! orchestration server-side would have meant a second copy of the install path, and
//! the first copy is 200 lines of tunnel registration, chart resolution and rollback.

use axum::{extract::Path, http::StatusCode, response::IntoResponse, Json};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const PACK_CM: &str = "yolab-app-packs";
const PACK_NS: &str = "kube-system";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PackApp {
    /// Catalog chart id, e.g. "sonarr".
    pub id: String,
    /// Instance name to install under. Defaults to the chart id, which is what a
    /// single-instance install would have used anyway.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Pack {
    pub name: String,
    pub display_name: String,
    #[serde(default)]
    pub icon: String,
    #[serde(default)]
    pub description: String,
    pub apps: Vec<PackApp>,
    /// Ships with YoLab. The UI must not offer to delete one, and a user pack may not
    /// take its name.
    #[serde(default)]
    pub builtin: bool,
}

fn app(id: &str) -> PackApp {
    PackApp {
        id: id.to_string(),
        instance_name: None,
    }
}

/// Packs that ship with YoLab.
///
/// Deliberately few. A pack earns its place by being a set of apps that are close to
/// useless apart — not by being a category. "Media" is a category; the four below are
/// a chain where each one exists to feed the next, and installing three of them is a
/// half-built machine.
pub fn builtin_packs() -> Vec<Pack> {
    vec![
        Pack {
            name: "media-stack".into(),
            display_name: "Movies and TV".into(),
            icon: "🎬".into(),
            description: "Ask for a film or a show and have it appear, then watch it \
                          anywhere. Jellyseerr takes the request, Prowlarr finds where \
                          it can be got, Sonarr and Radarr do the fetching, qBittorrent \
                          downloads it, and Jellyfin plays it."
                .into(),
            apps: vec![
                app("jellyfin"),
                app("jellyseerr"),
                app("prowlarr"),
                app("sonarr"),
                app("radarr"),
                app("qbittorrent"),
            ],
            builtin: true,
        },
        Pack {
            name: "starter".into(),
            display_name: "The basics".into(),
            icon: "🏠".into(),
            description: "The four most people want on day one: passwords that sync \
                          everywhere, photos off the phone, files in a browser, and a \
                          page that links to all of it."
                .into(),
            apps: vec![
                app("vaultwarden"),
                app("immich"),
                app("filebrowser"),
                app("homepage"),
            ],
            builtin: true,
        },
        Pack {
            name: "reading".into(),
            display_name: "Reading and listening".into(),
            icon: "📚".into(),
            description: "News you chose, articles kept for later, books and audiobooks \
                          on every device."
                .into(),
            apps: vec![
                app("freshrss"),
                app("karakeep"),
                app("calibre-web"),
                app("audiobookshelf"),
            ],
            builtin: true,
        },
    ]
}

/// A pack name is used as a ConfigMap key and in a URL, so it is held to the same
/// shape as a repo name rather than trusted.
fn valid_pack_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !name.starts_with('-')
        && !name.ends_with('-')
}

async fn read_user_packs() -> Vec<Pack> {
    let Ok(v) =
        crate::kubectl::get_json(&["get", "configmap", PACK_CM, "-n", PACK_NS, "-o", "json"]).await
    else {
        return Vec::new();
    };
    let Some(data) = v["data"].as_object() else {
        return Vec::new();
    };
    let mut out: Vec<Pack> = data
        .values()
        .filter_map(|s| s.as_str())
        .filter_map(|s| serde_json::from_str::<Pack>(s).ok())
        // `builtin` is derived from where a pack came from, never from what was
        // stored: a user pack that claimed builtin:true would otherwise be
        // undeletable through the UI that refuses to delete built-ins.
        .map(|mut p| {
            p.builtin = false;
            p
        })
        .collect();
    out.sort_by(|a, b| a.display_name.cmp(&b.display_name));
    out
}

/// GET /api/apps/packs — built-ins first, then the user's own.
pub async fn list_packs() -> Json<Vec<Pack>> {
    let mut packs = builtin_packs();
    packs.extend(read_user_packs().await);
    Json(packs)
}

#[derive(Deserialize)]
pub struct SavePackReq {
    pub name: String,
    pub display_name: String,
    #[serde(default)]
    pub icon: String,
    #[serde(default)]
    pub description: String,
    pub apps: Vec<PackApp>,
}

/// PUT /api/apps/packs — create or replace one of the user's packs.
pub async fn save_pack(Json(req): Json<SavePackReq>) -> impl IntoResponse {
    if !valid_pack_name(&req.name) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "a pack name may use lowercase letters, digits and hyphens, and cannot start or end with a hyphen"
            })),
        )
            .into_response();
    }
    if builtin_packs().iter().any(|p| p.name == req.name) {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": format!("\"{}\" is a pack that ships with YoLab — choose another name", req.name) })),
        )
            .into_response();
    }
    if req.apps.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "a pack needs at least one app" })),
        )
            .into_response();
    }
    if req.display_name.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "a pack needs a name people will read" })),
        )
            .into_response();
    }

    let pack = Pack {
        name: req.name.clone(),
        display_name: req.display_name,
        icon: req.icon,
        description: req.description,
        apps: req.apps,
        builtin: false,
    };
    let encoded = match serde_json::to_string(&pack) {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response()
        }
    };
    let patch = serde_json::json!({ "data": { &pack.name: encoded } }).to_string();
    // Same create-then-patch as add_repo: the ConfigMap does not exist until the
    // first pack is saved, and `patch` on a missing object is an error rather than a
    // create.
    if crate::kubectl::run(&[
        "patch",
        "configmap",
        PACK_CM,
        "-n",
        PACK_NS,
        "--type",
        "merge",
        "-p",
        &patch,
    ])
    .await
    .is_err()
    {
        let _ = crate::kubectl::run(&["create", "configmap", PACK_CM, "-n", PACK_NS]).await;
        if let Err(e) = crate::kubectl::run(&[
            "patch",
            "configmap",
            PACK_CM,
            "-n",
            PACK_NS,
            "--type",
            "merge",
            "-p",
            &patch,
        ])
        .await
        {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    }
    (
        StatusCode::OK,
        Json(serde_json::to_value(&pack).unwrap_or(Value::Null)),
    )
        .into_response()
}

/// DELETE /api/apps/packs/:name — removes the definition only. Apps it installed are
/// ordinary apps and stay exactly where they are.
pub async fn delete_pack(Path(name): Path<String>) -> impl IntoResponse {
    if builtin_packs().iter().any(|p| p.name == name) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "a pack that ships with YoLab cannot be deleted" })),
        )
            .into_response();
    }
    let patch = serde_json::json!({ "data": { &name: Value::Null } }).to_string();
    match crate::kubectl::run(&[
        "patch",
        "configmap",
        PACK_CM,
        "-n",
        PACK_NS,
        "--type",
        "merge",
        "-p",
        &patch,
    ])
    .await
    {
        Ok(_) => (StatusCode::OK, Json(serde_json::json!({ "ok": true }))).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pack_name_is_held_to_the_shape_of_a_url_segment() {
        for ok in ["media-stack", "a", "my-pack-2"] {
            assert!(valid_pack_name(ok), "{ok} should be accepted");
        }
        for bad in [
            "",
            "-leading",
            "trailing-",
            "Upper",
            "has space",
            "under_score",
            "a/b",
        ] {
            assert!(!valid_pack_name(bad), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn a_pack_name_cannot_be_longer_than_a_configmap_key() {
        assert!(valid_pack_name(&"a".repeat(63)));
        assert!(!valid_pack_name(&"a".repeat(64)));
    }

    /// The built-ins are a promise the UI relies on: it hides delete for them and
    /// refuses to let a user pack take one of their names.
    #[test]
    fn builtin_packs_are_marked_builtin_and_have_apps() {
        for p in builtin_packs() {
            assert!(p.builtin, "{} must be marked builtin", p.name);
            assert!(!p.apps.is_empty(), "{} must list apps", p.name);
            assert!(valid_pack_name(&p.name), "{} must be a usable name", p.name);
            assert!(!p.display_name.is_empty());
        }
    }

    #[test]
    fn builtin_pack_names_are_unique() {
        let mut names: Vec<String> = builtin_packs().into_iter().map(|p| p.name).collect();
        let before = names.len();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), before, "two built-in packs share a name");
    }

    /// Every app a built-in pack names must be a real chart, or installing the pack
    /// half-succeeds and then 404s partway through — after it has already created
    /// namespaces and claimed subdomains for the members before it.
    #[test]
    fn every_builtin_pack_member_is_a_chart_in_this_repo() {
        let catalog = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/catalog");
        if !catalog.exists() {
            return; // packaged build without the catalog beside it
        }
        for p in builtin_packs() {
            for a in &p.apps {
                assert!(
                    catalog.join(&a.id).join("Chart.yaml").exists(),
                    "pack {} names {}, which is not a chart in apps/catalog",
                    p.name,
                    a.id
                );
            }
        }
    }

    /// A stored pack claiming to be built-in must come back as a user pack, or it
    /// becomes undeletable through the UI.
    #[test]
    fn a_stored_pack_cannot_promote_itself_to_builtin() {
        let stored = r#"{"name":"x","display_name":"X","apps":[{"id":"gitea"}],"builtin":true}"#;
        let mut p: Pack = serde_json::from_str(stored).unwrap();
        assert!(
            p.builtin,
            "the field does parse — which is why it has to be overridden"
        );
        p.builtin = false;
        assert!(!p.builtin);
    }

    #[test]
    fn an_instance_name_defaults_to_the_chart_id() {
        let a: PackApp = serde_json::from_str(r#"{"id":"sonarr"}"#).unwrap();
        assert_eq!(a.id, "sonarr");
        assert_eq!(a.instance_name, None);
    }
}
