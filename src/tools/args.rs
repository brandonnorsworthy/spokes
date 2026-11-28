//! Tool arguments. Doc comments become the JSON Schema descriptions models read,
//! so they stay short. IDs are declared as strings but bare numbers are accepted too;
//! numbers are declared as integers but numeric strings ("5") are accepted too.

use schemars::JsonSchema;
use serde::Deserialize;

use crate::model::{IdArg, Kind, NumArg, SettableStatus};
use crate::recur::RecurArg;

/// One node to create, with optional nested children (a whole plan in one call).
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct NodeSpec {
    /// Short title. At the top of the call, leave it out and pass children to add them straight under an existing parent (several nodes in one call).
    #[serde(default)]
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
    /// Repeat rule for a task, e.g. {"every": 2, "unit": "week", "from": "completion"} or "every 2 weeks". from "schedule" counts from the due date (bills). Marking it done resets it with the next due date.
    pub recur: Option<RecurArg>,
    /// What sort of work: "bug", "feature", "chore", "idea", "epic" or "spike" (research that ends in a written answer in notes).
    #[serde(rename = "type")]
    pub work_type: Option<String>,
    /// How ready it is for an agent: "idea", "brainstormed", "understood", "planned" or "ready" (an agent can claim it now). Separate from status; execution is tracked by status, not here.
    pub refinement: Option<String>,
    /// true to track it: the user follows this node (usually an epic) and spokes-view lists it with its progress. Changes nothing about status, priority, focus or next.
    pub tracked: Option<bool>,
    /// Longer body: plan, research, links. Never shown in listings.
    pub notes: Option<String>,
    /// The user's own words that led to this node, pasted exactly as typed (no summary, no edits). Kept apart from notes so the original idea can always be recovered. Set once; it can only be appended to later.
    pub source: Option<String>,
    /// Nodes to create beneath this one, in order. Each takes the same fields (title, kind, ordered, notes, children...).
    pub children: Option<Vec<NodeSpec>>,
    /// Renamed to `refinement`; taken only so that it can be refused instead of ignored.
    #[schemars(skip)]
    pub stage: Option<serde_json::Value>,
    /// Every other field given, so that `add` refuses it by name instead of dropping it
    /// without a word (SPOKE-222).
    #[serde(flatten)]
    #[schemars(skip)]
    pub unknown: std::collections::BTreeMap<String, serde_json::Value>,
}

// The `agent` label is optional on plain writes (it logs who acted and renews that agent's
// claim) and says "no need to mention it": in the M0 usage test a model kept telling the
// user which name it had logged under.

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AddArgs {
    /// Parent id, e.g. "SPOKE-1". Omit to add under the root ("To do").
    #[schemars(with = "Option<String>")]
    pub parent: Option<IdArg>,
    #[serde(flatten)]
    pub node: NodeSpec,
    /// Place the new node just before this sibling, e.g. "SPOKE-7" (parent may then be omitted). Without before or after it goes last.
    #[schemars(with = "Option<String>")]
    pub before: Option<IdArg>,
    /// Place the new node just after this sibling, e.g. "SPOKE-6" (parent may then be omitted).
    #[schemars(with = "Option<String>")]
    pub after: Option<IdArg>,
    /// Your agent label, the same all session (e.g. "opus-red"). Optional; it logs who acted and renews your claim. No need to mention it to the user.
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
    /// Include the node's last 10 history events: who claimed, asked, answered, changed it (default false).
    pub history: Option<bool>,
    /// Show every ancestor in the path (default: the last 3, each with its id, as search shows them).
    pub full_paths: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NextArgs {
    /// Only look under this node, e.g. "SPOKE-3". Omit for everything.
    #[schemars(with = "Option<String>")]
    pub scope: Option<IdArg>,
    /// Max lines (default 5), not counting up to 3 stale nudges.
    #[schemars(with = "Option<u32>")]
    pub limit: Option<NumArg>,
    /// Only nodes of this type (bug, feature, chore, idea, epic, spike).
    #[serde(rename = "type")]
    pub work_type: Option<String>,
    /// Only nodes at this refinement (e.g. ready).
    pub refinement: Option<String>,
    /// Renamed to `refinement`; taken only so that it can be refused instead of ignored.
    #[schemars(skip)]
    pub stage: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SetStatusArgs {
    /// Node ids, e.g. ["SPOKE-12", "SPOKE-13"].
    #[schemars(with = "Vec<String>")]
    pub ids: Vec<IdArg>,
    /// "done" (finished), "dropped" (won't do; replaces delete) or "open" (reopen).
    pub status: SettableStatus,
    /// Your agent label, the same all session (e.g. "opus-red"). Optional; it logs who acted and renews your claim. No need to mention it to the user.
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
    /// Repeat rule, e.g. {"every": 1, "unit": "month", "from": "schedule"} or "every 2 weeks"; "" stops it repeating.
    pub recur: Option<RecurArg>,
    /// What sort of work: "bug", "feature", "chore", "idea", "epic" or "spike" (research that ends in a written answer in notes). "" clears it.
    #[serde(rename = "type")]
    pub work_type: Option<String>,
    /// How ready it is for an agent: "idea", "brainstormed", "understood", "planned" or "ready" (an agent can claim it now). Separate from status; execution is tracked by status, not here. "" clears it.
    pub refinement: Option<String>,
    /// true tracks it (the user follows it; spokes-view lists it with its progress), false stops tracking. Changes nothing about status, priority, focus or next.
    pub tracked: Option<bool>,
    /// Replaces the whole notes body; "" clears it. To add to it, use notes_append instead.
    pub notes: Option<String>,
    /// Text appended to the end of the notes (e.g. what you did, findings).
    pub notes_append: Option<String>,
    /// Text added to the end of the source: a later message from the user, pasted exactly as typed.
    pub source_append: Option<String>,
    /// Not accepted: the source can never be replaced or cleared. Use source_append.
    pub source: Option<String>,
    /// Your agent label, the same all session (e.g. "opus-red"). Optional; it logs who acted and renews your claim. No need to mention it to the user.
    pub agent: Option<String>,
    /// Renamed to `refinement`; taken only so that it can be refused instead of ignored.
    #[schemars(skip)]
    pub stage: Option<serde_json::Value>,
    /// Not taken: children are added with `add`. Taken only so that it can be refused
    /// instead of ignored (SPOKE-228).
    #[schemars(skip)]
    pub children: Option<serde_json::Value>,
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
    /// Your agent label, the same all session (e.g. "opus-red"). Optional; it logs who acted and renews your claim. No need to mention it to the user.
    pub agent: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchArgs {
    /// Several keywords at once, matched as alternatives: names, synonyms, likely misspellings, e.g. "server homelab nas proxmox". Leave it out to list every match of a filter.
    #[serde(default)]
    pub query: String,
    /// Only search under this node, e.g. "SPOKE-3". Omit for everything.
    #[schemars(with = "Option<String>")]
    pub scope: Option<IdArg>,
    /// Also find done and dropped nodes (default false).
    pub include_done: Option<bool>,
    /// Max results (default 10).
    #[schemars(with = "Option<u32>")]
    pub limit: Option<NumArg>,
    /// Skip this many results, for the next page (default 0); the reply says which offset comes next.
    #[schemars(with = "Option<u32>")]
    pub offset: Option<NumArg>,
    /// Only nodes of this type (bug, feature, chore, idea, epic, spike).
    #[serde(rename = "type")]
    pub work_type: Option<String>,
    /// Only nodes at this refinement (e.g. ready).
    pub refinement: Option<String>,
    /// true = only tracked nodes (with an empty query, every tracked node).
    pub tracked: Option<bool>,
    /// Only unfinished tasks in this state: "stalled" (the claim ran out), "claimed" (live claim), "in_progress" (both), "waiting" (on the user), "open" (nobody holds it), "children_done" (nothing open beneath it, a subtask done, itself unfinished) or "under_closed" (unfinished under a done or dropped task).
    #[serde(alias = "status")]
    pub state: Option<String>,
    /// Only tasks this agent label holds: claimed, stalled or waiting.
    pub claimed_by: Option<String>,
    /// Only unfinished, not deferred tasks with no activity on or beneath them for at least this many days.
    #[schemars(with = "Option<u32>")]
    pub untouched_days: Option<NumArg>,
    /// Show every ancestor in each result's path (default: the last 3, with ids).
    pub full_paths: Option<bool>,
    /// How keywords combine: "best" (default: any keyword, much weaker matches left out), "any" (every node matching any keyword) or "all" (only nodes matching every keyword).
    #[serde(rename = "match")]
    pub match_mode: Option<String>,
    /// Renamed to `refinement`; taken only so that it can be refused instead of ignored.
    #[schemars(skip)]
    pub stage: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RecentArgs {
    /// Only look under this node, e.g. "SPOKE-3". Omit for everything.
    #[schemars(with = "Option<String>")]
    pub scope: Option<IdArg>,
    /// Also list done and dropped nodes (default false).
    pub include_done: Option<bool>,
    /// Max results (default 10, up to 50).
    #[schemars(with = "Option<u32>")]
    pub limit: Option<NumArg>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FocusArgs {
    /// The node to place on (or take off) the focus list, at any depth, e.g. "SPOKE-12".
    #[schemars(with = "String")]
    pub id: IdArg,
    /// Place it at this position, 1 = top. Without rank, before or after it goes last.
    #[schemars(with = "Option<u32>")]
    pub rank: Option<NumArg>,
    /// Place it just above this focus-list entry.
    #[schemars(with = "Option<String>")]
    pub before: Option<IdArg>,
    /// Place it just below this focus-list entry.
    #[schemars(with = "Option<String>")]
    pub after: Option<IdArg>,
    /// true takes it off the focus list (its priority level stays).
    pub remove: Option<bool>,
    /// Your agent label, the same all session (e.g. "opus-red"). Optional; it logs who acted and renews your claim. No need to mention it to the user.
    pub agent: Option<String>,
}

// Claims, questions and answers. Here `agent` is required: it is who holds the task.

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ClaimArgs {
    /// The task to take, e.g. "SPOKE-12".
    #[schemars(with = "String")]
    pub id: IdArg,
    /// Your agent label, the same one for the whole session (e.g. "opus-red").
    pub agent: String,
    /// How long the claim lasts without activity (default 60, max 1440). Any write you make on the task or beneath it renews it.
    #[schemars(with = "Option<u32>")]
    pub lease_minutes: Option<NumArg>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReleaseArgs {
    /// The task to give back, e.g. "SPOKE-12".
    #[schemars(with = "String")]
    pub id: IdArg,
    /// Your agent label (the one you claimed with).
    pub agent: String,
    /// Why, and where you got to. Appended to the task's notes for the next agent.
    pub note: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AskArgs {
    /// The task the question is about, e.g. "SPOKE-12".
    #[schemars(with = "String")]
    pub id: IdArg,
    /// Your agent label.
    pub agent: String,
    /// The question for the user, short and specific (e.g. "Install to D: or E:?").
    pub question: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AnswerArgs {
    /// The waiting task, e.g. "SPOKE-12".
    #[schemars(with = "String")]
    pub id: IdArg,
    /// The user's reply, in their words. Appended to the task's notes with the question.
    pub text: String,
}

/// The guide takes no arguments.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct GuideArgs {}
