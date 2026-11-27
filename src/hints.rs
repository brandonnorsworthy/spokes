//! Next-action hints: one short line at the end of a reply that tells the calling agent the
//! most useful next step, given the node's state (SPOKE-738).
//!
//! Every hint is made here, by [`hint`], a pure function of the state and the action, and
//! every hint line starts with [`PREFIX`] so a model, a test or a UI can find it. Rules:
//! a hint fires only when the state calls for it, one line each, never on areas or closed
//! nodes, and it says the consequence or the next step, never what the caller just did.
//! `SPOKES_HINTS=0` turns them all off ([`strip`], applied by the MCP layer).

use crate::model::{Kind, Node, Status};

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
    /// `get` showed `node`. `later_step` is true when it is a step of an ordered parent
    /// with an earlier step still unfinished, so it is not the one to start.
    Read { node: &'a Node, later_step: bool },
    /// `next` listed this many work items (focus, due and up-next lines).
    Queue { work_items: usize },
    /// `update` appended to the notes of `node`. `held_above` is true when the calling
    /// agent holds a live claim on an ancestor (an orchestrator writing a plan).
    NotesAppended { node: &'a Node, held_above: bool },
}

/// The claim hint on a task someone may be about to work on (SPOKE-1006).
pub const CLAIM_ON_READ: &str = "Working on this? Claim it first (that marks it in_progress). Finish with set_status done, or release it.";
/// The footer of `next` when it lists work.
pub const CLAIM_FROM_QUEUE: &str = "Claim the one you pick before starting.";
/// After appending notes to a task nobody holds.
pub const CLAIM_ON_NOTES: &str = "Recording progress? Claim it so other agents skip it.";

/// The hint for an action, as one line starting with [`PREFIX`], or `None` when the state
/// does not call for one.
pub fn hint(action: &Action<'_>) -> Option<String> {
    let text = match action {
        Action::StepClosed { parent, next_step } => step_closed(parent, *next_step)?,
        Action::Read { node, later_step } => {
            (unheld_task(node) && !*later_step && !has_open_children(node))
                .then(|| CLAIM_ON_READ.to_string())?
        }
        Action::Queue { work_items } => (*work_items > 0).then(|| CLAIM_FROM_QUEUE.to_string())?,
        Action::NotesAppended { node, held_above } => {
            (unheld_task(node) && !*held_above && !has_open_children(node))
                .then(|| CLAIM_ON_NOTES.to_string())?
        }
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
    let claim = if unheld_task(step) && !has_open_children(step) {
        " Claim it before starting."
    } else {
        ""
    };
    Some(format!(
        "{} {}, the next step of {} {}.{claim}",
        step.id, step.title, parent.id, parent.title
    ))
}

/// An open task that nobody holds: not claimed, not waiting on the user, not closed.
fn unheld_task(node: &Node) -> bool {
    node.kind == Kind::Task && node.status == Status::Open && node.parent_id.is_some()
}

/// Open children mean the work is in a subtask; claiming the parent is SPOKE-692's hint.
fn has_open_children(node: &Node) -> bool {
    node.child_total > node.child_done
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
    use crate::model::NodeId;

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
            Some("Next: SPOKE-4 Two, the next step of SPOKE-2 Plan. Claim it before starting.")
        );
    }

    #[test]
    fn no_claim_suffix_when_the_next_step_is_held_or_has_steps() {
        let p = parent(true, 1, 3);
        let held = Node {
            status: Status::InProgress,
            claimed_by: Some("w".into()),
            ..node(4, "Two")
        };
        let project = Node {
            child_total: 2,
            child_done: 0,
            ..node(4, "Two")
        };
        for step in [&held, &project] {
            let h = hint(&Action::StepClosed {
                parent: &p,
                next_step: Some(step),
            });
            assert_eq!(
                h.as_deref(),
                Some("Next: SPOKE-4 Two, the next step of SPOKE-2 Plan.")
            );
        }
    }

    #[test]
    fn read_hint_on_an_open_unheld_task() {
        let t = node(4, "Task");
        assert_eq!(
            hint(&Action::Read {
                node: &t,
                later_step: false
            }),
            Some(format!("{PREFIX}{CLAIM_ON_READ}"))
        );
    }

    #[test]
    fn read_hint_silent_when_held_closed_area_later_step_or_parent() {
        let base = node(4, "Task");
        let cases = [
            Node {
                status: Status::InProgress,
                ..base.clone()
            },
            Node {
                status: Status::Waiting,
                ..base.clone()
            },
            Node {
                status: Status::Done,
                ..base.clone()
            },
            Node {
                kind: Kind::Area,
                ..base.clone()
            },
            Node {
                parent_id: None,
                ..base.clone()
            },
            Node {
                child_total: 3,
                child_done: 1,
                ..base.clone()
            },
        ];
        for n in &cases {
            let action = Action::Read {
                node: n,
                later_step: false,
            };
            assert_eq!(hint(&action), None);
        }
        let action = Action::Read {
            node: &base,
            later_step: true,
        };
        assert_eq!(hint(&action), None);
        // All children finished: the task itself is the work again.
        let finished = Node {
            child_total: 2,
            child_done: 2,
            ..base.clone()
        };
        let action = Action::Read {
            node: &finished,
            later_step: false,
        };
        assert!(hint(&action).is_some());
    }

    #[test]
    fn queue_footer_only_when_work_is_listed() {
        assert_eq!(
            hint(&Action::Queue { work_items: 2 }),
            Some(format!("{PREFIX}{CLAIM_FROM_QUEUE}"))
        );
        assert_eq!(hint(&Action::Queue { work_items: 0 }), None);
    }

    #[test]
    fn notes_hint_unless_held_here_or_above() {
        let t = node(4, "Task");
        assert_eq!(
            hint(&Action::NotesAppended {
                node: &t,
                held_above: false
            }),
            Some(format!("{PREFIX}{CLAIM_ON_NOTES}"))
        );
        let action = Action::NotesAppended {
            node: &t,
            held_above: true,
        };
        assert_eq!(hint(&action), None);
        let held = Node {
            status: Status::InProgress,
            ..t.clone()
        };
        let action = Action::NotesAppended {
            node: &held,
            held_above: false,
        };
        assert_eq!(hint(&action), None);
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
