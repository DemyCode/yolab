//! When backups run, as a cron expression the owner can see and change.
//!
//! ── Why cron is evaluated as "has this occurrence passed", not "is it 02:00 now" ──
//!
//! The scheduler this replaces was deliberately not a timer. Its comment:
//!
//!   Scheduling is wall-clock derived ("no run succeeded in the last 24h and none is
//!   active → start one") instead of a single `tokio::time::sleep` until 02:00 UTC —
//!   the latter uses CLOCK_MONOTONIC, which does not advance across laptop suspend,
//!   so a suspended machine could miss its nightly backup indefinitely.
//!
//! That property is not optional here: this runs on laptops, and a laptop is usually
//! asleep at 02:00. A cron implementation that fires only when the clock reads exactly
//! the scheduled minute would reintroduce the identical bug in a new shape — the
//! machine wakes at 09:00, 02:00 never "happened", and backups silently stop.
//!
//! So a schedule is never asked "is it time now". It is asked "when should this last
//! have run", and the answer is compared against the last backup that actually
//! succeeded. A machine asleep from 01:00 to 09:00 wakes up, finds that today's 02:00
//! occurrence has passed with no successful run since, and backs up immediately. A
//! machine that was awake does exactly the same thing at 02:00. Catch-up is not a
//! special case bolted on; it is the only case.
//!
//! ── Local time, not UTC ──
//!
//! "02:00" means the owner's two in the morning. Evaluating in UTC would put a French
//! cluster's "quiet hour" backup at 04:00 in summer and 03:00 in winter, drifting by an
//! hour twice a year for no reason the owner could see. Every description this module
//! produces names the zone it means.

use chrono::{DateTime, Datelike, Local, TimeZone, Timelike};

/// Runs at 02:00 local time daily. Matches what the fixed 24-hour scheduler did, so a
/// cluster that never opens this screen keeps its existing behaviour.
pub const DEFAULT_EXPR: &str = "0 2 * * *";

/// How far back `previous_occurrence` will look before giving up.
///
/// A schedule like "0 4 29 2 *" (29 February) legitimately has no occurrence for years,
/// and scanning to the beginning of time to discover that is not useful. Beyond this
/// horizon a schedule is treated as having no due occurrence, which errs toward not
/// backing up rather than toward backing up constantly — the failure a user can see and
/// correct, rather than one that quietly hammers their storage bill.
const LOOKBACK_DAYS: i64 = 400;

#[derive(Debug, Clone, PartialEq)]
pub struct Schedule {
    minute: Vec<u32>,
    hour: Vec<u32>,
    day_of_month: Vec<u32>,
    month: Vec<u32>,
    day_of_week: Vec<u32>,
    /// Kept so a round trip through this type never rewrites what the owner typed.
    expr: String,
    dom_restricted: bool,
    dow_restricted: bool,
}

const DOW_NAMES: [&str; 7] = [
    "Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday",
];
const MONTH_NAMES: [&str; 12] = [
    "January", "February", "March", "April", "May", "June",
    "July", "August", "September", "October", "November", "December",
];

fn dow_from_name(s: &str) -> Option<u32> {
    let s = s.to_ascii_lowercase();
    DOW_NAMES.iter().position(|n| n.to_ascii_lowercase().starts_with(&s) && s.len() >= 3)
        .map(|i| i as u32)
}

fn month_from_name(s: &str) -> Option<u32> {
    let s = s.to_ascii_lowercase();
    MONTH_NAMES.iter().position(|n| n.to_ascii_lowercase().starts_with(&s) && s.len() >= 3)
        .map(|i| i as u32 + 1)
}

/// Expands one cron field into the sorted, deduplicated set of values it allows.
///
/// Errors are written for someone who typed the expression by hand, not for a log: they
/// name the field and the legal range, because "invalid cron" on a five-field string
/// tells you nothing about which field you got wrong.
fn parse_field(
    raw: &str,
    label: &str,
    min: u32,
    max: u32,
    name_lookup: Option<fn(&str) -> Option<u32>>,
) -> Result<Vec<u32>, String> {
    let mut out: Vec<u32> = Vec::new();
    if raw.trim().is_empty() {
        return Err(format!("the {label} field is empty"));
    }
    for part in raw.split(',') {
        let part = part.trim();
        let (range_part, step) = match part.split_once('/') {
            Some((r, s)) => {
                let step: u32 = s.parse().map_err(|_| {
                    format!("\"{s}\" is not a whole number, so \"{part}\" is not a step the {label} field can use")
                })?;
                if step == 0 {
                    return Err(format!("a step of 0 in the {label} field would mean \"never\""));
                }
                (r, step)
            }
            None => (part, 1),
        };

        let single = |tok: &str| -> Result<u32, String> {
            if let Some(lookup) = name_lookup {
                if let Some(v) = lookup(tok) {
                    return Ok(v);
                }
            }
            let v: u32 = tok.parse().map_err(|_| {
                format!("\"{tok}\" is not a number the {label} field understands (expected {min}-{max})")
            })?;
            if v < min || v > max {
                return Err(format!("{v} is outside {min}-{max} for the {label} field"));
            }
            Ok(v)
        };

        let (lo, hi) = if range_part == "*" {
            (min, max)
        } else if let Some((a, b)) = range_part.split_once('-') {
            let (a, b) = (single(a.trim())?, single(b.trim())?);
            if a > b {
                return Err(format!("{a}-{b} runs backwards in the {label} field"));
            }
            (a, b)
        } else {
            let v = single(range_part)?;
            // "5/15" means "from 5 onwards, every 15" — not "just 5".
            if step > 1 { (v, max) } else { (v, v) }
        };

        let mut v = lo;
        while v <= hi {
            out.push(v);
            v += step;
        }
    }
    out.sort_unstable();
    out.dedup();
    if out.is_empty() {
        return Err(format!("the {label} field matches nothing"));
    }
    Ok(out)
}

impl Schedule {
    /// Parses a standard five-field cron expression: minute, hour, day-of-month, month,
    /// day-of-week. Three-letter names are accepted for the last two.
    pub fn parse(expr: &str) -> Result<Self, String> {
        let fields: Vec<&str> = expr.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(format!(
                "a schedule needs exactly 5 parts — minute, hour, day of month, month, day of week — and this has {}",
                fields.len()
            ));
        }
        let day_of_week = parse_field(fields[4], "day of week", 0, 7, Some(dow_from_name))?
            .into_iter()
            // Both 0 and 7 mean Sunday in cron; collapse so matching has one answer.
            .map(|d| if d == 7 { 0 } else { d })
            .collect::<Vec<_>>();
        let mut day_of_week = day_of_week;
        day_of_week.sort_unstable();
        day_of_week.dedup();

        Ok(Schedule {
            minute: parse_field(fields[0], "minute", 0, 59, None)?,
            hour: parse_field(fields[1], "hour", 0, 23, None)?,
            day_of_month: parse_field(fields[2], "day of month", 1, 31, None)?,
            month: parse_field(fields[3], "month", 1, 12, Some(month_from_name))?,
            day_of_week,
            expr: expr.split_whitespace().collect::<Vec<_>>().join(" "),
            dom_restricted: fields[2].trim() != "*",
            dow_restricted: fields[4].trim() != "*",
        })
    }

    pub fn expr(&self) -> &str {
        &self.expr
    }

    /// True when `dt` falls on a minute this schedule selects.
    ///
    /// Day-of-month and day-of-week are OR'd when BOTH are restricted, which looks wrong
    /// and is what every cron implementation does: "0 0 1 * 1" means the 1st of the
    /// month AND every Monday, not "Mondays that are the 1st". Anything else would
    /// silently turn a common expression into one that fires a few times a decade.
    pub fn matches<Tz: TimeZone>(&self, dt: &DateTime<Tz>) -> bool {
        if !self.minute.contains(&dt.minute()) || !self.hour.contains(&dt.hour()) {
            return false;
        }
        if !self.month.contains(&dt.month()) {
            return false;
        }
        let dom_ok = self.day_of_month.contains(&dt.day());
        let dow_ok = self
            .day_of_week
            .contains(&dt.weekday().num_days_from_sunday());
        match (self.dom_restricted, self.dow_restricted) {
            (true, true) => dom_ok || dow_ok,
            _ => dom_ok && dow_ok,
        }
    }

    /// The most recent minute at or before `now` that this schedule selects.
    ///
    /// This is the whole scheduling primitive: comparing it against the last successful
    /// backup answers "are we overdue" without any dependence on the process having been
    /// awake, running, or even installed at the scheduled moment.
    pub fn previous_occurrence(&self, now: DateTime<Local>) -> Option<DateTime<Local>> {
        // Whole minutes only — a schedule has no opinion about seconds.
        let mut cursor = now.with_second(0)?.with_nanosecond(0)?;
        let limit = now - chrono::Duration::days(LOOKBACK_DAYS);
        // Days first, then minutes within a day: 400 day-checks plus at most 1440
        // minute-checks, instead of 576,000 minute-checks.
        while cursor >= limit {
            if self.day_could_match(&cursor) {
                let mut t = cursor;
                let day = t.day();
                loop {
                    if self.matches(&t) {
                        return Some(t);
                    }
                    let prev = t - chrono::Duration::minutes(1);
                    if prev.day() != day || prev < limit {
                        break;
                    }
                    t = prev;
                }
            }
            // Step to 23:59 of the previous day.
            let prev_day = cursor.date_naive() - chrono::Duration::days(1);
            cursor = Local
                .from_local_datetime(&prev_day.and_hms_opt(23, 59, 0)?)
                .earliest()?;
        }
        None
    }

    /// The next minute strictly after `now` that this schedule selects — shown to the
    /// owner as "next backup", so a schedule can be checked before it is trusted.
    pub fn next_occurrence(&self, now: DateTime<Local>) -> Option<DateTime<Local>> {
        let mut t = now.with_second(0)?.with_nanosecond(0)? + chrono::Duration::minutes(1);
        let limit = now + chrono::Duration::days(LOOKBACK_DAYS);
        while t <= limit {
            if self.day_could_match(&t) {
                let day = t.day();
                loop {
                    if self.matches(&t) {
                        return Some(t);
                    }
                    t += chrono::Duration::minutes(1);
                    if t.day() != day || t > limit {
                        break;
                    }
                }
            } else {
                let next_day = t.date_naive() + chrono::Duration::days(1);
                t = Local
                    .from_local_datetime(&next_day.and_hms_opt(0, 0, 0)?)
                    .earliest()?;
            }
        }
        None
    }

    /// Cheap date-only prefilter so the minute scan only runs on days that can match.
    fn day_could_match<Tz: TimeZone>(&self, dt: &DateTime<Tz>) -> bool {
        if !self.month.contains(&dt.month()) {
            return false;
        }
        let dom_ok = self.day_of_month.contains(&dt.day());
        let dow_ok = self
            .day_of_week
            .contains(&dt.weekday().num_days_from_sunday());
        match (self.dom_restricted, self.dow_restricted) {
            (true, true) => dom_ok || dow_ok,
            _ => dom_ok && dow_ok,
        }
    }

    /// The schedule in plain English.
    ///
    /// Only says what it can actually justify from the parsed fields. An expression this
    /// cannot phrase well falls back to a plainer sentence that is still true, because a
    /// confident wrong description of when backups run is worse than a clumsy right one.
    pub fn describe(&self) -> String {
        let when = self.describe_time();
        match self.describe_days() {
            Some(d) => format!("{d}, {when}"),
            // No day restriction. "Every day" is worth saying before a specific time —
            // "every day, at 02:00" — but in front of an interval it is noise that
            // reads as a contradiction: "every day, every 15 minutes".
            None if when.starts_with("at ") => format!("every day, {when}"),
            None => when,
        }
    }

    fn describe_time(&self) -> String {
        let every_minute = self.minute.len() == 60;
        let every_hour = self.hour.len() == 24;

        if every_minute && every_hour {
            return "every minute".into();
        }
        if let Some(step) = step_of(&self.minute, 60) {
            if every_hour {
                return format!("every {step} minutes");
            }
        }
        if self.minute.len() == 1 {
            let m = self.minute[0];
            if every_hour {
                return if m == 0 {
                    "every hour, on the hour".into()
                } else {
                    format!("every hour at :{m:02}")
                };
            }
            if let Some(step) = step_of(&self.hour, 24) {
                return format!("every {step} hours at :{m:02}");
            }
            if self.hour.len() == 1 {
                return format!("at {:02}:{:02}", self.hour[0], m);
            }
            let times: Vec<String> = self.hour.iter().map(|h| format!("{h:02}:{m:02}")).collect();
            return format!("at {}", join_list(&times));
        }
        // A step within a window of hours — "every 30 minutes between 09:00 and 17:59"
        // — is a schedule someone would plausibly want, and listing nine hours and two
        // minutes to describe it is not a description.
        if let Some(step) = step_of(&self.minute, 60) {
            if let Some((first, last)) = contiguous(&self.hour) {
                return format!("every {step} minutes between {first:02}:00 and {last:02}:59");
            }
        }
        // Several minutes and several hours: describable, but not prettily.
        format!(
            "at minute {} of hour {}",
            join_list(&self.minute.iter().map(|m| m.to_string()).collect::<Vec<_>>()),
            join_list(&self.hour.iter().map(|h| h.to_string()).collect::<Vec<_>>()),
        )
    }

    fn describe_days(&self) -> Option<String> {
        let month_part = if self.month.len() == 12 {
            None
        } else {
            Some(format!(
                "in {}",
                join_list(
                    &self.month.iter()
                        .map(|m| MONTH_NAMES[(*m as usize) - 1].to_string())
                        .collect::<Vec<_>>()
                )
            ))
        };

        let day_part = match (self.dom_restricted, self.dow_restricted) {
            (false, false) => None,
            (false, true) => {
                if self.day_of_week.len() == 7 {
                    None
                } else if self.day_of_week == [1, 2, 3, 4, 5] {
                    // Spelling out all five is technically right and reads badly; these
                    // two groupings are common enough to be worth naming.
                    Some("every weekday".into())
                } else if self.day_of_week == [0, 6] {
                    Some("every weekend".into())
                } else {
                    Some(format!(
                        "every {}",
                        join_list(
                            &self.day_of_week.iter()
                                .map(|d| DOW_NAMES[*d as usize].to_string())
                                .collect::<Vec<_>>()
                        )
                    ))
                }
            }
            (true, false) => Some(format!(
                "on the {} of the month",
                join_list(
                    &self.day_of_month.iter().map(|d| ordinal(*d)).collect::<Vec<_>>()
                )
            )),
            // OR semantics: say so, rather than implying an intersection.
            (true, true) => Some(format!(
                "on the {} of the month and every {}",
                join_list(&self.day_of_month.iter().map(|d| ordinal(*d)).collect::<Vec<_>>()),
                join_list(
                    &self.day_of_week.iter()
                        .map(|d| DOW_NAMES[*d as usize].to_string())
                        .collect::<Vec<_>>()
                ),
            )),
        };

        match (day_part, month_part) {
            (None, None) => None,
            (Some(d), None) => Some(d),
            (None, Some(m)) => Some(format!("every day {m}")),
            (Some(d), Some(m)) => Some(format!("{d} {m}")),
        }
    }
}

/// The step size of an evenly spaced field covering its whole range from 0, or None.
///
/// `*/15` on minutes expands to 0,15,30,45; this recovers the 15 so the description can
/// say "every 15 minutes" rather than listing them. A field that merely happens to be
/// evenly spaced but does not start at 0 (`5,20,35,50`) is not this, and returns None so
/// it gets listed honestly instead.
fn step_of(values: &[u32], range: u32) -> Option<u32> {
    if values.len() < 2 || values[0] != 0 {
        return None;
    }
    let step = values[1];
    if step == 0 || range % step != 0 || values.len() != (range / step) as usize {
        return None;
    }
    values.iter().enumerate().all(|(i, v)| *v == i as u32 * step).then_some(step)
}


/// First and last of an unbroken run of values, or None if there are gaps.
///
/// Only used to phrase an hour window; a set with holes gets listed instead, because
/// "between 09:00 and 17:59" would be a lie about a schedule that skips noon.
fn contiguous(values: &[u32]) -> Option<(u32, u32)> {
    if values.len() < 2 {
        return None;
    }
    values
        .windows(2)
        .all(|w| w[1] == w[0] + 1)
        .then(|| (values[0], values[values.len() - 1]))
}

fn ordinal(n: u32) -> String {
    let suffix = match (n % 10, n % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

fn join_list(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [a] => a.clone(),
        [a, b] => format!("{a} and {b}"),
        [rest @ .., last] => format!("{}, and {last}", rest.join(", ")),
    }
}

/// Whether a backup is overdue: the last occurrence this schedule selected has passed,
/// and nothing has succeeded since.
///
/// `last_ok` is the finish time of the most recent run that produced a restorable
/// snapshot. None means none ever did, which is due on the first occurrence that has
/// passed — not immediately on boot, so a freshly installed cluster does not start a
/// backup during first-boot setup.
pub fn is_due(
    schedule: &Schedule,
    last_ok: Option<DateTime<Local>>,
    now: DateTime<Local>,
) -> bool {
    let Some(occurrence) = schedule.previous_occurrence(now) else {
        return false;
    };
    match last_ok {
        Some(t) => t < occurrence,
        None => true,
    }
}

/// Parses and describes in one step for the API, so the sentence the page shows is
/// produced by the same code that will decide when backups actually run. A description
/// generated separately in the frontend could drift from the scheduler and be confidently
/// wrong about the one thing this screen exists to communicate.
pub fn preview(expr: &str) -> serde_json::Value {
    match Schedule::parse(expr) {
        Ok(s) => {
            let now = Local::now();
            let next = s.next_occurrence(now);
            serde_json::json!({
                "valid": true,
                "expr": s.expr(),
                "description": s.describe(),
                "timezone": now.format("%Z").to_string(),
                "next": next.map(|t| t.to_rfc3339()),
                "next_local": next.map(|t| t.format("%A %-d %B, %H:%M").to_string()),
            })
        }
        Err(e) => serde_json::json!({ "valid": false, "expr": expr, "error": e }),
    }
}


// ── Persistence ───────────────────────────────────────────────────────────────
//
// A ConfigMap rather than the backup Secret next door: this is not a credential, and a
// setting the owner edits should be readable with kubectl without handing out the B2
// keys that live in that Secret.

const SCHEDULE_CM: &str = "yolab-backup-schedule";
const SCHEDULE_NS: &str = "kube-system";

/// The configured schedule, or the default when none is stored or the stored one no
/// longer parses.
///
/// Falling back rather than failing is deliberate: this is called from the reconcile
/// tick, and an unreadable ConfigMap must not be a reason to stop backing up. The
/// default is what the fixed 24-hour scheduler used to do, so the quiet path is
/// unchanged behaviour rather than no behaviour.
pub async fn load() -> Schedule {
    let stored = crate::kubectl::get_json(&[
        "get", "configmap", SCHEDULE_CM, "-n", SCHEDULE_NS, "-o", "json",
    ])
    .await
    .ok()
    .and_then(|v| v["data"]["expr"].as_str().map(String::from));

    match stored {
        Some(expr) => Schedule::parse(&expr).unwrap_or_else(|e| {
            tracing::warn!("backup schedule {expr:?} is not usable ({e}) — falling back to {DEFAULT_EXPR}");
            Schedule::parse(DEFAULT_EXPR).expect("the default schedule must parse")
        }),
        None => Schedule::parse(DEFAULT_EXPR).expect("the default schedule must parse"),
    }
}

/// Whether a schedule has been chosen, so the page can say "using the default" instead
/// of implying the owner picked 02:00.
pub async fn is_configured() -> bool {
    crate::kubectl::get_json(&[
        "get", "configmap", SCHEDULE_CM, "-n", SCHEDULE_NS, "-o", "json",
    ])
    .await
    .ok()
    .and_then(|v| v["data"]["expr"].as_str().map(String::from))
    .is_some_and(|e| Schedule::parse(&e).is_ok())
}

async fn store(expr: &str) -> anyhow::Result<()> {
    let manifest = format!(
        "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: {SCHEDULE_CM}\n  namespace: {SCHEDULE_NS}\n  labels:\n    app.kubernetes.io/managed-by: yolab\ndata:\n  expr: \"{expr}\"\n"
    );
    crate::kubectl::apply(&manifest).await
}

// ── HTTP ──────────────────────────────────────────────────────────────────────

/// GET /api/backups/schedule
pub async fn get_schedule() -> axum::Json<serde_json::Value> {
    let s = load().await;
    let mut v = preview(s.expr());
    v["configured"] = serde_json::json!(is_configured().await);
    v["default"] = serde_json::json!(DEFAULT_EXPR);
    axum::Json(v)
}

#[derive(serde::Deserialize)]
pub struct SetScheduleReq {
    pub expr: String,
}

/// PUT /api/backups/schedule
///
/// Returns the same shape as GET, so the page renders the saved schedule from the
/// server's own description rather than the one it was showing while typing.
pub async fn set_schedule(
    axum::Json(req): axum::Json<SetScheduleReq>,
) -> (axum::http::StatusCode, axum::Json<serde_json::Value>) {
    let parsed = match Schedule::parse(&req.expr) {
        Ok(s) => s,
        Err(e) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({ "valid": false, "expr": req.expr, "error": e })),
            )
        }
    };
    // Store the normalised form, never the raw text: what comes back on the next read
    // is then exactly what the scheduler parsed.
    if let Err(e) = store(parsed.expr()).await {
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(serde_json::json!({ "valid": true, "error": e.to_string() })),
        );
    }
    let mut v = preview(parsed.expr());
    v["configured"] = serde_json::json!(true);
    v["default"] = serde_json::json!(DEFAULT_EXPR);
    (axum::http::StatusCode::OK, axum::Json(v))
}

#[derive(serde::Deserialize)]
pub struct PreviewQuery {
    pub expr: String,
}

/// GET /api/backups/schedule/preview?expr=... — the live sentence under the field.
///
/// Served rather than computed in the browser so the description cannot drift from the
/// scheduler: the same parser that decides when backups run is the one that writes the
/// sentence claiming when they will.
pub async fn preview_schedule(
    axum::extract::Query(q): axum::extract::Query<PreviewQuery>,
) -> axum::Json<serde_json::Value> {
    axum::Json(preview(&q.expr))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn local(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Local> {
        Local
            .from_local_datetime(&NaiveDate::from_ymd_opt(y, m, d).unwrap().and_hms_opt(h, min, 0).unwrap())
            .earliest()
            .unwrap()
    }

    fn sched(expr: &str) -> Schedule {
        Schedule::parse(expr).unwrap_or_else(|e| panic!("{expr} should parse: {e}"))
    }

    // ── Plain English ─────────────────────────────────────────────────────────
    //
    // These are the sentences the owner reads under the field. They are the whole
    // feature: a cron expression nobody can read is not a setting, it is a trap.

    #[test]
    fn the_presets_all_describe_themselves() {
        let cases = [
            ("*/15 * * * *", "every 15 minutes"),
            ("0 * * * *", "every hour, on the hour"),
            ("0 */6 * * *", "every 6 hours at :00"),
            ("0 2 * * *", "every day, at 02:00"),
            ("30 3 * * 0", "every Sunday, at 03:30"),
            ("0 4 1 * *", "on the 1st of the month, at 04:00"),
        ];
        for (expr, want) in cases {
            assert_eq!(sched(expr).describe(), want, "for {expr}");
        }
    }


    /// The full wording table, pinned. Every string here is something an owner reads
    /// and acts on, so a change to any of them should be a deliberate edit to this list
    /// rather than a surprise in the UI.
    #[test]
    fn the_wording_is_stable_across_the_expressions_people_write() {
        let cases = [
            ("*/5 * * * *", "every 5 minutes"),
            ("0 */2 * * *", "every 2 hours at :00"),
            ("0 2 * * 1-5", "every weekday, at 02:00"),
            ("0 2 * * 6,0", "every weekend, at 02:00"),
            ("15 2,14 * * *", "every day, at 02:15 and 14:15"),
            ("0 0 1 1 *", "on the 1st of the month in January, at 00:00"),
            ("5 4 * * sun", "every Sunday, at 04:05"),
            ("0 2 1,15 * *", "on the 1st and 15th of the month, at 02:00"),
            (
                "*/30 9-17 * * 1-5",
                "every weekday, every 30 minutes between 09:00 and 17:59",
            ),
        ];
        for (expr, want) in cases {
            assert_eq!(sched(expr).describe(), want, "for {expr}");
        }
    }

    /// An hour set with a hole must not be phrased as a window — "between 09:00 and
    /// 17:59" would claim backups at noon that never happen.
    #[test]
    fn a_gapped_hour_set_is_never_called_a_window() {
        assert_eq!(contiguous(&[9, 10, 11, 13, 14]), None);
        assert_eq!(contiguous(&[9, 10, 11]), Some((9, 11)));
        let d = sched("*/30 9-11,13-14 * * *").describe();
        assert!(!d.contains("between"), "got {d}");
    }

    #[test]
    fn named_days_and_months_are_accepted() {
        assert_eq!(sched("0 3 * * MON").describe(), "every Monday, at 03:00");
        assert_eq!(sched("0 3 * JAN *").describe(), "every day in January, at 03:00");
    }

    #[test]
    fn several_days_read_as_a_list() {
        assert_eq!(sched("0 2 * * 1,3,5").describe(), "every Monday, Wednesday, and Friday, at 02:00");
    }

    /// Both day fields restricted is OR in cron, and the description has to say so —
    /// reading it as an intersection would have someone expect a few runs a decade.
    #[test]
    fn both_day_fields_restricted_is_described_as_or() {
        let d = sched("0 0 1 * 1").describe();
        assert!(d.contains("1st of the month and every Monday"), "got {d}");
    }

    /// An evenly spaced field that does not start at 0 must be listed, not turned into
    /// a step it does not mean.
    #[test]
    fn an_offset_series_is_not_called_a_step() {
        assert_eq!(step_of(&[5, 20, 35, 50], 60), None);
        assert_eq!(step_of(&[0, 15, 30, 45], 60), Some(15));
    }

    // ── Errors a person can act on ────────────────────────────────────────────

    #[test]
    fn errors_name_the_field_that_is_wrong() {
        let cases = [
            ("0 2 * *", "5 parts"),
            ("60 2 * * *", "minute"),
            ("0 24 * * *", "hour"),
            ("0 2 32 * *", "day of month"),
            ("0 2 * 13 *", "month"),
            ("0 2 * * 9", "day of week"),
            ("*/0 2 * * *", "never"),
            ("0-  2 * * *", "minute"),
        ];
        for (expr, needle) in cases {
            let err = Schedule::parse(expr).unwrap_err();
            assert!(err.contains(needle), "{expr}: {err:?} should mention {needle:?}");
        }
    }

    // ── Occurrences ───────────────────────────────────────────────────────────

    #[test]
    fn the_previous_occurrence_of_a_daily_schedule_is_today_or_yesterday() {
        let s = sched("0 2 * * *");
        assert_eq!(s.previous_occurrence(local(2026, 8, 25, 9, 30)), Some(local(2026, 8, 25, 2, 0)));
        assert_eq!(s.previous_occurrence(local(2026, 8, 25, 1, 30)), Some(local(2026, 8, 24, 2, 0)));
    }

    /// Exactly on the minute counts as having occurred.
    #[test]
    fn an_occurrence_includes_its_own_minute() {
        let s = sched("0 2 * * *");
        assert_eq!(s.previous_occurrence(local(2026, 8, 25, 2, 0)), Some(local(2026, 8, 25, 2, 0)));
    }

    #[test]
    fn the_next_occurrence_is_strictly_in_the_future() {
        let s = sched("0 2 * * *");
        assert_eq!(s.next_occurrence(local(2026, 8, 25, 2, 0)), Some(local(2026, 8, 26, 2, 0)));
        assert_eq!(s.next_occurrence(local(2026, 8, 25, 1, 59)), Some(local(2026, 8, 25, 2, 0)));
    }

    #[test]
    fn a_weekly_schedule_finds_the_right_weekday() {
        // 2026-08-25 is a Tuesday; the previous Sunday is the 23rd.
        let s = sched("30 3 * * 0");
        assert_eq!(s.previous_occurrence(local(2026, 8, 25, 9, 0)), Some(local(2026, 8, 23, 3, 30)));
    }

    // ── is_due: the suspend case this whole module is shaped around ───────────

    /// The bug the old scheduler's comment warns about, in its new shape. A laptop
    /// asleep from 01:00 to 09:00 never sees 02:00 on the clock. It must still back up.
    #[test]
    fn a_laptop_that_slept_through_the_schedule_is_due_on_wake() {
        let s = sched("0 2 * * *");
        let woke = local(2026, 8, 25, 9, 0);
        let last_ok = Some(local(2026, 8, 24, 2, 5));
        assert!(is_due(&s, last_ok, woke));
    }

    #[test]
    fn a_backup_taken_after_the_occurrence_is_not_due_again() {
        let s = sched("0 2 * * *");
        let now = local(2026, 8, 25, 9, 0);
        assert!(!is_due(&s, Some(local(2026, 8, 25, 2, 5)), now));
        // ...until the next occurrence has passed.
        assert!(is_due(&s, Some(local(2026, 8, 25, 2, 5)), local(2026, 8, 26, 2, 0)));
    }

    #[test]
    fn a_cluster_that_has_never_backed_up_is_due() {
        assert!(is_due(&sched("0 2 * * *"), None, local(2026, 8, 25, 9, 0)));
    }

    /// Running late does not queue up the runs that were missed — one backup brings you
    /// current, and a machine off for a month must not wake to thirty of them.
    #[test]
    fn a_long_outage_still_produces_exactly_one_backup() {
        let s = sched("0 2 * * *");
        let now = local(2026, 8, 25, 9, 0);
        assert!(is_due(&s, Some(local(2026, 7, 1, 2, 5)), now));
        // The single catch-up run clears it.
        assert!(!is_due(&s, Some(now), now));
    }


    /// A `finishedAt` in the future relative to this node's clock — NTP correction, or a
    /// laptop resuming from suspend with a stale RTC — must read as "already backed up",
    /// not start a run every tick. The comparison against the occurrence handles it
    /// without a special case, and this pins that it stays true.
    #[test]
    fn a_clock_skewed_future_backup_does_not_trigger_a_storm() {
        let s = sched("0 2 * * *");
        let now = local(2026, 8, 25, 9, 0);
        assert!(!is_due(&s, Some(local(2026, 8, 25, 23, 0)), now));
        assert!(!is_due(&s, Some(local(2027, 1, 1, 0, 0)), now));
    }

    #[test]
    fn preview_reports_a_bad_expression_without_panicking() {
        let v = preview("nonsense");
        assert_eq!(v["valid"], serde_json::json!(false));
        assert!(v["error"].as_str().unwrap().contains("5 parts"));
    }

    #[test]
    fn preview_of_the_default_is_valid_and_has_a_next_run() {
        let v = preview(DEFAULT_EXPR);
        assert_eq!(v["valid"], serde_json::json!(true));
        assert_eq!(v["description"], serde_json::json!("every day, at 02:00"));
        assert!(v["next"].is_string());
    }

    /// Whitespace is normalised so a stored schedule round-trips to itself.
    #[test]
    fn an_expression_is_stored_normalised() {
        assert_eq!(sched("  0   2 *  * * ").expr(), "0 2 * * *");
    }
}
