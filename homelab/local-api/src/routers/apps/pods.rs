use super::*;

#[derive(Serialize)]
pub struct PodInfo {
    pub name: String,
    pub member: Option<String>,
    pub phase: String,
    pub ready: bool,
    pub node: String,
    pub restarts: u64,
    pub containers: Vec<ContainerInfo>,
    pub events: Vec<PodEvent>,
}

#[derive(Serialize, Debug, PartialEq)]
pub struct PodEvent {
    pub warning: bool,
    pub reason: String,
    pub message: String,
    pub at: String,
    pub count: u64,
}

pub(super) const POD_EVENTS_SHOWN: usize = 8;

pub(super) fn pod_events(events: &[Value], pod: &str) -> Vec<PodEvent> {
    let mut found: Vec<PodEvent> = events
        .iter()
        .filter(|e| e["involvedObject"]["kind"] == "Pod" && e["involvedObject"]["name"] == pod)
        .map(|e| PodEvent {
            warning: e["type"] == "Warning",
            reason: e["reason"].as_str().unwrap_or("").to_string(),
            message: e["message"].as_str().unwrap_or("").trim().to_string(),
            at: ["lastTimestamp", "eventTime", "firstTimestamp"]
                .iter()
                .find_map(|k| e[*k].as_str())
                .or_else(|| e["metadata"]["creationTimestamp"].as_str())
                .unwrap_or("")
                .to_string(),
            count: e["count"].as_u64().unwrap_or(1),
        })
        .collect();
    found.sort_by(|a, b| a.at.cmp(&b.at));
    let skip = found.len().saturating_sub(POD_EVENTS_SHOWN);
    found.split_off(skip)
}

#[derive(Serialize, Debug, PartialEq)]
pub struct ContainerInfo {
    pub name: String,
    pub init: bool,
    pub ready: bool,
    pub state: String,
    pub restarts: u64,
}

pub(super) fn container_state(status: &Value) -> String {
    let state = &status["state"];
    if state["running"].is_object() {
        return "running".into();
    }
    if let Some(w) = state["waiting"].as_object() {
        return w
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("waiting")
            .to_string();
    }
    if let Some(t) = state["terminated"].as_object() {
        let reason = t.get("reason").and_then(Value::as_str).unwrap_or("stopped");
        return match t.get("exitCode").and_then(Value::as_i64) {
            Some(0) | None => reason.to_string(),
            Some(code) => format!("{reason} (exit {code})"),
        };
    }
    "not started".into()
}

pub(super) fn pod_info(p: &Value) -> PodInfo {
    let containers: Vec<ContainerInfo> = [
        ("initContainerStatuses", true),
        ("containerStatuses", false),
    ]
    .into_iter()
    .flat_map(|(key, init)| {
        p["status"][key]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(move |c| ContainerInfo {
                name: c["name"].as_str().unwrap_or("").to_string(),
                init,
                ready: c["ready"].as_bool().unwrap_or(false),
                state: container_state(&c),
                restarts: c["restartCount"].as_u64().unwrap_or(0),
            })
    })
    .collect();
    PodInfo {
        name: p["metadata"]["name"].as_str().unwrap_or("").to_string(),
        member: p["metadata"]["labels"][LABEL_MEMBER]
            .as_str()
            .filter(|m| !m.is_empty())
            .map(str::to_string),
        phase: p["status"]["phase"]
            .as_str()
            .unwrap_or("Unknown")
            .to_string(),
        ready: p["status"]["conditions"]
            .as_array()
            .map(|cs| {
                cs.iter()
                    .any(|c| c["type"] == "Ready" && c["status"] == "True")
            })
            .unwrap_or(false),
        node: p["spec"]["nodeName"].as_str().unwrap_or("").to_string(),
        restarts: containers.iter().map(|c| c.restarts).sum(),
        containers,
        events: Vec::new(),
    }
}

pub async fn list_pods(
    State(state): State<AppState>,
    Path(instance_name): Path<String>,
) -> Result<Json<Vec<PodInfo>>> {
    let client = state.kube.client().await?;
    let ns = format!("yolab-{instance_name}");
    let pods = crate::k8s::list(&client, "v1", "Pod", Some(&ns), &Default::default()).await?;
    let events = crate::k8s::list(&client, "v1", "Event", Some(&ns), &Default::default())
        .await
        .unwrap_or_else(|e| {
            tracing::warn!("{ns}: events unreadable, pods shown without them ({e})");
            Vec::new()
        });
    Ok(Json(
        pods.iter()
            .filter(|p| !is_backup_mover_pod(p))
            .map(|p| {
                let mut info = pod_info(p);
                info.events = pod_events(&events, &info.name);
                info
            })
            .collect(),
    ))
}

pub async fn pod_logs(
    State(state): State<AppState>,
    Path((instance_name, pod_name)): Path<(String, String)>,
) -> Sse<impl futures::Stream<Item = std::result::Result<Event, Infallible>>> {
    let ns = format!("yolab-{instance_name}");
    let kube = state.kube.clone();
    let stream = async_stream::stream! {
        use futures::StreamExt as _;
        let followed = match kube.client().await {
            Ok(client) => follow_pod_logs(&client, &ns, &pod_name).await,
            Err(e) => Err(e),
        };
        let mut lines = match followed {
            Ok(lines) => lines,
            Err(e) => {
                yield Ok(Event::default().data(format!("[yolab] could not read the logs: {e:#}")));
                return;
            }
        };
        while let Some(line) = lines.next().await {
            yield Ok(Event::default().data(line));
        }
    };
    Sse::new(stream)
}

pub(super) async fn follow_pod_logs(
    client: &Client,
    ns: &str,
    pod: &str,
) -> anyhow::Result<futures::stream::BoxStream<'static, String>> {
    use futures::{AsyncBufReadExt as _, StreamExt as _};
    use k8s_openapi::api::core::v1::Pod;
    let pods = kube::Api::<Pod>::namespaced(client.clone(), ns);
    let spec = pods.get(pod).await?.spec.unwrap_or_default();
    let containers = spec
        .init_containers
        .unwrap_or_default()
        .into_iter()
        .chain(spec.containers)
        .map(|c| c.name);
    let mut streams = Vec::new();
    for container in containers {
        let prefix = format!("[pod/{pod}/{container}]");
        let params = kube::api::LogParams {
            container: Some(container),
            follow: true,
            tail_lines: Some(i64::from(LOGS_FOLLOW_TAIL)),
            ..Default::default()
        };
        match pods.log_stream(pod, &params).await {
            Ok(reader) => streams.push(
                reader
                    .lines()
                    .filter_map(move |line| {
                        let prefixed = line.ok().map(|l| format!("{prefix} {l}"));
                        async move { prefixed }
                    })
                    .boxed(),
            ),
            Err(e) => {
                let said = format!("[yolab] {prefix} {e}");
                streams.push(futures::stream::once(async move { said }).boxed());
            }
        }
    }
    Ok(futures::stream::select_all(streams).boxed())
}
