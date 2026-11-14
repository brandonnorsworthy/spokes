//! Tool arguments. Doc comments become the JSON Schema descriptions models read,
//! so they stay short. IDs are declared as strings but bare numbers are accepted too;
//! numbers are declared as integers but numeric strings ("5") are accepted too.

use schemars::JsonSchema;
use serde::Deserialize;

use crate::model::{IdArg, Kind, NumArg, SettableStatus};

/// One node to create, with optional nested children (a whole plan in one call).
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct NodeSpec {
    /// Short title.
    pub title: String,
    /// "task" (can be finished, the default) or "area" (a permanent bucket like Home or Work that never finishes).
    pub kind: Option<Kind>,
    /// true if this node's children are steps to do in order (only the first unfinished one is actionable).
    pub ordered: Option<bool>,
    /// 1 (highest) to 4. Omit to inherit.
    #[schemars(with = "Option<i64>")]
    pub priority: Option<NumArg>,
    /// Deadline as YYYY-MM-DD.
    pub due: Option<String>,
    /// Hide from the queue until this date, YYYY-MM-DD.
    pub defer_until: Option<String>,
    /// 2 to 5 other phrases this could be searched by: synonyms, plain spelling, what you'd call it out loud.
    pub aliases: Option<Vec<String>>,
    /// Longer body: plan, research, links. Never shown in listings.
    pub notes: Option<String>,
    /// Nodes to create beneath this one, in order. Each takes the same fields (title, kind, ordered, notes, children...).
    pub children: Option<Vec<NodeSpec>>,
}

// The `agent` label is worded as optional bookkeeping on every write: in the M0 usage
// test a model kept telling the user which name it had logged under.

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AddArgs {
    /// Parent id, e.g. "SPOKE-1". Omit to add under the root ("To do").
    #[schemars(with = "Option<String>")]
    pub parent: Option<IdArg>,
    #[serde(flatten)]
    pub node: NodeSpec,
    /// Optional bookkeeping: a short label for which agent or session is acting (e.g. your model or terminal name). No need to mention it to the user.
    pub agent: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetArgs {
    /// Node id, e.g. "SPOKE-12". Omit for the root.
    #[schemars(with = "Option<String>")]
    pub id: Option<IdArg>,
    /// Outline levels to show: 1 (default) = direct children, 2 = grandchildren too, up to 10.
    #[schemars(with = "Option<u32>")]
    pub depth: Option<NumArg>,
    /// Include the notes body (default false).
    pub notes: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NextArgs {
    /// Only look under this node, e.g. "SPOKE-3". Omit for everything.
    #[schemars(with = "Option<String>")]
    pub scope: Option<IdArg>,
    /// Max lines (default 5).
    #[schemars(with = "Option<u32>")]
    pub limit: Option<NumArg>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SetStatusArgs {
    /// Node ids, e.g. ["SPOKE-12", "SPOKE-13"].
    #[schemars(with = "Vec<String>")]
    pub ids: Vec<IdArg>,
    /// "done" (finished), "dropped" (won't do; replaces delete) or "open" (reopen).
    pub status: SettableStatus,
    /// Optional bookkeeping: a short label for which agent or session is acting (e.g. your model or terminal name). No need to mention it to the user.
    pub agent: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UpdateArgs {
    /// Node id, e.g. "SPOKE-12".
    #[schemars(with = "String")]
    pub id: IdArg,
    /// New title.
    pub title: Option<String>,
    /// "task" or "area".
    pub kind: Option<Kind>,
    /// true if the children are steps in order.
    pub ordered: Option<bool>,
    /// 1 (highest) to 4; 0 clears it so it inherits again.
    #[schemars(with = "Option<i64>")]
    pub priority: Option<NumArg>,
    /// Deadline YYYY-MM-DD; "" clears it.
    pub due: Option<String>,
    /// Hide until YYYY-MM-DD; "" clears it.
    pub defer_until: Option<String>,
    /// Replaces all aliases; [] clears them.
    pub aliases: Option<Vec<String>>,
    /// Replaces the whole notes body; "" clears it. To add to it, use notes_append instead.
    pub notes: Option<String>,
    /// Text appended to the end of the notes (e.g. what you did, findings).
    pub notes_append: Option<String>,
    /// Optional bookkeeping: a short label for which agent or session is acting (e.g. your model or terminal name). No need to mention it to the user.
    pub agent: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MoveArgs {
    /// The node to move, e.g. "SPOKE-12".
    #[schemars(with = "String")]
    pub id: IdArg,
    /// New parent. Omit to stay under the current parent (or the parent of before/after).
    #[schemars(with = "Option<String>")]
    pub parent: Option<IdArg>,
    /// Place just before this sibling.
    #[schemars(with = "Option<String>")]
    pub before: Option<IdArg>,
    /// Place just after this sibling. Without before or after the node goes last.
    #[schemars(with = "Option<String>")]
    pub after: Option<IdArg>,
    /// Optional bookkeeping: a short label for which agent or session is acting (e.g. your model or terminal name). No need to mention it to the user.
    pub agent: Option<String>,
}
