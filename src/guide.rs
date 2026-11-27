//! The text behind the `guide` tool: every state a node can be in, in one short page.
//! The connect-time instructions stay a few lines and point here ("call guide if unsure").

pub const GUIDE: &str = "\
SPOKES GUIDE: what every field and state means

THREE QUESTIONS, THREE FIELDS (they are independent; changing one never changes another)
  status      Is it finished?                 open, in_progress, waiting, done, dropped
  refinement  Can an agent start it yet?      idea, brainstormed, understood, planned, ready
  type        What kind of work is it?        bug, feature, chore, idea, epic, spike
refinement and type are optional labels. Leave them unset unless the user gave one or you know it.

STATUS (what happened to the work)
  open         nobody holds it; it can show up in next.
  in_progress  an agent claimed it (claim). Set by claim, not by hand.
  waiting      an agent asked the user a question (ask). Set by ask; answer reopens it.
  done         finished. set_status done. A repeating task comes back with its next due date.
  dropped      won't do. set_status dropped. There is no delete.
  Areas (kind area) never finish: they can be dropped but never done.

REFINEMENT (how ready the work is to pick up; it stops at ready)
  idea          a rough thought; nothing is decided.
  brainstormed  options were explored; no choice yet.
  understood    what is wanted is clear and played back to the user.
  planned       a plan exists: ordered steps with a done-when for each.
  ready         an agent can claim it now and start.
  Once work starts, status carries it (claim -> in_progress -> done). Do not put started work back in refinement.
  Done and dropped nodes show none and match no refinement filter; the stored value returns on reopen.

TYPE (what kind of work)
  bug       something is broken.   feature  something new.   chore  upkeep, no behaviour change.
  idea      a thought to keep.     epic     a big goal; its children are the parts.
  spike     research that ends in a written answer in notes. Spikes under one parent are independent,
            so they can run in parallel. Finish one by notes_append of the findings, then set_status done;
            if it implies work, add that work as new nodes.

TRACKED (what the user follows)
  The user follows some nodes, usually an epic, and spokes-view lists the tracked ones with their progress.
  update tracked: true when the user asks to track a node; tracked: false stops it. search tracked: true
  lists them. It changes nothing about status, priority, focus or next.

STRUCTURE
  Everything is one tree; each node has one parent. A parent with ordered: true holds steps and only the
  first unfinished step is actionable (marked <- next). A project is simply a task with children.

HOW AN AGENT WORKS A TASK
  next -> claim (use one agent label all session) -> do it, notes_append as you go -> set_status done.
  Stuck or need a decision: ask (it waits for the user), do not guess. Giving up: release with a note.
  Adding: search first, then add with 2 to 5 aliases; pass the user's own words in source.

SEARCHING
  Several keywords at once, as alternatives. A typed id (958, #958, SPOKE-958) puts that node first,
  even if it is done or dropped. Nodes matching more of the rarer words rank first; by default
  (match: \"best\") much weaker matches are left out and counted. match: \"any\" lists every node with
  any keyword, match: \"all\" only nodes with every keyword. \"(matched 2 of 3: a, b)\" says which words a
  partial match had. \"+N more ... Pass offset: 10\" gives the next page. Paths show the last 3
  ancestors with ids (\"… / Spokes (SPOKE-5) / agent-feedback (SPOKE-182)\"); full_paths: true shows all.

READING A LINE
  +notes has a body (get notes: true). +source has the user's original words. (2/5) means two of five
  children are done. p1..p4 is priority; children inherit it. \"tracked\" means the user follows it.";

#[cfg(test)]
mod tests {
    use super::GUIDE;
    use crate::model::{Refinement, WorkType};

    /// The guide must name every refinement, type and status the server accepts, so a new
    /// value cannot ship without being explained.
    #[test]
    fn names_every_value_the_server_accepts() {
        for r in Refinement::ALL {
            assert!(GUIDE.contains(r.as_str()), "guide misses refinement {r}");
        }
        for t in WorkType::ALL {
            assert!(GUIDE.contains(t.as_str()), "guide misses type {t}");
        }
        for s in ["open", "in_progress", "waiting", "done", "dropped"] {
            assert!(GUIDE.contains(s), "guide misses status {s}");
        }
    }
}
