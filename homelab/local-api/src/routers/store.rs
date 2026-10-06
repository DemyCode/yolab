use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use kube::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::Config;
use crate::AppState;

const SETTINGS_CM: &str = "yolab-store-settings";
const SETTINGS_NS: &str = "kube-system";
const SHARE_INSTALLS_KEY: &str = "share_installs";
const PLATFORM_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Verb {
    Get,
    Put,
    Post,
    Delete,
}

pub(crate) struct Platform {
    pub(crate) api: String,
    pub(crate) token: String,
}

pub(crate) fn platform(cfg: &Config) -> Option<Platform> {
    let api = cfg
        .tunnel_table()?
        .get("platform_api_url")?
        .as_str()?
        .trim_end_matches('/')
        .to_string();
    let token = crate::config::read_account_token(&cfg.config_path);
    (!api.is_empty()).then_some(Platform { api, token })
}

pub(crate) async fn call(
    http: &crate::http::Client,
    platform: &Platform,
    verb: Verb,
    path: &str,
    body: Option<&Value>,
) -> anyhow::Result<(u16, Value)> {
    let url = format!("{}{path}", platform.api);
    let request = match verb {
        Verb::Get => http.get(url),
        Verb::Put => http.put(url),
        Verb::Post => http.post(url),
        Verb::Delete => http.delete(url),
    };
    let request = request
        .bearer_auth(&platform.token)
        .timeout(PLATFORM_TIMEOUT);
    let request = match body {
        Some(b) => request.json(b),
        None => request,
    };
    let response = request.send().await?;
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    let value = serde_json::from_str(&text).unwrap_or(match text.trim() {
        "" => Value::Null,
        other => json!({ "detail": other }),
    });
    Ok((status, value))
}

pub(crate) fn answer(outcome: anyhow::Result<(u16, Value)>) -> Response {
    match outcome {
        Ok((status, body)) => {
            let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
            if status == StatusCode::NO_CONTENT {
                status.into_response()
            } else {
                (status, Json(body)).into_response()
            }
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "detail": format!("the YoLab platform did not answer: {e}") })),
        )
            .into_response(),
    }
}

fn not_connected() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({ "detail": "this server is not connected to the YoLab platform" })),
    )
        .into_response()
}

pub(crate) fn valid_app_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 63
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

async fn forward(state: &AppState, verb: Verb, path: String, body: Option<Value>) -> Response {
    let Some(platform) = platform(&state.config) else {
        return not_connected();
    };
    answer(call(&state.http, &platform, verb, &path, body.as_ref()).await)
}

fn bad_app() -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "detail": "that is not an app id" })),
    )
        .into_response()
}

pub async fn stats(State(state): State<AppState>) -> Response {
    forward(&state, Verb::Get, "/catalog/stats".into(), None).await
}

pub async fn my_rating(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    if !valid_app_id(&id) {
        return bad_app();
    }
    forward(
        &state,
        Verb::Get,
        format!("/catalog/apps/{id}/rating"),
        None,
    )
    .await
}

pub async fn rate(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    if !valid_app_id(&id) {
        return bad_app();
    }
    let stars = body.get("stars").cloned().unwrap_or(Value::Null);
    let path = format!("/catalog/apps/{id}/rating");
    forward(&state, Verb::Put, path, Some(json!({ "stars": stars }))).await
}

pub async fn comments(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    if !valid_app_id(&id) {
        return bad_app();
    }
    forward(
        &state,
        Verb::Get,
        format!("/catalog/apps/{id}/comments"),
        None,
    )
    .await
}

#[derive(Deserialize)]
pub struct NewComment {
    pub body: String,
}

pub async fn post_comment(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(new): Json<NewComment>,
) -> Response {
    if !valid_app_id(&id) {
        return bad_app();
    }
    let path = format!("/catalog/apps/{id}/comments");
    forward(&state, Verb::Post, path, Some(json!({ "body": new.body }))).await
}

pub async fn delete_comment(State(state): State<AppState>, Path(id): Path<i32>) -> Response {
    forward(
        &state,
        Verb::Delete,
        format!("/catalog/comments/{id}"),
        None,
    )
    .await
}

pub async fn report_comment(State(state): State<AppState>, Path(id): Path<i32>) -> Response {
    let path = format!("/catalog/comments/{id}/report");
    forward(&state, Verb::Post, path, None).await
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct StoreSettings {
    pub share_installs: bool,
}

fn settings_ref() -> Value {
    crate::k8s::reference("v1", "ConfigMap", SETTINGS_NS, SETTINGS_CM)
}

pub(crate) fn share_installs_from(cm: Option<&Value>) -> bool {
    cm.and_then(|cm| cm["data"][SHARE_INSTALLS_KEY].as_str())
        .is_none_or(|v| v != "false")
}

pub(crate) async fn share_installs(client: &Client) -> bool {
    match crate::k8s::get(client, &settings_ref()).await {
        Ok(cm) => share_installs_from(cm.as_ref()),
        Err(_) => false,
    }
}

pub async fn settings(State(state): State<AppState>) -> crate::error::Result<Json<StoreSettings>> {
    let client = state.kube.client().await?;
    let cm = crate::k8s::get(&client, &settings_ref()).await?;
    Ok(Json(StoreSettings {
        share_installs: share_installs_from(cm.as_ref()),
    }))
}

pub async fn set_settings(
    State(state): State<AppState>,
    Json(body): Json<StoreSettings>,
) -> crate::error::Result<Json<StoreSettings>> {
    let client = state.kube.client().await?;
    let mut cm = settings_ref();
    cm["data"] = json!({ SHARE_INSTALLS_KEY: body.share_installs.to_string() });
    crate::k8s::apply(&client, &cm).await?;
    Ok(Json(body))
}

pub(crate) async fn report_install(
    client: &Client,
    http: &crate::http::Client,
    cfg: &Config,
    app_id: &str,
) {
    if !valid_app_id(app_id) || !share_installs(client).await {
        return;
    }
    let public = crate::charts::resolve_chart(client, app_id, Some(crate::charts::OFFICIAL))
        .await
        .is_some();
    if !public {
        return;
    }
    let Some(platform) = platform(cfg) else {
        return;
    };
    let path = format!("/catalog/apps/{app_id}/installs");
    match call(http, &platform, Verb::Post, &path, None).await {
        Ok((status, _)) if (200..300).contains(&status) => {}
        Ok((status, body)) => tracing::debug!("install of {app_id} not counted: {status} {body}"),
        Err(e) => tracing::debug!("install of {app_id} not counted: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn platform_at(server: &MockServer) -> Platform {
        Platform {
            api: server.uri(),
            token: "acct".into(),
        }
    }

    #[test]
    fn sharing_installs_is_on_until_someone_turns_it_off() {
        assert!(share_installs_from(None));
        assert!(share_installs_from(Some(&json!({ "data": {} }))));
        assert!(share_installs_from(Some(
            &json!({ "data": { "share_installs": "true" } })
        )));
        assert!(!share_installs_from(Some(
            &json!({ "data": { "share_installs": "false" } })
        )));
    }

    #[test]
    fn only_a_chart_name_may_be_sent_as_an_app_id() {
        assert!(valid_app_id("paperless-ngx"));
        for bad in ["", "../billing", "Immich", "a/b"] {
            assert!(!valid_app_id(bad), "{bad}");
        }
    }

    #[tokio::test]
    async fn a_call_carries_the_account_token_and_the_body() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/catalog/apps/immich/rating"))
            .and(header("authorization", "Bearer acct"))
            .and(body_json(json!({ "stars": 5 })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "stars": 5 })))
            .expect(1)
            .mount(&server)
            .await;
        let body = json!({ "stars": 5 });
        let (status, got) = call(
            &crate::testkit::http(),
            &platform_at(&server),
            Verb::Put,
            "/catalog/apps/immich/rating",
            Some(&body),
        )
        .await
        .unwrap();
        assert_eq!(status, 200);
        assert_eq!(got, json!({ "stars": 5 }));
    }

    #[tokio::test]
    async fn the_platform_s_refusal_reaches_the_page_with_its_reason() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(409)
                    .set_body_json(json!({ "detail": "at most 5 comments a day" })),
            )
            .mount(&server)
            .await;
        let outcome = call(
            &crate::testkit::http(),
            &platform_at(&server),
            Verb::Post,
            "/catalog/apps/immich/comments",
            None,
        )
        .await;
        let response = answer(outcome);
        assert_eq!(response.status(), StatusCode::CONFLICT);
    }

    #[test]
    fn an_unreachable_platform_is_a_bad_gateway_not_a_silent_empty_store() {
        let response = answer(Err(anyhow::anyhow!("connection refused")));
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    }
}
