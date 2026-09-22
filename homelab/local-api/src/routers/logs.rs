use std::collections::BTreeSet;

use axum::{
    extract::{Query, State},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::process::Command;

use crate::AppState;

const TIMEOUT_SECS: u64 = 20;

const MAX_LINES: usize = 2000;
const DEFAULT_LINES: usize = 300;

#[derive(Deserialize)]
pub struct LogQuery {
    pub unit: Option<String>,
    pub priority: Option<u8>,
    pub since: Option<String>,
    pub search: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Serialize)]
pub struct LogEntry {
    pub timestamp: String,
    pub unit: String,
    pub priority: u8,
    pub message: String,
}

#[derive(Serialize)]
pub struct LogsResponse {
    pub entries: Vec<LogEntry>,
    pub units: Vec<String>,
    pub truncated: bool,
}

fn rfc3339_from_realtime(usec: &str) -> String {
    let Ok(usec) = usec.parse::<i64>() else {
        return String::new();
    };
    chrono::DateTime::from_timestamp_micros(usec)
        .map(|t| t.to_rfc3339())
        .unwrap_or_default()
}

fn unit_of(entry: &Value) -> String {
    for key in ["_SYSTEMD_UNIT", "UNIT", "SYSLOG_IDENTIFIER", "_COMM"] {
        if let Some(v) = entry[key].as_str().filter(|s| !s.is_empty()) {
            return v.to_string();
        }
    }
    "kernel".to_string()
}

fn message_of(entry: &Value) -> String {
    match &entry["MESSAGE"] {
        Value::String(s) => s.clone(),
        Value::Array(bytes) => {
            let raw: Vec<u8> = bytes
                .iter()
                .filter_map(|b| b.as_u64().map(|n| n as u8))
                .collect();
            String::from_utf8_lossy(&raw).to_string()
        }
        _ => String::new(),
    }
}

pub(crate) fn parse_journal(raw: &str, search: Option<&str>) -> (Vec<LogEntry>, BTreeSet<String>) {
    let needle = search
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase);

    let mut entries = Vec::new();
    let mut units = BTreeSet::new();

    for line in raw.lines().filter(|l| !l.trim().is_empty()) {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let unit = unit_of(&v);
        units.insert(unit.clone());

        let message = message_of(&v);
        if let Some(n) = &needle {
            if !message.to_lowercase().contains(n.as_str()) {
                continue;
            }
        }

        entries.push(LogEntry {
            timestamp: rfc3339_from_realtime(v["__REALTIME_TIMESTAMP"].as_str().unwrap_or("")),
            unit,
            priority: v["PRIORITY"]
                .as_str()
                .and_then(|p| p.parse().ok())
                .unwrap_or(6),
            message,
        });
    }

    (entries, units)
}

pub async fn list_logs(
    State(_s): State<AppState>,
    Query(q): Query<LogQuery>,
) -> Json<LogsResponse> {
    let limit = q.limit.unwrap_or(DEFAULT_LINES).clamp(1, MAX_LINES);

    let mut args: Vec<String> = vec![
        "--no-pager".into(),
        "-o".into(),
        "json".into(),
        "-r".into(),
        "-n".into(),
        limit.to_string(),
    ];

    match q.since.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(since) => args.extend(["--since".into(), since.to_string()]),
        None => args.push("-b".into()),
    }
    if let Some(unit) = q.unit.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        args.extend(["-u".into(), unit.to_string()]);
    }
    if let Some(p) = q.priority {
        args.extend(["-p".into(), p.min(7).to_string()]);
    }

    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(TIMEOUT_SECS),
        Command::new("journalctl")
            .args(&argv)
            .kill_on_drop(true)
            .output(),
    )
    .await;

    let raw = match out {
        Ok(Ok(o)) => String::from_utf8_lossy(&o.stdout).to_string(),
        Ok(Err(e)) => {
            tracing::warn!("logs: could not run journalctl: {e}");
            String::new()
        }
        Err(_) => {
            tracing::warn!("logs: journalctl timed out after {TIMEOUT_SECS}s");
            String::new()
        }
    };

    let (mut entries, units) = parse_journal(&raw, q.search.as_deref());
    entries.reverse();

    Json(LogsResponse {
        truncated: entries.len() >= limit,
        entries,
        units: units.into_iter().collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
{"__REALTIME_TIMESTAMP":"1788785220245403","PRIORITY":"6","_SYSTEMD_UNIT":"yolab-containerd-store.service","MESSAGE":"images RBD mapped at /dev/rbd0"}
{"__REALTIME_TIMESTAMP":"1788785232767836","PRIORITY":"4","_SYSTEMD_UNIT":"yolab-containerd-store.service","MESSAGE":"the image store on /dev/rbd0 will not mount and read — rebuilding it"}
{"__REALTIME_TIMESTAMP":"1788785240000000","PRIORITY":"3","SYSLOG_IDENTIFIER":"k3s","MESSAGE":"Failed to test etcd connection"}
"#;

    #[test]
    fn parses_entries_and_collects_units() {
        let (entries, units) = parse_journal(SAMPLE, None);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].unit, "yolab-containerd-store.service");
        assert_eq!(entries[0].priority, 6);
        assert!(entries[0].timestamp.starts_with("2026-"));
        assert_eq!(entries[2].unit, "k3s");
        assert!(units.contains("yolab-containerd-store.service"));
        assert!(units.contains("k3s"));
    }

    #[test]
    fn search_is_a_plain_case_insensitive_substring() {
        let (entries, _) = parse_journal(SAMPLE, Some("RBD MAPPED"));
        assert_eq!(entries.len(), 1);
        assert!(entries[0].message.contains("images RBD mapped"));

        let (none, _) = parse_journal(SAMPLE, Some("nothing matches this"));
        assert!(none.is_empty());

        let (literal, _) = parse_journal(SAMPLE, Some("("));
        assert!(literal.is_empty());
    }

    #[test]
    fn a_blank_search_is_not_a_filter() {
        assert_eq!(parse_journal(SAMPLE, Some("   ")).0.len(), 3);
        assert_eq!(parse_journal(SAMPLE, Some("")).0.len(), 3);
    }

    #[test]
    fn a_broken_line_does_not_discard_the_others() {
        let mixed = format!("{SAMPLE}\nnot json at all\n");
        assert_eq!(parse_journal(&mixed, None).0.len(), 3);
    }

    #[test]
    fn a_non_utf8_message_is_decoded_not_dumped() {
        let raw =
            r#"{"__REALTIME_TIMESTAMP":"1788785220245403","PRIORITY":"6","MESSAGE":[104,105,255]}"#;
        let (entries, _) = parse_journal(raw, None);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].message.starts_with("hi"));
        assert!(!entries[0].message.contains("104"));
    }

    #[test]
    fn an_unattributed_line_is_still_shown() {
        let raw = r#"{"__REALTIME_TIMESTAMP":"1788785220245403","PRIORITY":"4","MESSAGE":"rbd0: writeback error"}"#;
        let (entries, units) = parse_journal(raw, None);
        assert_eq!(entries[0].unit, "kernel");
        assert!(units.contains("kernel"));
    }
}
