//! Plain-text output: one compact line per node, flags only when they are set.

use chrono::{DateTime, NaiveDate, Utc};

use crate::model::{Kind, Node, Status};

/// Today's date (UTC for now; a configured time zone arrives with deadlines).
pub fn today() -> NaiveDate {
    Utc::now().date_naive()
}

/// `2026-11-20` -> `Nov 20`, or `Nov 20 2027` outside the current year.
pub fn short_date(value: &str, today: NaiveDate) -> String {
    let Some(date) = parse_date(value) else {
        return value.to_string();
    };
    if date.format("%Y").to_string() == today.format("%Y").to_string() {
        date.format("%b %-d").to_string()
    } else {
        date.format("%b %-d %Y").to_string()
    }
}

/// Reads the date part of an ISO date or RFC 3339 timestamp.
pub fn parse_date(value: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(value.get(..10)?, "%Y-%m-%d").ok()
}

/// `3h ago`, `2d ago`, `just now`.
pub fn ago(timestamp: &str, now: DateTime<Utc>) -> String {
    let Ok(then) = DateTime::parse_from_rfc3339(timestamp) else {
        return timestamp.to_string();
    };
    let secs = (now - then.with_timezone(&Utc)).num_seconds().max(0);
    match secs {
        0..60 => "just now".to_string(),
        60..3600 => format!("{}m ago", secs / 60),
        3600..86400 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86400),
    }
}

/// Task progress over direct children that are not dropped, e.g. `(2/6)`.
/// Areas never finish, so they show no progress.
pub fn progress(node: &Node) -> Option<String> {
    (node.kind == Kind::Task && node.child_total > 0)
        .then(|| format!("({}/{})", node.child_done, node.child_total))
}

/// A task with no children and no notes needs research before work. A step of an
/// ordered task is already part of a written plan, so it is not flagged.
pub fn is_unplanned(node: &Node) -> bool {
    node.kind == Kind::Task
        && !node.status.is_closed()
        && node.child_total == 0
        && !node.has_notes
        && !node.is_step
}

/// The earlier of two optional due dates (the binding deadline from above).
pub fn earliest<'a>(a: Option<&'a str>, b: Option<&'a str>) -> Option<&'a str> {
    match (a, b) {
        (Some(x), Some(y)) => Some(if parse_date(y) < parse_date(x) { y } else { x }),
        (x, None) => x,
        (None, y) => y,
    }
}

/// Flags shared by every line style: priority, dates, order, progress, notes, plan state.
/// `parent_due` is the earliest due date above the node; a later own due date gets a warning.
fn detail_flags(node: &Node, today: NaiveDate, parent_due: Option<&str>, out: &mut Vec<String>) {
    if let Some(p) = node.priority {
        out.push(format!("p{p}"));
    }
    if let Some(due) = &node.due_at {
        out.push(format!("due {}", short_date(due, today)));
        if let Some(bound) = parent_due
            && parse_date(due) > parse_date(bound)
        {
            out.push(format!("(after parent's {})", short_date(bound, today)));
        }
    }
    if let Some(defer) = &node.defer_until
        && parse_date(defer).is_some_and(|d| d > today)
    {
        out.push(format!("after {}", short_date(defer, today)));
    }
    if node.ordered {
        out.push("ordered".to_string());
    }
    if let Some(p) = progress(node) {
        out.push(p);
    }
    if node.has_notes {
        out.push("+notes".to_string());
    }
    if is_unplanned(node) {
        out.push("unplanned".to_string());
    }
}

/// The first line of `get`: `SPOKE-142 task p1 due Nov 20 ordered (2/6) +notes  Title`.
pub fn header_line(node: &Node, today: NaiveDate, parent_due: Option<&str>) -> String {
    let mut flags = vec![node.id.to_string(), node.kind.as_str().to_string()];
    if node.status != Status::Open {
        flags.push(node.status.as_str().to_string());
    }
    detail_flags(node, today, parent_due, &mut flags);
    format!("{}  {}", flags.join(" "), node.title)
}

/// A child line in an outline: `SPOKE-145 open  Add secrets`, `SPOKE-2 area  Home`.
pub fn child_line(node: &Node, today: NaiveDate, parent_due: Option<&str>) -> String {
    let mut flags = vec![node.id.to_string()];
    match node.kind {
        Kind::Area if node.status == Status::Open => flags.push("area".to_string()),
        Kind::Area => flags.push(format!("area {}", node.status.as_str())),
        Kind::Task => flags.push(node.status.as_str().to_string()),
    }
    detail_flags(node, today, parent_due, &mut flags);
    format!("{}  {}", flags.join(" "), node.title)
}

/// A queue line in `next`: `SPOKE-142  p1 due Nov 20  Title (2/6)`.
pub fn queue_line(node: &Node, today: NaiveDate) -> String {
    let mut flags = Vec::new();
    if let Some(p) = node.priority {
        flags.push(format!("p{p}"));
    }
    if let Some(due) = &node.due_at {
        flags.push(format!("due {}", short_date(due, today)));
    }
    let mut line = node.id.to_string();
    if !flags.is_empty() {
        line.push_str("  ");
        line.push_str(&flags.join(" "));
    }
    line.push_str("  ");
    line.push_str(&node.title);
    if let Some(p) = progress(node) {
        line.push(' ');
        line.push_str(&p);
    }
    if is_unplanned(node) {
        line.push_str("  unplanned");
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::NodeId;

    fn node() -> Node {
        Node {
            id: NodeId(142),
            parent_id: Some(NodeId(1)),
            position: "V".into(),
            kind: Kind::Task,
            ordered: true,
            title: "Deploy game to production".into(),
            status: Status::Open,
            priority: Some(1),
            due_at: Some("2026-11-20".into()),
            defer_until: None,
            waiting_on: None,
            claimed_by: None,
            claim_expires: None,
            created_at: "2026-11-01T00:00:00Z".into(),
            updated_at: "2026-11-01T00:00:00Z".into(),
            has_notes: true,
            is_step: false,
            child_total: 6,
            child_done: 2,
        }
    }

    fn day() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 11, 14).unwrap()
    }

    #[test]
    fn header_matches_spec_style() {
        assert_eq!(
            header_line(&node(), day(), None),
            "SPOKE-142 task p1 due Nov 20 ordered (2/6) +notes  Deploy game to production"
        );
    }

    #[test]
    fn unplanned_leaf_is_flagged() {
        let mut n = node();
        n.child_total = 0;
        n.child_done = 0;
        n.has_notes = false;
        n.priority = None;
        n.due_at = None;
        n.ordered = false;
        assert_eq!(
            child_line(&n, day(), None),
            "SPOKE-142 open unplanned  Deploy game to production"
        );
        assert_eq!(
            queue_line(&n, day()),
            "SPOKE-142  Deploy game to production  unplanned"
        );
    }

    #[test]
    fn plan_steps_are_not_unplanned_and_late_due_warns() {
        let mut n = node();
        n.child_total = 0;
        n.child_done = 0;
        n.has_notes = false;
        n.priority = None;
        n.ordered = false;
        n.is_step = true;
        n.due_at = Some("2026-12-01".into());
        assert_eq!(
            child_line(&n, day(), Some("2026-11-20")),
            "SPOKE-142 open due Dec 1 (after parent's Nov 20)  Deploy game to production"
        );
        assert_eq!(
            child_line(&n, day(), Some("2026-12-05")),
            "SPOKE-142 open due Dec 1  Deploy game to production"
        );
        assert_eq!(
            earliest(Some("2026-12-05"), Some("2026-11-20")),
            Some("2026-11-20")
        );
    }

    #[test]
    fn dates_outside_this_year_show_the_year() {
        assert_eq!(short_date("2027-01-02", day()), "Jan 2 2027");
        assert_eq!(short_date("2026-11-20T10:00:00Z", day()), "Nov 20");
    }
}
