//! Next-action hints: one short line at the end of a reply that tells the calling agent the
//! most useful next step, given the node's state (SPOKE-738).
//!
//! Every hint is made here, by [`hint`], a pure function of the state and the action, and
//! every hint line starts with [`PREFIX`] so a model, a test or a UI can find it. Rules:
//! a hint fires only when the state calls for it, one line each, never on areas or closed
//! nodes, and it says the consequence or the next step, never what the caller just did.
//! `SPOKES_HINTS=0` turns them all off ([`strip`], applied by the MCP layer).

use crate::model::{Kind, Node};

/// Every hint line starts with this.
pub const PREFIX: &str = "Next: ";

/// What just happened, with the state a hint needs. The caller loads the state; this
/// module only decides.
pub enum Action<'a> {
    /// A child of `parent` was closed (done or dropped). `next_step` is the parent's first
    /// unfinished child, if any.
    StepClosed {
        parent: &'a Node,
        next_step: Option<&'a Node>,
    },
}

/// The hint for an action, as one line starting with [`PREFIX`], or `None` when the state
/// does not call for one.
pub fn hint(action: &Action<'_>) -> Option<String> {
    let text = match action {
        Action::StepClosed { parent, next_step } => step_closed(parent, *next_step)?,
    };
    Some(format!("{PREFIX}{text}"))
}

fn step_closed(parent: &Node, next_step: Option<&Node>) -> Option<String> {
    if parent.kind != Kind::Task || parent.status.is_closed() {
        return None;
    }
    if parent.child_total > 0 && parent.child_done == parent.child_total {
        return Some(format!(
            "All steps of {} {} are done. If it is finished too, mark it done with set_status.",
            parent.id, parent.title
        ));
    }
    let step = next_step.filter(|_| parent.ordered)?;
    Some(format!(
        "{} {}, the next step of {} {}.",
        step.id, step.title, parent.id, parent.title
    ))
}

/// Whether a reply line is a hint.
pub fn is_hint(line: &str) -> bool {
    line.starts_with(PREFIX)
}

/// The reply with its hint lines removed, for `SPOKES_HINTS=0`.
pub fn strip(text: &str) -> String {
    text.lines()
        .filter(|l| !is_hint(l))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{NodeId, Status};

    fn node(id: i64, title: &str) -> Node {
        Node {
            id: NodeId(id),
            parent_id: Some(NodeId(1)),
            position: "V".into(),
            kind: Kind::Task,
            ordered: false,
            title: title.to_string(),
            status: Status::Open,
            priority: None,
            focus_rank: None,
            due_at: None,
            defer_until: None,
            waiting_on: None,
            claimed_by: None,
            claim_expires: None,
            answered_at: None,
            recur: None,
            work_type: None,
            refinement: None,
            tracked_at: None,
            created_at: "2026-11-01T00:00:00Z".into(),
            updated_at: "2026-11-01T00:00:00Z".into(),
            touched_at: "2026-11-01T00:00:00Z".into(),
            has_notes: false,
            has_source: false,
            is_step: false,
            child_total: 0,
            child_done: 0,
        }
    }

    fn parent(ordered: bool, done: i64, total: i64) -> Node {
        Node {
            ordered,
            child_done: done,
            child_total: total,
            ..node(2, "Plan")
        }
    }

    #[test]
    fn names_the_next_step_of_an_ordered_parent() {
        let p = parent(true, 1, 2);
        let step = node(4, "Two");
        let h = hint(&Action::StepClosed {
            parent: &p,
            next_step: Some(&step),
        });
        assert_eq!(
            h.as_deref(),
            Some("Next: SPOKE-4 Two, the next step of SPOKE-2 Plan.")
        );
    }

    #[test]
    fn says_all_steps_are_done() {
        let p = parent(false, 2, 2);
        let h = hint(&Action::StepClosed {
            parent: &p,
            next_step: None,
        });
        assert_eq!(
            h.as_deref(),
            Some(
                "Next: All steps of SPOKE-2 Plan are done. If it is finished too, mark it done with set_status."
            )
        );
    }

    #[test]
    fn silent_for_unordered_parents_with_work_left() {
        let p = parent(false, 1, 2);
        let step = node(4, "Two");
        let action = Action::StepClosed {
            parent: &p,
            next_step: Some(&step),
        };
        assert_eq!(hint(&action), None);
    }

    #[test]
    fn silent_for_areas_and_closed_parents() {
        let area = Node {
            kind: Kind::Area,
            ..parent(true, 2, 2)
        };
        let closed = Node {
            status: Status::Done,
            ..parent(true, 2, 2)
        };
        for p in [&area, &closed] {
            let action = Action::StepClosed {
                parent: p,
                next_step: None,
            };
            assert_eq!(hint(&action), None);
        }
    }

    #[test]
    fn strip_removes_only_hint_lines() {
        let reply = "SPOKE-3 done  One\nNext: SPOKE-4 Two, the next step of SPOKE-2 Plan.\nSPOKE-5 done  Other";
        assert_eq!(strip(reply), "SPOKE-3 done  One\nSPOKE-5 done  Other");
        assert_eq!(strip("no hints here"), "no hints here");
    }
}
