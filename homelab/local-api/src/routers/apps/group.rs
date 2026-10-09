use super::*;

use crate::groups::{Joining, Membership};
use crate::setups::{CatalogSetup, Member, Setup};

#[derive(Deserialize)]
pub struct GroupRequest {
    #[serde(default)]
    pub group: Option<Joining>,
}

pub async fn set_group(
    State(state): State<AppState>,
    Path(instance_name): Path<String>,
    Json(body): Json<GroupRequest>,
) -> axum::response::Response {
    let membership = match body.group.as_ref().map(Joining::membership).transpose() {
        Ok(m) => m,
        Err(why) => return (StatusCode::BAD_REQUEST, why).into_response(),
    };
    let client = match state.kube.client().await {
        Ok(c) => c,
        Err(e) => return (StatusCode::SERVICE_UNAVAILABLE, format!("{e:#}")).into_response(),
    };
    let ns = format!("yolab-{instance_name}");
    match namespace(&client, &ns).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            return (StatusCode::NOT_FOUND, format!("{instance_name} is not installed"))
                .into_response()
        }
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
    }
    match crate::groups::set(&client, &ns, membership.as_ref()).await {
        Ok(()) => Json(serde_json::json!({ "group": membership })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
    }
}

pub(crate) async fn join_on_install(client: &Client, ns: &str, joining: Option<&Membership>) {
    if let Some(m) = joining {
        crate::groups::set(client, ns, Some(m))
            .await
            .warn_on_err(format!("{ns}: could not put it in the group {}", m.title));
    }
}

pub async fn export_group(
    State(state): State<AppState>,
    Path(group): Path<String>,
) -> axum::response::Response {
    if !crate::folders::is_name(&group) {
        return (StatusCode::BAD_REQUEST, format!("{group:?} is not a group name")).into_response();
    }
    let client = match state.kube.client().await {
        Ok(c) => c,
        Err(e) => return (StatusCode::SERVICE_UNAVAILABLE, format!("{e:#}")).into_response(),
    };
    let in_group = kube::api::ListParams::default()
        .labels(&format!("{}={group}", crate::groups::LABEL_GROUP));
    let namespaces = match crate::k8s::list(&client, "v1", "Namespace", None, &in_group).await {
        Ok(found) => found,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
    };
    let members = crate::groups::members(&namespaces, &group);
    let Some(title) = members
        .first()
        .and_then(|ns| crate::groups::group_of(ns))
        .map(|m| m.title)
    else {
        return (StatusCode::NOT_FOUND, format!("no app is in the group {group}")).into_response();
    };
    let catalog = state.config.catalog_dir();
    let mut exported = Vec::new();
    for ns in members {
        let Some(name) = ns["metadata"]["name"].as_str() else {
            continue;
        };
        let ann = ns["metadata"]["annotations"]
            .as_object()
            .cloned()
            .unwrap_or_default();
        let Some(app_id) = ann.get(ANN_APP_ID).and_then(Value::as_str) else {
            continue;
        };
        let app = installed_schema(&client, name, app_id, &catalog).await;
        exported.push(Member {
            instance: name.trim_start_matches("yolab-").to_string(),
            app_id: app_id.to_string(),
            main: crate::groups::group_of(ns).is_some_and(|m| m.main),
            settings: without_redacted(&saved_settings(&ann)),
            folder_fields: crate::folders::folder_fields(&app.config()),
            credentials: app.credentials(),
        });
    }
    let titles = crate::folders::list(&client)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|f| (f.name, f.title))
        .collect();
    match crate::setups::export(&title, &exported, &titles).to_yaml() {
        Ok(text) => (
            [
                (axum::http::header::CONTENT_TYPE, "application/yaml".to_string()),
                (
                    axum::http::header::CONTENT_DISPOSITION,
                    format!("attachment; filename=\"{group}.yaml\""),
                ),
            ],
            text,
        )
            .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
    }
}

pub async fn list_setups(State(state): State<AppState>) -> Result<Json<Vec<CatalogSetup>>> {
    let client = state.kube.client().await?;
    let mut seen = std::collections::BTreeSet::new();
    let mut setups = Vec::new();
    for (_, dir) in crate::charts::chart_sources(&client).await {
        for setup in crate::setups::kept(&dir) {
            if seen.insert(setup.id.clone()) {
                setups.push(setup);
            }
        }
    }
    Ok(Json(setups))
}

#[derive(Deserialize)]
pub struct ParseRequest {
    pub text: String,
}

pub async fn parse_setup(Json(body): Json<ParseRequest>) -> axum::response::Response {
    match Setup::parse(&body.text) {
        Ok(setup) => Json(setup).into_response(),
        Err(why) => (StatusCode::BAD_REQUEST, why).into_response(),
    }
}
