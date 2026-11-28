//! Next-action hints: one short line at the end of a reply that tells the calling agent the
//! most useful next step, given the node's state (SPOKE-738).
//!
//! Every hint is made here, by [`hint`], a pure function of the state and the action, and
//! every hint line starts with [`PREFIX`] so a model, a test or a UI can find it. Rules:
//! a hint fires only when the state calls for it, one line each, and it says the
//! consequence or the next step, never what the caller just did. Hints do not suggest
//! working on or finishing an area or a closed node.
//! `SPOKES_HINTS=0` turns them all off: the MCP layer runs each tool call inside
//! [`with_enabled`], and [`hint`] then makes none. Hints are never cut out of finished
//! reply text, so a line the user wrote that starts with "Next: " always comes back
//! (SPOKE-1110).

use std::cell::Cell;

use crate::model::{Kind, Node, NodeId, ROOT_ID, Status, id_ranges};

thread_local! {
    /// Whether [`hint`] makes hints on this thread. On unless [`with_enabled`] says off.
    static ENABLED: Cell<bool> = const { Cell::new(true) };
}

/// Runs `f` with hints on or off on this thread, and restores the previous setting
/// afterwards, even if `f` panics (the blocking pool reuses threads).
pub fn with_enabled<R>(on: bool, f: impl FnOnce() -> R) -> R {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            ENABLED.with(|e| e.set(self.0));
        }
    }
    let _restore = Restore(ENABLED.with(|e| e.replace(on)));
    f()
}

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
    /// `claim` took `parent`, a task with children (SPOKE-692). `open` are its open
    /// children in tree order (for an ordered parent, empty unless its first unfinished
    /// step is open); `held_below` is true when the claiming agent already holds
    /// a live claim on a descendant.
    ParentClaimed {
        parent: &'a Node,
        open: &'a [Node],
        held_below: bool,
    },
    /// `add` created `top`, perhaps with a plan beneath it. `unaliased` are the new tasks
    /// that have no aliases; `first_step` is the first child when `top` is ordered;
    /// `no_agent` is true when the call named no agent label (SPOKE-266).
    Added {
        top: &'a Node,
        unaliased: &'a [NodeId],
        first_step: Option<&'a Node>,
        no_agent: bool,
    },
    /// `claim` took `node`.
    Claimed { node: &'a Node },
    /// `claim` lost because another agent holds the node or something under it.
    /// `scope` is its parent task or area, when that has other work nobody holds.
    ClaimRefused { scope: Option<&'a Node> },
    /// `release` gave `node` back. `handoff` is true when a note came with it, or notes
    /// were written on it since it was claimed.
    Released { node: &'a Node, handoff: bool },
    /// `ask` left `node` waiting on the user.
    Asked { node: &'a Node },
    /// `answer` recorded the reply and reopened `node`.
    Answered { node: &'a Node },
    /// `move` changed the children of these parents (the old one and/or the new one).
    Moved { parents: &'a [Reordered<'a>] },
    /// `search` listed work by state, holder or idle time (SPOKE-1029). `stalled`: a line
    /// shown is a claim that ran out. `children_done`: it listed parents left open.
    Listed { stalled: bool, children_done: bool },
}

/// A parent whose children a move changed, read after the move.
pub struct Reordered<'a> {
    pub parent: &'a Node,
    /// Its first unfinished child before the move.
    pub step_before: Option<NodeId>,
    /// Its first unfinished child now.
    pub step_now: Option<&'a Node>,
    /// The moved node was unfinished and left this parent.
    pub lost_open: bool,
}

/// The claim hint on a task someone may be about to work on (SPOKE-1006).
pub const CLAIM_ON_READ: &str = "Working on this? Claim it first (that marks it in_progress). Finish with set_status done, or release it.";
/// The footer of `next` when it lists work.
pub const CLAIM_FROM_QUEUE: &str = "Claim the one you pick before starting.";
/// After appending notes to a task nobody holds.
pub const CLAIM_ON_NOTES: &str = "Recording progress? Claim it so other agents skip it.";
/// After a listing that shows a claim which ran out.
pub const TAKE_OVER: &str =
    "To take over a stalled task, claim it and read its notes (get notes: true).";
/// After listing parents with nothing open beneath them.
pub const CHECK_FINISHED: &str =
    "Check the work is really finished before you mark it done with set_status.";
/// After an add that named no agent label (SPOKE-266).
pub const AGENT_ON_ADD: &str =
    "Pass agent (your label, the same all session) on writes, so the creator is recorded.";
/// How many nodes a hint names before "+N more".
const LISTED: usize = 3;

/// The hint for an action, as one line starting with [`PREFIX`], or `None` when the state
/// does not call for one, or hints are off ([`with_enabled`]).
pub fn hint(action: &Action<'_>) -> Option<String> {
    if !ENABLED.with(Cell::get) {
        return None;
    }
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
        Action::ParentClaimed {
            parent,
            open,
            held_below,
        } => parent_claimed(parent, open, *held_below)?,
        Action::Added {
            top,
            unaliased,
            first_step,
            no_agent,
        } => added(top, unaliased, *first_step, *no_agent)?,
        // With notes, the claim reply already says to read them, which shows the source too.
        Action::Claimed { node } => (node.has_source && !node.has_notes).then(|| {
            format!(
                "{} has the user's own words: read them (get notes: true) before starting.",
                node.id
            )
        })?,
        Action::ClaimRefused { scope } => match scope {
            Some(p) => format!(
                "Call next with scope {} for other work under {}.",
                p.id, p.title
            ),
            None => "Call next to find other open work.".to_string(),
        },
        Action::Released { node, handoff } => (!*handoff).then(|| {
            format!(
                "No note says where {} got to. Add one with update notes_append so the next agent can pick it up.",
                node.id
            )
        })?,
        Action::Asked { node } => format!(
            "When the user replies, record it with answer on {}.",
            node.id
        ),
        Action::Answered { node } => {
            format!("Claim {} before continuing the work.", node.id)
        }
        Action::Moved { parents } => moved(parents)?,
        Action::Listed {
            stalled,
            children_done,
        } => {
            if *stalled {
                TAKE_OVER.to_string()
            } else {
                children_done.then(|| CHECK_FINISHED.to_string())?
            }
        }
    };
    Some(format!("{PREFIX}{text}"))
}

/// A move matters when it changes which step of an ordered task comes next, or takes the
/// last unfinished child away from a task.
fn moved(parents: &[Reordered<'_>]) -> Option<String> {
    let parts: Vec<String> = parents
        .iter()
        .filter_map(|r| {
            let p = r.parent;
            if p.kind != Kind::Task || p.status.is_closed() {
                return None;
            }
            if r.lost_open && all_done(p) {
                return Some(all_steps_done(p));
            }
            let step = r.step_now.filter(|_| p.ordered)?;
            (r.step_before != Some(step.id)).then(|| {
                format!(
                    "The next step of {} {} is now {} {}.",
                    p.id, p.title, step.id, step.title
                )
            })
        })
        .collect();
    (!parts.is_empty()).then(|| parts.join(" "))
}

fn all_done(parent: &Node) -> bool {
    parent.child_total > 0 && parent.child_done == parent.child_total
}

fn all_steps_done(parent: &Node) -> String {
    format!(
        "All steps of {} {} are done. If it is finished too, mark it done with set_status.",
        parent.id, parent.title
    )
}

fn step_closed(parent: &Node, next_step: Option<&Node>) -> Option<String> {
    if parent.kind != Kind::Task || parent.status.is_closed() {
        return None;
    }
    if all_done(parent) {
        return Some(all_steps_done(parent));
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

/// Claiming a parent: the work is usually in one subtask, which should be claimed too. A
/// claim inside the caller's own claim is allowed, so this never leads to a refusal.
fn parent_claimed(parent: &Node, open: &[Node], held_below: bool) -> Option<String> {
    if held_below || open.is_empty() || parent.kind != Kind::Task {
        return None;
    }
    let n = open.len();
    let subtasks = if n == 1 { "subtask" } else { "subtasks" };
    let which = if parent.ordered {
        // The caller passes nothing when the first unfinished step is not open, so the
        // first open child is the next step.
        let first = &open[0];
        format!("the next step is {} {}", first.id, first.title)
    } else {
        names(open)
    };
    Some(format!(
        "{} has {n} open {subtasks} ({which}). Working on one? Claim that one too. Orchestrating? Keep this claim; workers can claim their own subtasks inside it.",
        parent.id
    ))
}

/// A new node's loose ends, in one line: which step starts the plan, a task left under
/// the root, and tasks search will have a hard time finding.
fn added(
    top: &Node,
    unaliased: &[NodeId],
    first_step: Option<&Node>,
    no_agent: bool,
) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(step) = first_step.filter(|_| top.kind == Kind::Task) {
        parts.push(format!("The first step is {} {}.", step.id, step.title));
    }
    if top.kind == Kind::Task && top.parent_id == Some(ROOT_ID) {
        parts.push(format!(
            "{} is directly under the root; move it if it belongs in an area.",
            top.id
        ));
    }
    if !unaliased.is_empty() {
        let them = if unaliased.len() == 1 { "it" } else { "them" };
        parts.push(format!(
            "No aliases on {}: add 2 to 5 each with update so search finds {them}.",
            id_ranges(unaliased)
        ));
    }
    if no_agent {
        parts.push(AGENT_ON_ADD.to_string());
    }
    (!parts.is_empty()).then(|| parts.join(" "))
}

/// `SPOKE-4 Two, SPOKE-5 Three, SPOKE-6 Four, +2 more`.
fn names(nodes: &[Node]) -> String {
    let mut names: Vec<String> = nodes
        .iter()
        .take(LISTED)
        .map(|c| format!("{} {}", c.id, c.title))
        .collect();
    if nodes.len() > LISTED {
        names.push(format!("+{} more", nodes.len() - LISTED));
    }
    names.join(", ")
}

/// An open task that nobody holds: not claimed, not waiting on the user, not closed.
fn unheld_task(node: &Node) -> bool {
    node.kind == Kind::Task && node.status == Status::Open && node.parent_id.is_some()
}

/// Open children mean the work is in a subtask; claiming the parent is SPOKE-692's hint.
fn has_open_children(node: &Node) -> bool {
    node.child_total > node.child_done
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn parent_claim_names_the_next_step_of_an_ordered_parent() {
        let p = parent(true, 1, 3);
        let open = [node(4, "Two"), node(5, "Three")];
        let h = hint(&Action::ParentClaimed {
            parent: &p,
            open: &open,
            held_below: false,
        });
        assert_eq!(
            h.as_deref(),
            Some(
                "Next: SPOKE-2 has 2 open subtasks (the next step is SPOKE-4 Two). Working on one? Claim that one too. Orchestrating? Keep this claim; workers can claim their own subtasks inside it."
            )
        );
    }

    #[test]
    fn parent_claim_lists_up_to_three_open_children() {
        let p = parent(false, 0, 5);
        let open: Vec<Node> = (4..9).map(|i| node(i, &format!("C{i}"))).collect();
        let h = hint(&Action::ParentClaimed {
            parent: &p,
            open: &open,
            held_below: false,
        })
        .unwrap();
        assert!(
            h.starts_with(
                "Next: SPOKE-2 has 5 open subtasks (SPOKE-4 C4, SPOKE-5 C5, SPOKE-6 C6, +2 more). "
            ),
            "{h}"
        );
        let one = hint(&Action::ParentClaimed {
            parent: &p,
            open: &open[..1],
            held_below: false,
        })
        .unwrap();
        assert!(
            one.starts_with("Next: SPOKE-2 has 1 open subtask (SPOKE-4 C4). "),
            "{one}"
        );
    }

    #[test]
    fn parent_claim_silent_without_open_children_or_when_already_holding_one() {
        let p = parent(false, 0, 2);
        let open = [node(4, "Two")];
        for (open, held_below) in [(&open[..], true), (&[][..], false)] {
            let action = Action::ParentClaimed {
                parent: &p,
                open,
                held_below,
            };
            assert_eq!(hint(&action), None);
        }
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
    fn add_names_the_first_step_the_root_and_missing_aliases() {
        let top = Node {
            ordered: true,
            ..node(2, "Plan")
        };
        let step = node(3, "One");
        let bare = [NodeId(2), NodeId(3), NodeId(4)];
        let h = hint(&Action::Added {
            top: &top,
            unaliased: &bare,
            first_step: Some(&step),
            no_agent: false,
        });
        assert_eq!(
            h.as_deref(),
            Some(
                "Next: The first step is SPOKE-3 One. SPOKE-2 is directly under the root; move it if it belongs in an area. No aliases on SPOKE-2..SPOKE-4: add 2 to 5 each with update so search finds them."
            )
        );
        let placed = Node {
            parent_id: Some(NodeId(9)),
            ..node(10, "Task")
        };
        let action = Action::Added {
            top: &placed,
            unaliased: &[],
            first_step: None,
            no_agent: false,
        };
        assert_eq!(hint(&action), None);
        let area = Node {
            kind: Kind::Area,
            ..node(2, "Home")
        };
        let action = Action::Added {
            top: &area,
            unaliased: &[],
            first_step: None,
            no_agent: false,
        };
        assert_eq!(hint(&action), None);
        let action = Action::Added {
            top: &placed,
            unaliased: &[],
            first_step: None,
            no_agent: true,
        };
        assert_eq!(
            hint(&action).as_deref(),
            Some(
                "Next: Pass agent (your label, the same all session) on writes, so the creator is recorded."
            )
        );
    }

    #[test]
    fn claim_points_at_source_only_when_the_reply_does_not() {
        let spoken = Node {
            has_source: true,
            ..node(4, "Idea")
        };
        assert_eq!(
            hint(&Action::Claimed { node: &spoken }).as_deref(),
            Some(
                "Next: SPOKE-4 has the user's own words: read them (get notes: true) before starting."
            )
        );
        let noted = Node {
            has_notes: true,
            ..spoken
        };
        for n in [&noted, &node(5, "Plain")] {
            assert_eq!(hint(&Action::Claimed { node: n }), None);
        }
    }

    #[test]
    fn claim_refusal_says_where_other_work_is() {
        let p = parent(false, 0, 2);
        assert_eq!(
            hint(&Action::ClaimRefused { scope: Some(&p) }).as_deref(),
            Some("Next: Call next with scope SPOKE-2 for other work under Plan.")
        );
        assert_eq!(
            hint(&Action::ClaimRefused { scope: None }).as_deref(),
            Some("Next: Call next to find other open work.")
        );
    }

    #[test]
    fn release_asks_for_a_handoff_only_without_one() {
        let t = node(4, "Task");
        let h = hint(&Action::Released {
            node: &t,
            handoff: false,
        });
        assert_eq!(
            h.as_deref(),
            Some(
                "Next: No note says where SPOKE-4 got to. Add one with update notes_append so the next agent can pick it up."
            )
        );
        let action = Action::Released {
            node: &t,
            handoff: true,
        };
        assert_eq!(hint(&action), None);
    }

    #[test]
    fn ask_says_how_to_record_the_reply() {
        let t = node(4, "Task");
        assert_eq!(
            hint(&Action::Asked { node: &t }).as_deref(),
            Some("Next: When the user replies, record it with answer on SPOKE-4.")
        );
    }

    #[test]
    fn answer_says_to_claim_before_continuing() {
        let t = node(4, "Task");
        assert_eq!(
            hint(&Action::Answered { node: &t }).as_deref(),
            Some("Next: Claim SPOKE-4 before continuing the work.")
        );
    }

    #[test]
    fn move_names_a_new_next_step_and_a_parent_left_finished() {
        let ordered = parent(true, 0, 2);
        let step = node(5, "Five");
        let emptied = Node {
            id: NodeId(3),
            ..parent(false, 1, 1)
        };
        let both = [
            Reordered {
                parent: &emptied,
                step_before: None,
                step_now: None,
                lost_open: true,
            },
            Reordered {
                parent: &ordered,
                step_before: Some(NodeId(4)),
                step_now: Some(&step),
                lost_open: false,
            },
        ];
        assert_eq!(
            hint(&Action::Moved { parents: &both }).as_deref(),
            Some(
                "Next: All steps of SPOKE-3 Plan are done. If it is finished too, mark it done with set_status. The next step of SPOKE-2 Plan is now SPOKE-5 Five."
            )
        );
    }

    #[test]
    fn move_is_silent_when_nothing_to_act_on_changed() {
        let ordered = parent(true, 0, 2);
        let unordered = parent(false, 0, 2);
        let finished = parent(false, 1, 1);
        let area = Node {
            kind: Kind::Area,
            ..parent(true, 0, 2)
        };
        let step = node(5, "Five");
        let cases = [
            // Same next step as before.
            (&ordered, Some(NodeId(5)), false),
            // Order means nothing here.
            (&unordered, Some(NodeId(4)), false),
            (&area, Some(NodeId(4)), false),
            // It was already finished: the move took nothing unfinished away.
            (&finished, None, false),
        ];
        for (parent, step_before, lost_open) in cases {
            let one = [Reordered {
                parent,
                step_before,
                step_now: Some(&step),
                lost_open,
            }];
            assert_eq!(hint(&Action::Moved { parents: &one }), None);
        }
    }

    #[test]
    fn off_makes_no_hint_and_restores_the_setting() {
        let asked = node(4, "Ask");
        let action = Action::Asked { node: &asked };
        assert!(hint(&action).is_some());
        assert_eq!(with_enabled(false, || hint(&action)), None);
        // Nested calls restore the outer setting, and a panic restores it too.
        with_enabled(false, || {
            assert!(with_enabled(true, || hint(&action)).is_some());
            assert_eq!(hint(&action), None);
        });
        let _ = std::panic::catch_unwind(|| with_enabled(false, || panic!("mid-call")));
        assert!(hint(&action).is_some());
    }
}
