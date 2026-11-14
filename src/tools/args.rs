//! Tool arguments. Doc comments become the JSON Schema descriptions models read,
//! so they stay short. IDs are declared as strings but bare numbers are accepted too.

use schemars::JsonSchema;
use serde::Deserialize;

use crate::model::{IdArg, Kind, SettableStatus};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AddArgs {
    /// Parent id, e.g. "SPOKE-1". Omit to add under the root ("To do").
    #[schemars(with = "Option<String>")]
    pub parent: Option<IdArg>,
    /// Short title.
    pub title: String,
    /// "task" (can be finished, the default) or "area" (a permanent bucket like Home or Work that never finishes).
    pub kind: Option<Kind>,
    /// true if this node's children are steps to do in order.
    pub ordered: Option<bool>,
    /// 1 (highest) to 4. Omit to inherit.
    pub priority: Option<i64>,
    /// Deadline as YYYY-MM-DD.
    pub due: Option<String>,
    /// Hide from the queue until this date, YYYY-MM-DD.
    pub defer_until: Option<String>,
    /// 2 to 5 other phrases this could be searched by: synonyms, plain spelling, what you'd call it out loud.
    pub aliases: Option<Vec<String>>,
    /// Longer body: plan, research, links.
    pub notes: Option<String>,
    /// Your agent name, for the history log.
    pub agent: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetArgs {
    /// Node id, e.g. "SPOKE-12". Omit for the root.
    #[schemars(with = "Option<String>")]
    pub id: Option<IdArg>,
    /// Levels of children to show (default 1, max 5).
    pub depth: Option<u32>,
    /// Include the notes body (default false).
    pub notes: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NextArgs {
    /// Only look under this node, e.g. "SPOKE-3". Omit for everything.
    #[schemars(with = "Option<String>")]
    pub scope: Option<IdArg>,
    /// Max lines (default 5).
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SetStatusArgs {
    /// Node ids, e.g. ["SPOKE-12", "SPOKE-13"].
    #[schemars(with = "Vec<String>")]
    pub ids: Vec<IdArg>,
    /// "done" (finished), "dropped" (won't do; replaces delete) or "open" (reopen).
    pub status: SettableStatus,
    /// Your agent name, for the history log.
    pub agent: Option<String>,
}
