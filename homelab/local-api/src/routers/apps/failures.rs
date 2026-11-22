use super::*;

pub(crate) fn is_terminating_pod(pod: &Value) -> bool {
    !pod["metadata"]["deletionTimestamp"].is_null()
}

pub(crate) fn is_finished_pod(pod: &Value) -> bool {
    matches!(
        pod["status"]["phase"].as_str(),
        Some("Succeeded") | Some("Failed")
    )
}

pub(crate) fn explain_app_state(pods: &[&Value]) -> String {
    if pods.is_empty() {
        return "Installing…".into();
    }

    let mut waiting: Vec<String> = Vec::new();
    let mut unschedulable = false;
    let mut storage_pending = false;
    let mut running_not_ready = false;

    for pod in pods {
        if pod["status"]["phase"].as_str() == Some("Pending") {
            let conditions = pod["status"]["conditions"].as_array();
            unschedulable |= conditions
                .map(|cs| {
                    cs.iter()
                        .any(|c| c["type"] == "PodScheduled" && c["reason"] == "Unschedulable")
                })
                .unwrap_or(false);
            storage_pending |= conditions
                .map(|cs| {
                    cs.iter().any(|c| {
                        c["message"]
                            .as_str()
                            .map(|m| m.contains("PersistentVolumeClaim"))
                            .unwrap_or(false)
                    })
                })
                .unwrap_or(false);
        }

        for key in ["initContainerStatuses", "containerStatuses"] {
            for cs in pod["status"][key].as_array().into_iter().flatten() {
                if let Some(reason) = cs["state"]["waiting"]["reason"].as_str() {
                    waiting.push(reason.to_string());
                }
                if cs["state"]["running"].is_object() && cs["ready"] == false {
                    running_not_ready = true;
                }
            }
        }
    }

    let has = |r: &str| waiting.iter().any(|reason| reason == r);

    if unschedulable {
        return if storage_pending {
            "Getting this app's storage ready — copying its files can take a while.".into()
        } else {
            "No machine has room for this app right now.".into()
        };
    }

    if has("ContainerCreating") {
        return "Getting ready — downloading files and connecting storage.".into();
    }
    if has("PodInitializing") {
        return "Running first-time setup.".into();
    }
    if running_not_ready {
        return "Almost ready — waiting for the app to respond.".into();
    }
    if !waiting.is_empty() {
        return "Getting ready…".into();
    }

    String::new()
}

pub(super) const EXPLAINED_WAITS: [&str; 2] = ["ContainerCreating", "PodInitializing"];

pub(crate) fn unexplained_waits(pods: &[&Value]) -> String {
    let mut reasons: Vec<&str> = pods
        .iter()
        .flat_map(|pod| {
            ["initContainerStatuses", "containerStatuses"]
                .into_iter()
                .flat_map(move |key| pod["status"][key].as_array().into_iter().flatten())
        })
        .filter_map(|cs| cs["state"]["waiting"]["reason"].as_str())
        .filter(|reason| !EXPLAINED_WAITS.contains(reason))
        .collect();
    reasons.dedup();
    reasons.join(", ")
}

pub(crate) fn waiting_since(pods: &[&Value]) -> Option<String> {
    pods.iter()
        .filter_map(|p| p["metadata"]["creationTimestamp"].as_str())
        .min()
        .map(str::to_string)
}

pub(crate) fn install_failure_headline(reason: &str) -> String {
    let said = reason.to_lowercase();
    if said.contains("timed out") || said.contains("deadline exceeded") {
        "It took too long to start, so the installation was stopped.".into()
    } else if said.contains("no space") || said.contains("insufficient") {
        "There is not enough room on the server for it.".into()
    } else {
        "Something went wrong while setting it up.".into()
    }
}

pub(super) fn go_duration(text: &str) -> Option<chrono::Duration> {
    let mut rest = text.trim();
    if rest.is_empty() {
        return None;
    }
    let mut total = 0f64;
    while !rest.is_empty() {
        let digits = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(rest.len());
        let number: f64 = rest[..digits].parse().ok()?;
        rest = &rest[digits..];
        let unit = rest
            .find(|c: char| c.is_ascii_digit() || c == '.')
            .unwrap_or(rest.len());
        let seconds = match &rest[..unit] {
            "h" => 3600.0,
            "m" => 60.0,
            "s" => 1.0,
            "ms" => 0.001,
            _ => return None,
        };
        total += number * seconds;
        rest = &rest[unit..];
    }
    Some(chrono::Duration::milliseconds(
        (total * 1000.0).round() as i64
    ))
}

pub(super) fn next_restart(waiting: &Value, last: &Value) -> Option<String> {
    let delay = waiting["message"]
        .as_str()?
        .strip_prefix("back-off ")?
        .split_whitespace()
        .next()
        .and_then(go_duration)?;
    let ended = chrono::DateTime::parse_from_rfc3339(last["finishedAt"].as_str()?).ok()?;
    Some(
        (ended.with_timezone(&chrono::Utc) + delay)
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    )
}

pub(super) const CANNOT_START: [&str; 7] = [
    "ImagePullBackOff",
    "ErrImagePull",
    "ErrImageNeverPull",
    "CreateContainerConfigError",
    "CreateContainerError",
    "RunContainerError",
    "InvalidImageName",
];

pub(super) const FAILURE_LOG_LINES: i64 = 5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ContainerFailure {
    pub pod: String,
    pub container: String,
    pub init: bool,
    pub said: String,
    pub exit_code: Option<i64>,
    pub previous: bool,
    pub reason: String,
    pub retry_at: Option<String>,
}

impl ContainerFailure {
    pub(crate) fn headline(&self) -> String {
        let said = self.said.to_lowercase();
        let mentions = |words: &[&str]| words.iter().any(|w| said.contains(w));
        match self.reason.as_str() {
            "ImagePullBackOff" | "ErrImagePull" | "ErrImageNeverPull" => {
                if mentions(&["not found", "manifest unknown", "does not exist"]) {
                    "The version of its program it asks for could not be found online.".into()
                } else if mentions(&["unauthorized", "denied", "forbidden"]) {
                    "Its program could not be downloaded: the place it comes from refused access."
                        .into()
                } else {
                    "Its program could not be downloaded. Check that the server is connected to the internet.".into()
                }
            }
            "InvalidImageName" => "It points to a program that does not exist.".into(),
            "CreateContainerConfigError" => {
                "A setting or password it needs to start is missing.".into()
            }
            "CreateContainerError" | "RunContainerError" => {
                "Its program could not be started.".into()
            }
            "OOMKilled" => "It ran out of memory and was stopped.".into(),
            _ => "It starts, then stops right away.".into(),
        }
    }

    pub(crate) fn describe(&self, said: &str) -> String {
        match (said.trim(), self.exit_code) {
            ("", Some(code)) => format!(
                "{} stopped with exit code {code} and left no message",
                self.container
            ),
            ("", None) => format!("{} stopped and left no message", self.container),
            (said, _) => format!("{}: {said}", self.container),
        }
    }
}

pub(super) fn failure_of(pod: &str, init: bool, cs: &Value) -> Option<ContainerFailure> {
    let text = |v: &Value| v["message"].as_str().unwrap_or("").trim().to_string();
    let failure = |said: String, ended: &Value, previous: bool, reason: &str| ContainerFailure {
        pod: pod.to_string(),
        container: cs["name"].as_str().unwrap_or("").to_string(),
        init,
        said,
        exit_code: ended["exitCode"].as_i64(),
        previous,
        reason: reason.to_string(),
        retry_at: None,
    };
    let waiting = &cs["state"]["waiting"];
    let reason = waiting["reason"].as_str().unwrap_or("");
    if reason == "CrashLoopBackOff" {
        let last = &cs["lastState"]["terminated"];
        let why = match last["reason"].as_str() {
            Some("OOMKilled") => "OOMKilled",
            _ => reason,
        };
        return Some(ContainerFailure {
            retry_at: next_restart(waiting, last),
            ..failure(text(last), last, true, why)
        });
    }
    if CANNOT_START.contains(&reason) {
        let said = text(waiting);
        let said = if said.is_empty() {
            reason.to_string()
        } else {
            said
        };
        return Some(failure(said, &Value::Null, false, reason));
    }
    let ended = &cs["state"]["terminated"];
    if ended["exitCode"].as_i64().is_some_and(|code| code != 0) {
        let why = ended["reason"].as_str().unwrap_or("");
        return Some(failure(text(ended), ended, false, why));
    }
    None
}

pub(crate) fn container_failure(pods: &[&Value]) -> Option<ContainerFailure> {
    pods.iter().find_map(|pod| {
        let name = pod["metadata"]["name"].as_str().unwrap_or("");
        [
            ("initContainerStatuses", true),
            ("containerStatuses", false),
        ]
        .iter()
        .flat_map(|&(key, init)| {
            pod["status"][key]
                .as_array()
                .into_iter()
                .flatten()
                .map(move |cs| (init, cs))
        })
        .find_map(|(init, cs)| failure_of(name, init, cs))
    })
}

pub(super) fn last_lines(log: &str) -> String {
    log.lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

pub(super) async fn explain_failure(
    client: &Client,
    ns: &str,
    failure: &ContainerFailure,
) -> String {
    if !failure.said.is_empty() {
        return failure.describe(&failure.said);
    }
    let pods = kube::Api::<k8s_openapi::api::core::v1::Pod>::namespaced(client.clone(), ns);
    let params = kube::api::LogParams {
        container: Some(failure.container.clone()),
        previous: failure.previous,
        tail_lines: Some(FAILURE_LOG_LINES),
        ..Default::default()
    };
    let said = match pods.logs(&failure.pod, &params).await {
        Ok(log) => last_lines(&log),
        Err(e) => {
            tracing::debug!(
                "{ns}: could not read why {} stopped: {e}",
                failure.container
            );
            String::new()
        }
    };
    failure.describe(&said)
}

pub(super) fn is_cloning(pvc: &Value) -> bool {
    pvc["status"]["phase"].as_str() == Some("Pending")
        && ["dataSource", "dataSourceRef"]
            .iter()
            .any(|key| pvc["spec"][key]["kind"].as_str() == Some("VolumeSnapshot"))
}

pub(super) fn clone_percent(events: &[Value], namespace: &str, pvc: &str) -> Option<u32> {
    let mut latest: Option<(&str, &str)> = None;
    for event in events {
        if event["involvedObject"]["kind"].as_str() != Some("PersistentVolumeClaim")
            || event["involvedObject"]["namespace"].as_str() != Some(namespace)
            || event["involvedObject"]["name"].as_str() != Some(pvc)
        {
            continue;
        }
        let Some(message) = event["message"].as_str() else {
            continue;
        };
        if !message.contains("percentage cloned=") {
            continue;
        }
        let at = event["lastTimestamp"]
            .as_str()
            .or_else(|| event["eventTime"].as_str())
            .unwrap_or("");
        if latest.map(|(seen, _)| at >= seen).unwrap_or(true) {
            latest = Some((at, message));
        }
    }
    let (_, message) = latest?;
    let rest = message.split("percentage cloned=").nth(1)?;
    let number: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    number
        .parse::<f64>()
        .ok()
        .map(|percent| percent.round() as u32)
}

pub(super) fn copying_data(
    namespace: &str,
    pvcs_by_ns: &std::collections::HashMap<&str, Vec<&Value>>,
    events: &[Value],
) -> Option<(String, Option<String>)> {
    let pvc = pvcs_by_ns
        .get(namespace)?
        .iter()
        .find(|pvc| is_cloning(pvc))?;
    let name = pvc["metadata"]["name"].as_str().unwrap_or("");
    let said = match clone_percent(events, namespace, name) {
        Some(percent) => format!("Copying this app's files… {percent}%"),
        None => "Copying this app's files…".to_string(),
    };
    let started = pvc["metadata"]["creationTimestamp"]
        .as_str()
        .map(str::to_string);
    Some((said, started))
}

pub(crate) fn install_failure(ann: &serde_json::Map<String, Value>) -> Option<String> {
    ann.get(ANN_INSTALL_FAILED)
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

pub(super) async fn set_install_failed(
    client: &Client,
    ns: &str,
    value: Value,
) -> anyhow::Result<()> {
    let mut patch = namespace_ref(ns);
    patch["metadata"]["annotations"] = serde_json::json!({ ANN_INSTALL_FAILED: value });
    match crate::k8s::merge_patch(client, &patch).await {
        Err(e) if crate::k8s::refused_with(&e, 404) => Ok(()),
        other => other,
    }
}

pub(crate) async fn mark_install_failed(client: &Client, ns: &str, reason: &str) {
    tracing::warn!("{ns}: install failed, kept for inspection: {reason}");
    set_install_failed(client, ns, Value::String(reason.to_string()))
        .await
        .warn_on_err(format!("{ns}: could not mark the install as failed"));
}

pub(crate) async fn clear_install_failed(client: &Client, ns: &str) {
    set_install_failed(client, ns, Value::Null)
        .await
        .warn_on_err(format!("{ns}: could not clear the failed-install mark"));
}
