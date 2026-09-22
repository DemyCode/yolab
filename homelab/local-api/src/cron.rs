
use chrono::{DateTime, Datelike, Duration, Timelike, Utc};

const MAX_LOOKBACK_MINUTES: i64 = 366 * 24 * 60;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cron {
    minutes: [bool; 60],
    hours: [bool; 24],
    days: [bool; 32],
    months: [bool; 13],
    weekdays: [bool; 7],
    dom_restricted: bool,
    dow_restricted: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub struct CronError(pub String);

impl std::fmt::Display for CronError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for CronError {}

impl Cron {
    pub fn parse(expr: &str) -> Result<Cron, CronError> {
        let fields: Vec<&str> = expr.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(CronError(format!(
                "a schedule needs 5 fields (minute hour day month weekday), got {}",
                fields.len()
            )));
        }

        let minutes = parse_field(fields[0], 0, 59, "minute")?;
        let hours = parse_field(fields[1], 0, 23, "hour")?;
        let days = parse_field(fields[2], 1, 31, "day")?;
        let months = parse_field(fields[3], 1, 12, "month")?;

        let raw_weekdays = parse_field(fields[4], 0, 7, "weekday")?;
        let mut weekdays = [false; 7];
        for (i, on) in raw_weekdays.iter().enumerate() {
            if *on {
                weekdays[i % 7] = true;
            }
        }

        let mut m = [false; 60];
        for (i, on) in minutes.iter().enumerate() {
            m[i] = *on;
        }
        let mut h = [false; 24];
        for (i, on) in hours.iter().enumerate() {
            h[i] = *on;
        }
        let mut d = [false; 32];
        for (i, on) in days.iter().enumerate() {
            d[i] = *on;
        }
        let mut mo = [false; 13];
        for (i, on) in months.iter().enumerate() {
            mo[i] = *on;
        }

        Ok(Cron {
            minutes: m,
            hours: h,
            days: d,
            months: mo,
            weekdays,
            dom_restricted: fields[2].trim() != "*",
            dow_restricted: fields[4].trim() != "*",
        })
    }

    pub fn matches(&self, dt: &DateTime<Utc>) -> bool {
        let dom = self.days[dt.day() as usize];
        let dow = self.weekdays[dt.weekday().num_days_from_sunday() as usize];
        let day_ok = match (self.dom_restricted, self.dow_restricted) {
            (true, true) => dom || dow,
            (true, false) => dom,
            (false, true) => dow,
            (false, false) => true,
        };
        day_ok
            && self.minutes[dt.minute() as usize]
            && self.hours[dt.hour() as usize]
            && self.months[dt.month() as usize]
    }

    pub fn last_fire(&self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let mut t = now
            .with_second(0)
            .and_then(|t| t.with_nanosecond(0))
            .unwrap_or(now);
        for _ in 0..MAX_LOOKBACK_MINUTES {
            if self.matches(&t) {
                return Some(t);
            }
            t -= Duration::minutes(1);
        }
        None
    }

    pub fn due(&self, last_ok: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
        let Some(last) = last_ok else {
            return true;
        };
        match self.last_fire(now) {
            Some(fire) => last < fire,
            None => false,
        }
    }
}

fn parse_field(field: &str, lo: u32, hi: u32, name: &str) -> Result<Vec<bool>, CronError> {
    let mut set = vec![false; (hi + 1) as usize];
    for part in field.split(',') {
        let part = part.trim();
        if part.is_empty() {
            return Err(CronError(format!("{name}: empty item in list")));
        }
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => {
                let step: u32 = s
                    .parse()
                    .map_err(|_| CronError(format!("{name}: '{s}' is not a step")))?;
                if step == 0 {
                    return Err(CronError(format!("{name}: step cannot be 0")));
                }
                (r, step)
            }
            None => (part, 1),
        };
        let (start, end) = if range == "*" {
            (lo, hi)
        } else if let Some((a, b)) = range.split_once('-') {
            let a: u32 = a
                .parse()
                .map_err(|_| CronError(format!("{name}: '{a}' is not a number")))?;
            let b: u32 = b
                .parse()
                .map_err(|_| CronError(format!("{name}: '{b}' is not a number")))?;
            if a > b {
                return Err(CronError(format!("{name}: range {a}-{b} is backwards")));
            }
            (a, b)
        } else {
            let n: u32 = range
                .parse()
                .map_err(|_| CronError(format!("{name}: '{range}' is not a number")))?;
            (n, if step > 1 { hi } else { n })
        };
        if start < lo || end > hi {
            return Err(CronError(format!(
                "{name}: {start}-{end} is outside {lo}-{hi}"
            )));
        }
        let mut v = start;
        while v <= end {
            set[v as usize] = true;
            v += step;
        }
    }
    Ok(set)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
    }

    #[test]
    fn parses_the_common_shapes() {
        assert!(Cron::parse("* * * * *").is_ok());
        assert!(Cron::parse("0 3 * * *").is_ok());
        assert!(Cron::parse("*/15 9-17 * * 1-5").is_ok());
        assert!(Cron::parse("0 0 1,15 * 0").is_ok());
        assert!(Cron::parse("30 4/6 * * *").is_ok());
    }

    #[test]
    fn rejects_malformed_expressions() {
        assert!(Cron::parse("* * * *").is_err(), "four fields");
        assert!(Cron::parse("60 * * * *").is_err(), "minute out of range");
        assert!(Cron::parse("* * 32 * *").is_err(), "day out of range");
        assert!(Cron::parse("* * * * 8").is_err(), "weekday out of range");
        assert!(Cron::parse("*/0 * * * *").is_err(), "zero step");
        assert!(Cron::parse("5-1 * * * *").is_err(), "backwards range");
    }

    #[test]
    fn matches_a_daily_time() {
        let c = Cron::parse("0 3 * * *").unwrap();
        assert!(c.matches(&at(2026, 9, 17, 3, 0)));
        assert!(!c.matches(&at(2026, 9, 17, 3, 1)));
        assert!(!c.matches(&at(2026, 9, 17, 2, 59)));
    }

    #[test]
    fn weekday_seven_means_sunday() {
        let c = Cron::parse("0 0 * * 7").unwrap();
        assert!(c.matches(&at(2026, 9, 20, 0, 0)));
        assert!(!c.matches(&at(2026, 9, 21, 0, 0)));
    }

    #[test]
    fn when_both_day_fields_are_restricted_either_matches() {
        let c = Cron::parse("0 0 1 * 1").unwrap();
        assert!(c.matches(&at(2026, 9, 1, 0, 0)), "the 1st");
        assert!(c.matches(&at(2026, 9, 7, 0, 0)), "a Monday");
        assert!(
            !c.matches(&at(2026, 9, 8, 0, 0)),
            "a Tuesday that is not the 1st"
        );
    }

    #[test]
    fn due_only_after_the_next_fire_time() {
        let c = Cron::parse("0 3 * * *").unwrap();
        assert!(c.due(None, at(2026, 9, 17, 12, 0)), "never run is due");
        assert!(
            !c.due(Some(at(2026, 9, 17, 3, 0)), at(2026, 9, 17, 12, 0)),
            "ran this morning"
        );
        assert!(
            c.due(Some(at(2026, 9, 16, 3, 0)), at(2026, 9, 17, 12, 0)),
            "missed this morning"
        );
        assert!(
            !c.due(Some(at(2026, 9, 16, 3, 0)), at(2026, 9, 17, 2, 0)),
            "not yet this morning"
        );
    }

    #[test]
    fn last_fire_finds_todays_slot() {
        let c = Cron::parse("0 3 * * *").unwrap();
        assert_eq!(
            c.last_fire(at(2026, 9, 17, 12, 0)),
            Some(at(2026, 9, 17, 3, 0))
        );
    }

    #[test]
    fn a_never_matching_schedule_is_never_due_after_a_run() {
        let c = Cron::parse("0 0 31 2 *").unwrap();
        assert!(!c.due(Some(at(2026, 1, 1, 0, 0)), at(2026, 9, 17, 12, 0)));
    }
}
