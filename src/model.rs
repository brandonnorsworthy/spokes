//! Core domain types: node IDs, kinds, statuses and the node record.

use std::fmt;
use std::str::FromStr;

use schemars::JsonSchema;
use serde::Deserialize;

/// The display prefix for node IDs (`SPOKE-142`).
pub const ID_PREFIX: &str = "SPOKE-";

/// The id of the seeded root node ("To do").
pub const ROOT_ID: NodeId = NodeId(1);

/// The only user until auth exists; every row carries it so auth is not a rewrite.
pub const DEFAULT_USER: i64 = 1;

/// A node's global, never-reused id. Shown as `SPOKE-<n>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(pub i64);

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{ID_PREFIX}{}", self.0)
    }
}

impl FromStr for NodeId {
    type Err = String;

    /// Accepts `SPOKE-12`, `spoke-12`, `#12` or a bare `12`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let t = s.trim();
        let digits =
            if t.len() >= ID_PREFIX.len() && t[..ID_PREFIX.len()].eq_ignore_ascii_case(ID_PREFIX) {
                &t[ID_PREFIX.len()..]
            } else {
                t.strip_prefix('#').unwrap_or(t)
            };
        match digits.parse::<i64>() {
            Ok(n) if n > 0 => Ok(NodeId(n)),
            _ => Err(format!(
                "'{s}' is not a node id. Use the form SPOKE-12 (or just 12)."
            )),
        }
    }
}

/// An id as a tool argument: models send either a string (`"SPOKE-12"`) or a number (`12`).
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum IdArg {
    Number(i64),
    Text(String),
}

impl IdArg {
    pub fn parse(&self) -> Result<NodeId, String> {
        match self {
            IdArg::Number(n) if *n > 0 => Ok(NodeId(*n)),
            IdArg::Number(n) => Err(format!("{n} is not a node id. Use the form SPOKE-12.")),
            IdArg::Text(s) => s.parse(),
        }
    }
}

/// A number as a tool argument: models send `5` or, now and then, `"5"`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum NumArg {
    Number(i64),
    Text(String),
}

impl NumArg {
    pub fn parse(&self, field: &str) -> Result<i64, String> {
        match self {
            NumArg::Number(n) => Ok(*n),
            NumArg::Text(s) => s
                .trim()
                .parse()
                .map_err(|_| format!("{field} must be a whole number, not '{s}'.")),
        }
    }
}

/// `area` holds work forever and never finishes; `task` can be finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
#[schemars(inline)]
pub enum Kind {
    #[serde(alias = "Area", alias = "AREA")]
    Area,
    #[serde(alias = "Task", alias = "TASK")]
    Task,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Area => "area",
            Kind::Task => "task",
        }
    }
}

impl FromStr for Kind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "area" => Ok(Kind::Area),
            "task" => Ok(Kind::Task),
            other => Err(format!("unknown kind '{other}'")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Open,
    InProgress,
    Waiting,
    Done,
    Dropped,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Open => "open",
            Status::InProgress => "in_progress",
            Status::Waiting => "waiting",
            Status::Done => "done",
            Status::Dropped => "dropped",
        }
    }

    /// Done or dropped: no longer part of the live tree.
    pub fn is_closed(self) -> bool {
        matches!(self, Status::Done | Status::Dropped)
    }
}

impl FromStr for Status {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "open" => Ok(Status::Open),
            "in_progress" => Ok(Status::InProgress),
            "waiting" => Ok(Status::Waiting),
            "done" => Ok(Status::Done),
            "dropped" => Ok(Status::Dropped),
            other => Err(format!("unknown status '{other}'")),
        }
    }
}

/// The statuses `set_status` may set. Claims and questions have their own tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
#[schemars(inline)]
pub enum SettableStatus {
    #[serde(alias = "Done", alias = "DONE")]
    Done,
    #[serde(alias = "Dropped", alias = "DROPPED")]
    Dropped,
    #[serde(alias = "Open", alias = "OPEN", alias = "reopen")]
    Open,
}

impl From<SettableStatus> for Status {
    fn from(s: SettableStatus) -> Self {
        match s {
            SettableStatus::Done => Status::Done,
            SettableStatus::Dropped => Status::Dropped,
            SettableStatus::Open => Status::Open,
        }
    }
}

/// A node row without its notes body (listings never load bodies).
#[derive(Debug, Clone)]
pub struct Node {
    pub id: NodeId,
    pub parent_id: Option<NodeId>,
    pub position: String,
    pub kind: Kind,
    pub ordered: bool,
    pub title: String,
    pub status: Status,
    pub priority: Option<i64>,
    pub due_at: Option<String>,
    pub defer_until: Option<String>,
    pub waiting_on: Option<String>,
    pub claimed_by: Option<String>,
    pub claim_expires: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub has_notes: bool,
    /// A step of a written plan: the parent is a task with ordered children.
    pub is_step: bool,
    /// Direct children that are not dropped.
    pub child_total: i64,
    /// Direct children that are done.
    pub child_done: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_id_forms() {
        assert_eq!("SPOKE-12".parse::<NodeId>(), Ok(NodeId(12)));
        assert_eq!("spoke-12".parse::<NodeId>(), Ok(NodeId(12)));
        assert_eq!(" 7 ".parse::<NodeId>(), Ok(NodeId(7)));
        assert_eq!("#3".parse::<NodeId>(), Ok(NodeId(3)));
        assert!("SPOKE-".parse::<NodeId>().is_err());
        assert!("0".parse::<NodeId>().is_err());
        assert!("abc".parse::<NodeId>().is_err());
        assert_eq!(IdArg::Number(5).parse(), Ok(NodeId(5)));
        assert!(IdArg::Number(-1).parse().is_err());
        assert_eq!(NumArg::Text(" 5 ".into()).parse("depth"), Ok(5));
        assert!(NumArg::Text("five".into()).parse("depth").is_err());
    }

    #[test]
    fn displays_id() {
        assert_eq!(NodeId(142).to_string(), "SPOKE-142");
    }
}
