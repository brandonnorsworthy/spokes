//! Recurring tasks: the repeat rule stored in `nodes.recur` and the date arithmetic
//! that resets a task in place when it is marked done.
//!
//! A rule is `{"every":2,"unit":"week","from":"completion"}`: the next due date is
//! counted from the day it was done (`completion`) or from the previous due date
//! (`schedule`). Month and year steps clamp to the end of a short month (Jan 31 + 1
//! month = Feb 28 or 29); a `schedule` rule remembers the day of month it started on
//! (`day`), so a bill due on the 31st comes back on Mar 31, not Mar 28.
//!
//! After a reset the task is hidden (`defer_until`) until a lead time before the next
//! due date, proportional to the interval: a quarter of it, at most 14 days (daily: due
//! day itself; weekly: 1 day before; every 2 weeks: 3 days; monthly: 7; yearly: 14).

use chrono::{Datelike, Duration, Months, NaiveDate};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::model::NumArg;

/// The longest lead time before a recurring task's next due date.
const MAX_LEAD_DAYS: i64 = 14;
const MAX_EVERY: i64 = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Unit {
    Day,
    Week,
    Month,
    Year,
}

impl Unit {
    fn name(self) -> &'static str {
        match self {
            Unit::Day => "day",
            Unit::Week => "week",
            Unit::Month => "month",
            Unit::Year => "year",
        }
    }

    fn parse(s: &str) -> Option<Unit> {
        let s = s.trim().to_ascii_lowercase();
        let s = s.strip_suffix('s').unwrap_or(&s);
        Some(match s {
            "day" | "d" | "daily" => Unit::Day,
            "week" | "w" | "wk" | "weekly" => Unit::Week,
            "month" | "m" | "mo" | "monthly" => Unit::Month,
            "year" | "y" | "yr" | "yearly" | "annual" | "annually" => Unit::Year,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Anchor {
    Completion,
    Schedule,
}

/// A validated repeat rule, stored as JSON in `nodes.recur`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    pub every: i64,
    pub unit: Unit,
    pub from: Anchor,
    /// Day of month a `schedule` month/year rule is anchored to (set on first reset).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub day: Option<u32>,
}

/// The `recur` tool argument: an object, or a short text form ("every 2 weeks").
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum RecurArg {
    Rule(RecurSpec),
    Text(String),
}

/// `{"every": 2, "unit": "week", "from": "completion"}`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct RecurSpec {
    /// How many units between occurrences (default 1).
    #[schemars(with = "Option<i64>")]
    pub every: Option<NumArg>,
    /// "day", "week", "month" or "year".
    pub unit: String,
    /// "completion" (count from the day it is done, the default) or "schedule" (from the previous due date, for fixed dates like bills).
    pub from: Option<String>,
}

/// What a `recur` argument asks for: set a rule, or clear it (`""` / `"none"`).
pub enum RecurChange {
    Set(Rule),
    Clear,
}

const HELP: &str = "Use {\"every\": 2, \"unit\": \"week\", \"from\": \"completion\"} (unit day, week, month or year; from completion or schedule), or text like \"every 2 weeks\".";

impl RecurArg {
    pub fn parse(&self) -> Result<RecurChange, String> {
        match self {
            RecurArg::Rule(spec) => {
                let every = spec.every.as_ref().map(|e| e.parse("every")).transpose()?;
                let from = spec.from.as_deref().unwrap_or("completion");
                build(every, &spec.unit, from).map(RecurChange::Set)
            }
            RecurArg::Text(t) => parse_text(t),
        }
    }
}

fn build(every: Option<i64>, unit: &str, from: &str) -> Result<Rule, String> {
    let every = every.unwrap_or(1);
    if !(1..=MAX_EVERY).contains(&every) {
        return Err(format!(
            "recur every must be 1 to {MAX_EVERY}, not {every}."
        ));
    }
    let unit = Unit::parse(unit)
        .ok_or_else(|| format!("recur unit '{unit}' is not day, week, month or year. {HELP}"))?;
    let from = match from.trim().to_ascii_lowercase().as_str() {
        "completion" | "done" | "completed" => Anchor::Completion,
        "schedule" | "due" | "scheduled" => Anchor::Schedule,
        other => {
            return Err(format!(
                "recur from '{other}' is not completion or schedule. {HELP}"
            ));
        }
    };
    Ok(Rule {
        every,
        unit,
        from,
        day: None,
    })
}

/// `every 2 weeks`, `2 weeks`, `weekly`, `monthly from schedule`, `every month on schedule`,
/// or the JSON object written as a string. `""` or `none` clears the rule.
fn parse_text(raw: &str) -> Result<RecurChange, String> {
    let t = raw.trim();
    if t.is_empty() || t.eq_ignore_ascii_case("none") || t.eq_ignore_ascii_case("never") {
        return Ok(RecurChange::Clear);
    }
    if t.starts_with('{') {
        let spec: RecurSpec =
            serde_json::from_str(t).map_err(|_| format!("recur '{t}' is not a rule. {HELP}"))?;
        return RecurArg::Rule(spec).parse();
    }
    let lower = t.to_ascii_lowercase();
    let mut words: Vec<&str> = lower.split_whitespace().collect();
    let mut from = "completion";
    if let Some(i) = words
        .iter()
        .position(|w| matches!(*w, "from" | "after" | "on" | "by"))
    {
        from = match words[i + 1..].join(" ").as_str() {
            "completion" | "done" | "completed" | "i do it" => "completion",
            "schedule" | "due" | "due date" | "the due date" | "scheduled" => "schedule",
            _ => return Err(format!("recur '{t}' is not a rule. {HELP}")),
        };
        words.truncate(i);
    }
    if words.first() == Some(&"every") {
        words.remove(0);
    }
    let (every, unit) = match words.as_slice() {
        [unit] => (None, *unit),
        [n, unit] => (
            Some(
                n.parse::<i64>()
                    .map_err(|_| format!("recur '{t}' is not a rule. {HELP}"))?,
            ),
            *unit,
        ),
        _ => return Err(format!("recur '{t}' is not a rule. {HELP}")),
    };
    build(every, unit, from).map(RecurChange::Set)
}

impl Rule {
    pub fn to_json(self) -> String {
        serde_json::to_string(&self).expect("rule serializes")
    }

    pub fn from_json(s: &str) -> Option<Rule> {
        serde_json::from_str(s).ok()
    }

    /// `every 2 weeks, counted from completion`.
    pub fn describe(self) -> String {
        let unit = if self.every == 1 {
            self.unit.name().to_string()
        } else {
            format!("{} {}s", self.every, self.unit.name())
        };
        let from = match self.from {
            Anchor::Completion => "counted from completion",
            Anchor::Schedule => "counted from the due date",
        };
        format!("every {unit}, {from}")
    }

    /// Rough length of one interval in days, for the lead time.
    fn interval_days(self) -> i64 {
        self.every
            * match self.unit {
                Unit::Day => 1,
                Unit::Week => 7,
                Unit::Month => 30,
                Unit::Year => 365,
            }
    }

    /// How many days before the next due date the task shows again.
    pub fn lead_days(self) -> i64 {
        (self.interval_days() / 4).min(MAX_LEAD_DAYS)
    }

    /// `base` moved forward by `n` intervals, clamping month ends. `day` is the
    /// day-of-month anchor for month and year steps.
    fn step(self, base: NaiveDate, n: i64, day: Option<u32>) -> NaiveDate {
        match self.unit {
            Unit::Day => base + Duration::days(self.every * n),
            Unit::Week => base + Duration::weeks(self.every * n),
            Unit::Month | Unit::Year => {
                let months = self.every * n * if self.unit == Unit::Year { 12 } else { 1 };
                let first = base.with_day(1).expect("day 1 exists");
                let month = first + Months::new(u32::try_from(months).unwrap_or(u32::MAX));
                let want = day.unwrap_or(base.day());
                let last = last_day(month);
                month.with_day(want.min(last)).expect("clamped day exists")
            }
        }
    }

    /// The next occurrence after a completion on `today`, given the current due date.
    /// Returns the next due date, the hide-until date (None if it should show now) and
    /// the rule to store (a schedule rule learns its day-of-month anchor).
    pub fn next(self, due: Option<NaiveDate>, today: NaiveDate) -> Occurrence {
        let mut rule = self;
        let next_due = match (self.from, due) {
            (Anchor::Schedule, Some(due)) => {
                if matches!(self.unit, Unit::Month | Unit::Year) && rule.day.is_none() {
                    rule.day = Some(due.day());
                }
                // At least one interval past the old due date; a late completion skips
                // the periods already gone by, so it does not come back overdue.
                let mut n = 1;
                let mut d = self.step(due, n, rule.day);
                while d <= today && n < 100_000 {
                    n += 1;
                    d = self.step(due, n, rule.day);
                }
                d
            }
            // Without a due date there is no schedule yet: count from today.
            _ => self.step(today, 1, None),
        };
        let show = next_due - Duration::days(self.lead_days());
        Occurrence {
            due: next_due,
            defer_until: (show > today).then_some(show),
            rule,
        }
    }
}

pub struct Occurrence {
    pub due: NaiveDate,
    pub defer_until: Option<NaiveDate>,
    pub rule: Rule,
}

fn last_day(month_start: NaiveDate) -> u32 {
    let next = month_start + Months::new(1);
    (next - Duration::days(1)).day()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn rule(t: &str) -> Rule {
        match RecurArg::Text(t.into()).parse().unwrap() {
            RecurChange::Set(r) => r,
            RecurChange::Clear => panic!("cleared"),
        }
    }

    #[test]
    fn parses_text_and_json_forms() {
        assert_eq!(
            rule("every 2 weeks"),
            Rule {
                every: 2,
                unit: Unit::Week,
                from: Anchor::Completion,
                day: None
            }
        );
        assert_eq!(rule("monthly from schedule").from, Anchor::Schedule);
        assert_eq!(rule("1 year").unit, Unit::Year);
        assert_eq!(
            rule(r#"{"every":3,"unit":"days","from":"schedule"}"#).every,
            3
        );
        assert!(matches!(
            RecurArg::Text(" ".into()).parse(),
            Ok(RecurChange::Clear)
        ));
        for bad in [
            "every fortnight",
            "every 0 days",
            "2 weeks from lunch",
            "a b c",
        ] {
            assert!(RecurArg::Text(bad.into()).parse().is_err(), "{bad}");
        }
        assert_eq!(
            rule("every 2 weeks").to_json(),
            r#"{"every":2,"unit":"week","from":"completion"}"#
        );
    }

    #[test]
    fn month_steps_clamp_and_keep_their_anchor() {
        let r = rule("every month from schedule");
        let o = r.next(Some(d("2027-01-31")), d("2027-01-30"));
        assert_eq!(o.due, d("2027-02-28"));
        assert_eq!(o.rule.day, Some(31));
        let o2 = o.rule.next(Some(o.due), d("2027-02-27"));
        assert_eq!(o2.due, d("2027-03-31"));
        // Leap year.
        assert_eq!(
            r.next(Some(d("2028-01-31")), d("2028-01-31")).due,
            d("2028-02-29")
        );
        // From completion: Jan 31 + 1 month = Feb 28, no anchor.
        let c = rule("every month");
        let o = c.next(Some(d("2027-01-05")), d("2027-01-31"));
        assert_eq!(o.due, d("2027-02-28"));
        assert_eq!(o.rule.day, None);
        // Yearly from Feb 29.
        assert_eq!(
            rule("yearly from schedule")
                .next(Some(d("2028-02-29")), d("2028-02-29"))
                .due,
            d("2029-02-28")
        );
    }

    #[test]
    fn late_schedule_completion_skips_missed_periods() {
        let r = rule("every week from schedule");
        let o = r.next(Some(d("2026-11-01")), d("2026-11-14"));
        assert_eq!(o.due, d("2026-11-15"));
    }

    #[test]
    fn lead_time_scales_with_the_interval() {
        let today = d("2026-11-14");
        let o = rule("every day").next(None, today);
        assert_eq!(
            (o.due, o.defer_until),
            (d("2026-11-15"), Some(d("2026-11-15")))
        );
        let o = rule("every 2 weeks").next(None, today);
        assert_eq!(
            (o.due, o.defer_until),
            (d("2026-11-28"), Some(d("2026-11-25")))
        );
        assert_eq!(rule("monthly").lead_days(), 7);
        assert_eq!(rule("yearly").lead_days(), 14);
        // Due again within the lead time: shown right away.
        let o = rule("every week from schedule").next(Some(d("2026-11-14")), d("2026-11-20"));
        assert_eq!((o.due, o.defer_until), (d("2026-11-21"), None));
    }
}
