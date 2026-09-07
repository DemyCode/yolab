//! One place to read what the machine has been saying.
//!
//! Everything this platform does that is hard to see happens in a systemd unit
//! before k3s exists — mapping the image RBD, bootstrapping Ceph, minting keys,
//! reconciling disks. When one of those misbehaves the only record is the
//! journal, and reaching it meant SSH, which is exactly the moment someone is
//! least able to.
//!
//! Two failures on 2026-09-07 took hours longer than they needed to for want of
//! this: a storage unit that could not log at all (its subscriber was installed
//! after the subcommand dispatch, so every line went nowhere), and a timer that
//! had silently stopped firing days earlier. Both were plainly visible in
//! `journalctl` the entire time. Nothing was looking, and nothing could look
//! without a shell on the box.
//!
//! Reads the journal rather than tailing files: it is already structured, it
//! already spans every unit, and it survives the service that wrote it dying.

use std::collections::BTreeSet;

use axum::{
    extract::{Query, State},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::process::Command;

use crate::AppState;

/// Bounded like every other subprocess here: a wedged journalctl must degrade
/// the page, never the task pool.
const TIMEOUT_SECS: u64 = 20;

/// Hard ceiling on what one request may return, whatever `limit` asks for.
/// The journal on a busy node is hundreds of thousands of lines; the browser is
/// the wrong place to discover that.
const MAX_LINES: usize = 2000;
const DEFAULT_LINES: usize = 300;

#[derive(Deserialize)]
pub struct LogQuery {
    /// systemd unit to restrict to. Empty or absent means every unit.
    pub unit: Option<String>,
    /// Lowest severity to include, as journald's numeric priority (0 emerg ..
    /// 7 debug). Absent means everything.
    pub priority: Option<u8>,
    /// Anything journalctl's `--since` accepts: "1 hour ago", "today", a
    /// timestamp. Absent means the current boot.
    pub since: Option<String>,
    /// Case-insensitive substring filter, applied to the message text.
    ///
    /// Deliberately NOT journalctl's `--grep`: that is a PCRE over the whole
    /// entry and a stray `(` from someone typing an app name turns into an
    /// error rather than no results. Filtering here keeps a search box a search
    /// box.
    pub search: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Serialize)]
pub struct LogEntry {
    /// RFC3339, so the browser can format it in the viewer's timezone rather
    /// than the machine's.
    pub timestamp: String,
    pub unit: String,
    /// journald's numeric priority, kept as a number so the UI can colour by
    /// severity without re-deriving it from text.
    pub priority: u8,
    pub message: String,
}

#[derive(Serialize)]
pub struct LogsResponse {
    pub entries: Vec<LogEntry>,
    /// Every unit that appears in the journal for this boot, so the UI can offer
    /// a real list to filter by instead of asking someone to type a unit name.
    pub units: Vec<String>,
    /// True when the result hit `MAX_LINES` and older entries were dropped.
    /// Shown to the reader, because silently truncated logs are worse than none.
    pub truncated: bool,
}

/// journald stores the timestamp as microseconds since the epoch in a string.
/// Rendered here rather than in the browser so a malformed entry degrades to an
/// empty cell instead of an exception in the page.
fn rfc3339_from_realtime(usec: &str) -> String {
    let Ok(usec) = usec.parse::<i64>() else {
        return String::new();
    };
    chrono::DateTime::from_timestamp_micros(usec)
        .map(|t| t.to_rfc3339())
        .unwrap_or_default()
}

/// The unit a line belongs to, preferring the real cgroup attribution and
/// falling back to whatever identifier the writer chose.
///
/// Both are needed. The storage subcommands run as `local-api storage <cmd>`
/// inside their unit's cgroup, so `_SYSTEMD_UNIT` names the unit correctly; but
/// kernel messages and anything logged before a cgroup exists have only
/// `SYSLOG_IDENTIFIER`. Dropping either loses a class of line that matters.
fn unit_of(entry: &Value) -> String {
    for key in ["_SYSTEMD_UNIT", "UNIT", "SYSLOG_IDENTIFIER", "_COMM"] {
        if let Some(v) = entry[key].as_str().filter(|s| !s.is_empty()) {
            return v.to_string();
        }
    }
    "kernel".to_string()
}

/// journald's MESSAGE is usually a string, but is an array of bytes when the
/// line was not valid UTF-8. Rendering the byte array as JSON in the UI would be
/// unreadable, so it is decoded lossily here.
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

/// Parses journalctl's `-o json` output — one JSON object per line — into the
/// shape the page renders, applying the text filter as it goes.
///
/// Pure, and separate from the subprocess, so the parsing is testable against
/// real journald output without a journal.
pub(crate) fn parse_journal(raw: &str, search: Option<&str>) -> (Vec<LogEntry>, BTreeSet<String>) {
    let needle = search
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase);

    let mut entries = Vec::new();
    let mut units = BTreeSet::new();

    for line in raw.lines().filter(|l| !l.trim().is_empty()) {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            // A single unparsable line must not lose the rest of the journal.
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
            // Absent priority is treated as informational rather than dropped:
            // the line still happened.
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
        // Newest first, so a `limit` keeps the RECENT lines. `-n` alone would
        // also do that, but being explicit survives someone adding --since.
        "-r".into(),
        "-n".into(),
        limit.to_string(),
    ];

    // Default to this boot. Unbounded, the first query on a machine with
    // persistent storage can walk weeks of journal before returning anything.
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
        // An empty page with no explanation is the failure mode this whole
        // router exists to end, so say what went wrong in the log itself.
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
    // journalctl was asked for newest-first so that `-n` keeps recent lines;
    // the page reads oldest-first, the way a log does.
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

    /// Real journalctl `-o json` output, trimmed to the fields this reads.
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

    /// The search box is a substring match, not a regex — someone typing an app
    /// name with a bracket in it must get results, not an error.
    #[test]
    fn search_is_a_plain_case_insensitive_substring() {
        let (entries, _) = parse_journal(SAMPLE, Some("RBD MAPPED"));
        assert_eq!(entries.len(), 1);
        assert!(entries[0].message.contains("images RBD mapped"));

        let (none, _) = parse_journal(SAMPLE, Some("nothing matches this"));
        assert!(none.is_empty());

        // Regex metacharacters are literal here. Under --grep this would be an
        // error or a wildcard match; it must simply find nothing.
        let (literal, _) = parse_journal(SAMPLE, Some("("));
        assert!(literal.is_empty());
    }

    /// A blank search must not filter everything out — an empty box means "no
    /// filter", not "match the empty string against nothing".
    #[test]
    fn a_blank_search_is_not_a_filter() {
        assert_eq!(parse_journal(SAMPLE, Some("   ")).0.len(), 3);
        assert_eq!(parse_journal(SAMPLE, Some("")).0.len(), 3);
    }

    /// One malformed line must not cost the reader the rest of the journal.
    #[test]
    fn a_broken_line_does_not_discard_the_others() {
        let mixed = format!("{SAMPLE}\nnot json at all\n");
        assert_eq!(parse_journal(&mixed, None).0.len(), 3);
    }

    /// journald emits MESSAGE as a byte array when the line was not UTF-8.
    /// Rendering that array verbatim would put `[104,105]` on the page.
    #[test]
    fn a_non_utf8_message_is_decoded_not_dumped() {
        let raw =
            r#"{"__REALTIME_TIMESTAMP":"1788785220245403","PRIORITY":"6","MESSAGE":[104,105,255]}"#;
        let (entries, _) = parse_journal(raw, None);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].message.starts_with("hi"));
        assert!(!entries[0].message.contains("104"));
    }

    /// A line with no unit attribution still has to appear. Kernel messages
    /// carry neither _SYSTEMD_UNIT nor SYSLOG_IDENTIFIER.
    #[test]
    fn an_unattributed_line_is_still_shown() {
        let raw = r#"{"__REALTIME_TIMESTAMP":"1788785220245403","PRIORITY":"4","MESSAGE":"rbd0: writeback error"}"#;
        let (entries, units) = parse_journal(raw, None);
        assert_eq!(entries[0].unit, "kernel");
        assert!(units.contains("kernel"));
    }
}
