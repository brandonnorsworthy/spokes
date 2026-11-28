# Spokes Todo MCP — product spec

Nov 13, 2026 · @brandon

## Summary

Spokes Todo MCP is one small self-hosted server holding one tree of to-dos that any MCP-capable model can read, plan into and work from. A fresh chat can ask "what should I do next?" and get a short, ranked answer. Parallel agents can each take a task without colliding.

The design decisions that shape everything else:

- **One node type.** Home, Work, a project, a step in a deploy pipeline are all the same record with a parent. Depth is unlimited because nothing in the model knows about depth.
- **A tree, plus optional links.** Each node has exactly one parent, which keeps scope, paths and "show me everything under Work" simple. Cross-links such as "blocks" come later as a separate table, which gives you the graph without making the core one.
- **Each parent says whether its children are ordered.** Ordered means steps: only the first unfinished child is actionable. Unordered means the children are independent, but their position is still a rank: within a priority level, `next` lists them in tree order (SPOKE-1080), so moving a task up ranks it higher.
- **Areas never finish; tasks do.** Home and House maintenance are areas. They hold work forever and never show up as something to complete.
- **Stable short IDs** like `SPOKE-142`, global and never reused, so any device or agent can refer to the same item.
- **Two-tier reads.** Listings return one compact line per node. The long body (plan, notes, versions, links) is fetched only when asked for, so browsing stays cheap.
- **A claim hides work in progress.** An agent that starts a task claims it; the task leaves the queue. If the agent asks you a question or goes quiet, the task returns to the top marked as needing you.
- **Nothing is deleted.** Items are completed or dropped, and an append-only event log records who did what.
- **No user accounts in v1, but every row carries a user ID** so adding auth later is not a rewrite.

## Step 0: getting it running

The first working version is one process, one SQLite file and four tools, reachable on your home network. That is an evening of work and it already answers "what should I do next?" from Claude Code.

### Stack

| Piece | Choice | Why |
| --- | --- | --- |
| Language | Rust: rmcp (the official Rust MCP SDK), axum, and rusqlite with bundled SQLite | One small static binary and a tiny container. The Rust SDK's support for the 2026-07-28 spec was in beta at release, so pin a version and check its changelog |
| Transport | Streamable HTTP on a single `/mcp` endpoint | The current remote transport; the older HTTP+SSE transport is deprecated |
| Storage | SQLite in WAL mode, with FTS5 for search | One file, in-process, sub-millisecond tree queries at personal scale, trivial backup |
| Packaging | One Docker container with a volume for the database | Runs on a NAS, a mini PC or a small VPS |

Build the server stateless: no per-connection memory, every tool call self-contained. The [2026-07-28 MCP specification](https://blog.modelcontextprotocol.io/posts/2026-07-28/) removed protocol sessions, and its migration advice is to pass any per-session state as explicit tool arguments. That is why agent identity is a plain `agent` argument in this design. Let the SDK handle protocol version negotiation with older clients.

### Connecting clients

| Client | How it reaches the server | What you need |
| --- | --- | --- |
| Claude Code | Directly from your machine | `claude mcp add --transport http spokes http://<host>:8787/mcp` |
| OpenCode and other open-model clients | Directly, added as a remote MCP server in the client's config (opencode.json for OpenCode) | The same URL on your network |
| Claude web, desktop and mobile apps | From Anthropic's cloud, not from your device | A public HTTPS URL, added under Customize > Connectors as a custom connector |

The third row is the one that changes the plan. [Claude's connector docs](https://support.claude.com/en/articles/11175166) state that custom connectors are reached from Anthropic's infrastructure, so a server that only lives on your LAN is invisible to the Claude apps. Authless servers are [supported](https://claude.com/docs/connectors/building/authentication.md), but an open, writable to-do server on the public internet is a bad idea: anyone who finds the URL can read your list and plant text that your agents will later treat as their plan.

Recommended path:

1. Start on the LAN or a private Tailscale network with no auth. Claude Code and local models work immediately.
2. When you want the Claude apps, put the server behind a tunnel (Cloudflare Tunnel or Tailscale Funnel) and require one static bearer token, which the custom connector dialog can send as a request header. This is about ten lines of code and is not a user system.
3. Real per-user auth (OAuth) waits for v2.

### The walking skeleton

1. Scaffold the server with the SDK and expose `add`, `get`, `next` and `set_status`.
2. Create the `nodes` table and seed one root node.
3. Run it in Docker, add it to Claude Code, and say "add Home, Work and Hobbies, then tell me what's next".
4. Check it with the MCP Inspector before blaming the model for anything odd.

## Core concepts

Everything is a node, and a node is a title plus a parent plus a handful of optional fields.

| Field | Meaning |
| --- | --- |
| `id` | `SPOKE-142`. A global counter, never reused, unchanged when the node moves |
| `parent` | Exactly one, except the single root ("To do") |
| `kind` | `area` (perpetual: Home, House maintenance) or `task` (can be finished) |
| `ordered` | Whether this node's children are sequential steps or an unordered set |
| `position` | Where the node sits among its siblings |
| `status` | `open`, `in_progress`, `waiting`, `done`, `dropped` |
| `priority` | Coarse level 1 to 4. Unset nodes inherit the nearest ancestor's level |
| `focus_rank` | Optional place on the short global "top priorities" list |
| `due`, `defer_until` | Deadline, and "do not show me this before" |
| `recur` | Repeat rule, for example every 2 weeks after completion |
| `aliases` | Extra phrases the node should be findable by |
| `notes` | Unlimited-length body: plan, research, versions, links. Never returned in listings |
| `claim` | Which agent holds it and when the lease expires |
| `touched_at` | Last meaningful activity on the node or anything beneath it |

### Statuses

| Status | Set by | Appears in the queue |
| --- | --- | --- |
| `open` | Default | Yes, if actionable |
| `in_progress` | `claim` | No, while the lease is live |
| `waiting` | `ask`: the agent needs a human | Yes, at the very top |
| `done` | `set_status` | No |
| `dropped` | `set_status`: the replacement for delete | No |

Areas only ever use `open` and `dropped`. That covers "infinitely in progress": House maintenance is always there, holds children, and is never itself a thing to finish.

### Two kinds of priority

You described both, and they do different jobs, so the design keeps both:

- **Level** (1 to 4) is cheap to set when capturing something: "this is important". It inherits downward, so marking a project level 1 covers its steps.
- **Focus list** is the explicit ranked list: this project is number 1, that one is number 2. It can point at a node at any depth. It stays short on purpose, around 10 entries, and `next` warns when it grows past that.

### What counts as actionable

A node is actionable when it is an open task, is not deferred, is not inside a claimed subtree, and is not waiting behind an earlier unfinished sibling in an ordered parent. A task with no children and no notes is flagged `unplanned`, which is the cue for a model to research and expand it before doing it.

## Data model

Three tables and one search index cover v1. The tree is a plain parent pointer, walked with SQLite recursive queries, which is fast well past tens of thousands of nodes.

```sql
CREATE TABLE nodes (
  id            INTEGER PRIMARY KEY AUTOINCREMENT, -- shown as SPOKE-<id>, never reused
  user_id       INTEGER NOT NULL DEFAULT 1,        -- ready for auth later
  parent_id     INTEGER REFERENCES nodes(id),      -- NULL only for the root
  position      TEXT    NOT NULL,                  -- fractional index among siblings
  kind          TEXT    NOT NULL DEFAULT 'task',   -- 'area' | 'task'
  ordered       INTEGER NOT NULL DEFAULT 0,        -- are my children sequential?
  title         TEXT    NOT NULL,
  status        TEXT    NOT NULL DEFAULT 'open',   -- open|in_progress|waiting|done|dropped
  priority      INTEGER,                           -- 1..4, NULL = inherit
  focus_rank    TEXT,                              -- fractional index on the global list
  due_at        TEXT,
  defer_until   TEXT,
  recur         TEXT,                              -- JSON, e.g. {"every":2,"unit":"week","from":"completion"}
  aliases       TEXT,                              -- newline-separated search phrases
  waiting_on    TEXT,                              -- the question for the human
  claimed_by    TEXT,
  claim_expires TEXT,
  tracked_at    TEXT,                              -- UTC instant the user's tracking began; NULL = not tracked (schema 6)
  created_at    TEXT NOT NULL,
  updated_at    TEXT NOT NULL,
  touched_at    TEXT NOT NULL,                     -- bubbles up to ancestors
  completed_at  TEXT
);
CREATE INDEX nodes_children ON nodes(parent_id, position);
CREATE INDEX nodes_queue    ON nodes(user_id, status, due_at);

CREATE TABLE notes (                               -- kept apart so listings never load bodies
  node_id INTEGER PRIMARY KEY REFERENCES nodes(id),
  body    TEXT NOT NULL
);

CREATE TABLE events (                              -- append-only history
  id      INTEGER PRIMARY KEY,
  node_id INTEGER NOT NULL REFERENCES nodes(id),
  at      TEXT NOT NULL,
  agent   TEXT,
  type    TEXT NOT NULL,                           -- created|status|claimed|asked|answered|moved|recurred...
  detail  TEXT
);

CREATE VIRTUAL TABLE node_fts USING fts5(
  title, aliases, notes, tokenize = 'trigram'
);
```

Notes on the choices:

- **Fractional positions** let an agent insert between two siblings, or reorder the focus list, by writing one row instead of renumbering many.
- **AUTOINCREMENT** is what guarantees an ID is never handed out twice, even after rows are dropped.
- **`touched_at` bubbles up.** Any change to a node updates its ancestors too, so staleness of a whole project is one column read.
- **Links wait for v1.1** as a fourth table (`from_id`, `to_id`, `type`). That is where "blocks" and "related to" live.

## MCP tool surface

Twelve tools with flat arguments. The count is deliberately small: every tool's description is loaded into every conversation, and small local models pick badly from long menus.

| Tool | What it does | Main arguments |
| --- | --- | --- |
| `next` | The queue: what needs you, what is top priority, what is due, what is stale | `scope`, `limit` (default 5) |
| `get` | One node and an outline of its children | `id`, `depth` (default 1), `notes` (default false) |
| `search` | Find nodes by keywords, or list work by state | `query`, `scope`, `include_done`, `tracked`, `state`, `claimed_by`, `untouched_days` |
| `add` | Create a node, or a whole nested plan in one call | `parent`, `title`, optional fields (`tracked`), `before` or `after`, `children[]` |
| `update` | Change fields, replace or append notes | `id`, fields (`tracked`), `notes_append` |
| `set_status` | Mark done, dropped or reopened; nothing closes over unfinished work | `ids[]`, `status` |
| `move` | Reparent or reorder | `id`, `parent`, `before` or `after` |
| `focus` | Place a node on, or remove it from, the global ranked list | `id`, `rank` or `before`/`after` |
| `claim` | Take a task; hides it from the queue | `id`, `agent`, `lease_minutes` |
| `release` | Give a task back without finishing | `id`, `agent`, `note` |
| `ask` | Hand a question to the human; task goes to the top of the queue | `id`, `agent`, `question` |
| `answer` | Record the human's reply and return the task to work | `id`, `text` |

### Output format

Responses are plain text, one line per node, not JSON. A line carries the ID, the flags that are set and the title, and nothing that is unset. Every list is capped and ends with a count of what was left out.

```
> next
NEEDS YOU
  SPOKE-88   waiting   Set up emulator: "Install to D: or E:?" (sonnet-2, 3h ago)
FOCUS
  1. SPOKE-142  p1 due Nov 20  Deploy game to production (2/6)  next: SPOKE-145 Add secrets
  2. SPOKE-201  p2             Rewire garage outlet  unplanned
DUE SOON
  SPOKE-310  due Nov 15  repeats  Check oil
STALE
  SPOKE-67   untouched 41d  Workbench build
(+14 more open. Pass scope or limit.)
```

```
> get SPOKE-142
SPOKE-142 task p1 due Nov 20 ordered (2/6) +notes  Deploy game to production
Hobbies / Programming / Skyforge
  1. SPOKE-143 done  Map the pipeline
  2. SPOKE-144 done  Write the pipeline
  3. SPOKE-145 open  Add secrets  <- next
  4. SPOKE-146 open  Push and run the deployment
  5. SPOKE-147 open  Verify in production
  6. SPOKE-148 open  Retro
```

### Rules that keep it cheap

- `+notes` tells the model a body exists; the body only comes back with `notes: true`.
- `add` accepts nested `children`, so writing a six-step plan is one call, not seven.
- Writes return one confirmation line with the new IDs, not the whole node.
- Errors say what to do next: "SPOKE-142 is claimed by opus-red until 14:20. Pick another or ask the user."
- The server's `instructions` field carries the conventions in a few lines: search before adding (and `add` itself refuses a same-title unfinished sibling and names likely duplicates), give new nodes 2 to 5 aliases, claim before working, ask rather than guess.

## Queue, claims and parallel agents

The red task disappears from the second terminal because the first agent claimed it, and it comes back to the top only when a human is needed.

### How `next` orders things

1. **Needs you.** Tasks in `waiting` with their question, then stalled tasks whose claim lease expired.
2. **Focus list**, in rank order.
3. **Due.** Overdue first, then anything due within 7 days.
4. **By level**, 1 down to 4, then tree order within a level: the depth-first walk in sibling order, the order `get` shows. Under an unordered parent, position is the rank (SPOKE-1080); when it lists two or more lines, UP NEXT ends `(Within a level, in tree order: move a task up to rank it higher.)`.
5. **Stale.** Up to three open items untouched for 30 days or more, as a nudge.

The queue lists work items, not every leaf. A node appears if it carries its own rank, level or due date; its line shows progress and the next actionable step beneath it. That keeps a six-step project to one line.

### Claim lifecycle

1. `claim(SPOKE-142, agent: "opus-red")` sets the task to `in_progress` with a lease, 60 minutes by default. The check and the write are a single SQL statement, so two agents cannot both win.
2. The claimed node and everything under it vanish from `next` for everyone else. The claim is on that node only: it neither takes over nor blocks claims above or beneath it.
3. Any write by that agent on the node or its descendants renews the lease.
4. The task ends one of four ways: `set_status done`, `release`, `ask`, or the lease running out.

| Ending | Result |
| --- | --- |
| Done | Claim cleared. If it was the last open child, the reply suggests closing the parent |
| Released | Back to `open` at its normal place in the queue, with the agent's note logged |
| Asked | `waiting`, shown under Needs you with the question |
| Lease expired | Shown under Needs you as stalled, with the last agent and time |

Expiry is evaluated when the queue is read, so there is no background job to run or to fail.

### Orchestrator and workers

The frontier model claims the project, writes the plan as children, then hands child IDs to cheaper agents. The order does not matter: a claim is refused only when another agent holds that same node, so an orchestrator can claim a project after a worker has already taken a step in it, and the reply names what others hold beneath it (SPOKE-551). Each worker claims its own step under its own `agent` label, reads the step's notes, does the work, appends what it did, and marks it done. Workers can run in parallel on the children of an unordered parent; an ordered parent releases one step at a time.

Agent labels are free text supplied by the caller. Nothing verifies them in v1, which is acceptable for one user and is the first thing auth tightens.

### Answering a question

You open any chat and ask what needs you. The model shows the question, you reply in plain language, and it calls `answer`. The text is appended to the task's notes and the task returns to `open` near the top, so the next agent to pick it up finds the answer in the body.

### Tracked nodes

A node can be marked tracked (`tracked_at`, schema 6): the user follows it, usually an epic, and the read-only viewer lists tracked nodes with their progress (`GET /api/tracked`). It is set only through MCP (`update` or `add` with `tracked: true`; `tracked: false` clears it), tracking twice keeps the first time, and it changes nothing about status, priority, focus or `next`. Lines show a `tracked` flag, `get` shows `Tracked since <date>`, and `search` takes `tracked: true` as a filter.

## Search

Search has to work when the model only half knows what you mean ("my server") and when a project has an odd spelling. Three cheap layers handle that without an embedding model, after an exact check for ids.

0. **Exact ids first.** A keyword that is a node id (`958`, `#958`, `SPOKE-958`, `spoke958`, or `spoke` followed by `958`) puts that node first, marked `(id)`, even when it is done or dropped, because every node stays reachable by id. Text matches follow it, so a number that also appears in titles or notes still finds those. A number that is no node's id is searched as text only.

1. **Aliases written at creation.** The `add` tool description tells the model to supply 2 to 5 aliases: synonyms, the plain spelling, what you would call it out loud. A project titled "Skyforj" gets `skyforge`, `sky forge`, `the game`. The model creating the node is the best synonym generator available, and it runs once.
2. **Normalization.** Titles and aliases are indexed lowercased, with accents and punctuation stripped, and also in a squashed form with no spaces, so `home-lab`, `home lab` and `homelab` all match each other.
3. **Trigram matching.** The FTS5 trigram tokenizer matches on overlapping three-letter fragments, so partial words and most typos still hit. Results are ranked title first, then aliases, then notes.
4. **Ranking by coverage of the rarer words** (0.13.0, from agent feedback SPOKE-226, SPOKE-183 and SPOKE-774: a 7-keyword query returned 68 to 158 matches). Each keyword counts by how rare it is among the matches, times where it matched; notes-only hits are capped near one title word, because feedback notes often quote whole queries; a parent named by the words gets a small lift, more when its descendants match too, since the usual job is finding where to add. By default (`match: "best"`) matches far weaker than the best are left out and counted; `any` restores plain alternatives and `all` keeps only nodes with every keyword. Partial matches say which keywords they had.

The searching model passes several keywords in one call (`server homelab nas proxmox`) and they are matched as alternatives, ranked by layer 4. Each result line includes the breadcrumb path, which is what lets the model tell "Server" under Work from "Server" under Hobbies and ask you only when it is truly ambiguous.

### Listing leftover work

`next` is a queue, so it hides what sits under a held or waiting parent, and nothing else matched on status: after agents died mid-task the only way to find their claims was to walk the tree (SPOKE-1029). `search` therefore takes three more filters, which combine with each other, with keywords and with `scope`, and which list every match when the query is left out:

| Filter | Matches |
| --- | --- |
| `state` (`status` is accepted too) | `stalled` (the lease ran out), `claimed` (live lease), `in_progress` (both), `waiting` (on the user), `open` (nobody holds it), `children_done` (nothing open at any depth beneath it, at least one direct subtask done, itself unfinished), `under_closed` (unfinished under a done or dropped node) |
| `claimed_by` | Claimed, stalled and waiting tasks held by that agent label |
| `untouched_days` | Tasks with no activity on or beneath them for that many days, deferred ones left out; oldest first without keywords |

They match unfinished tasks only, never areas. Each line ends with the holder and the expiry (or the question, or the idle time). Open work under a done or dropped node stays hidden and counted, as elsewhere in search, until `include_done: true`. Since SPOKE-345 no write on clean data can create such work; `under_closed` finds the tasks older data still holds this way. An empty answer means no task is hidden; it does not list areas, as no state filter does.

If this proves too literal in practice, v2 adds an embedding column with a SQLite vector extension. It is not worth the moving parts on day one.

## Recurrence, deadlines and staleness

All three are computed when something is read or completed, so the server needs no scheduler.

### Recurring tasks

A recurring node keeps its ID forever. Marking it done logs a `recurred` event, then reopens it with the next date and `defer_until` set so it stays out of sight until it is relevant again. The event log is the history of every oil check.

| Rule | Stored as | Next date is counted from |
| --- | --- | --- |
| Every 2 weeks after I do it (check the oil) | `{"every":2,"unit":"week","from":"completion"}` | The day it was completed |
| Every month on a fixed date (pay a bill) | `{"every":1,"unit":"month","from":"schedule"}` | The previous due date |

Intervals in days, weeks, months and years cover v1. Calendar rules such as "second Tuesday" can be added later with standard RRULE strings in the same column.

### Deadlines and time frames

- `due_at` is the deadline. A parent's due date is shown as a warning on children that have a later one, but is not copied down.
- `defer_until` hides a node from the queue until that date, which is how "not before spring" or "after the 1st" is expressed.
- The tools accept ISO dates. The model converts "next Friday" itself; the server stores your time zone once in config so "due today" means your today.

### Staleness

A node is stale when it is open, not deferred, not an area, and its `touched_at` is more than 30 days old. Because touches bubble up, a project is only stale if nothing beneath it moved either. `next` surfaces at most three, oldest first, so a fresh chat mentions them without drowning the answer. Deferring or dropping a stale item is a valid outcome, and both stop the nudge.

## Worked scenarios

Each of the situations in your brief maps to a short tool sequence.

| You say | What the model calls | Result |
| --- | --- | --- |
| "What should I work on next?" | `next` | Needs-you items, the focus list, due dates and a few stale items in about a dozen lines |
| "What's due soonest?" | `next` | The same reply already contains the due section |
| "I just had an idea, write it down" | `search` to find the right parent, then `add` with a title, two lines of notes and aliases | An `unplanned` task, findable later, with enough notes to pick the thought back up |
| "Get the game to production" | `search` "game production deploy", `get SPOKE-142`, `claim`, research, `add` with six ordered children, `update` notes | A planned, claimed project; steps handed to cheaper agents by ID |
| "Set up that game engine" on a task with no plan | `get` shows `unplanned`; the model researches, then `update` notes with versions and links and `add` subtasks | The task now has a body and children for a worker to follow |
| "Family should be its own category" | `add` under the root with `kind: area` | A new spoke, available from every device |
| "Check the oil every two weeks" | `add` with `recur` | A repeating task that reappears two weeks after each completion |
| "Make the server migration number 2" | `search`, then `focus` with `after` the current number 1 | The global list is reordered in one write |
| Second terminal: "What's next?" while red is running | `next` | Red is absent; blue is at the top |
| An agent needs a secret added by hand | `ask` | Red reappears under Needs you with the exact command or question |

## Roadmap

Five milestones reach a complete v1; each one leaves you with something usable.

1. **M0, walking skeleton.** `add`, `get`, `next`, `set_status` on SQLite, running in Docker, connected to Claude Code on the LAN. Done when a fresh chat can add three areas and list them.
2. **M1, the tree.** Ordered and unordered children, `move`, notes kept apart from listings, nested `add`, `SPOKE-` IDs, breadcrumb paths, the compact output format. Done when a model can write and walk a six-step plan in three calls.
3. **M2, finding and ranking.** `search` with aliases and trigram matching, priority levels with inheritance, `focus`, due dates, the full `next` ordering. Done when "priority tasks for my server" finds the right project from a blank context.
4. **M3, agents.** `claim`, `release`, `ask`, `answer`, leases, the event log. Done when two Claude Code terminals asking "what's next" get different tasks.
5. **M4, time and reach.** Recurrence, `defer_until`, staleness, nightly backup of the database file. Done when a restore from backup works. The tunnel, bearer token and Claude apps connector are deferred until you want them.

### After v1

- **Auth and multiple users.** OAuth on the MCP endpoint, every query filtered by the `user_id` that is already on each row, verified agent identities.
- **Links.** Blocks and related-to between any two nodes, with blocked tasks held out of the queue.
- **Notifications.** A push or message when something lands in Needs you, so a stalled agent does not wait for you to ask.
- **A read-only web view** of the tree, for the moments when you want to look rather than ask.
- **Semantic search**, only if aliases and trigrams turn out not to be enough.

## Decisions

Settled on 2026-11-14:

| Question | Decision |
| --- | --- |
| Name | Spokes Todo MCP; binary, container and connector name `spokes` |
| Language | Rust |
| Where it runs | A Docker container |
| Clients | Claude Code, OpenCode and open models, all on your own network. No public URL or token is needed for these. The Claude web and mobile apps are deferred |
| Lease length | 60 minutes for now, renewed by any write; revisit once agents are running |
| Closing parents | The server suggests closing a parent when its last child is done; it never closes one itself |
| Closing over open work | Settled 2026-11-27 (SPOKE-345): done and dropped are refused while anything beneath is unfinished, and reopening under closed work is refused, so unfinished work is never hidden. Nothing cascades; the caller names every id |
| IDs | `SPOKE-123` |
| Unknown fields on add | Settled 2026-11-27 (SPOKE-222): `add` refuses any field it does not take, at the top or on a nested child, naming the field and the child's path, and writes nothing. Chosen over honoring `status` on add, which would need its own close rules (SPOKE-345). The refusal says what to do instead where callers are known to reach for something (`status` -> `set_status done`, `position` -> `before`/`after`). Other tools still ignore unknown fields |
| Placing a node on add | Settled 2026-11-27 (SPOKE-459): `add` takes `before` or `after` (a sibling id) at the top of the call and places the new node there, like `move`, so an inserted step costs one call instead of two; `parent` may be omitted and is then the sibling's parent. Nested children are placed by their order in `children`, so `before`/`after` on a child is refused. Chosen over keeping the 0.29.2 refusal because in an ordered parent position is meaning, and a two-call insert leaves the wrong step next in between |
| Position in an unordered parent | Settled 2026-11-27 (SPOKE-1080, built in SPOKE-1081): position is a rank. `next` sorts UP NEXT by level, then tree order (the depth-first walk in sibling order), instead of oldest first, and a footer line says so. Chosen over automatic sorting everywhere because agents reorder siblings when they plan and expect it to stick; ignoring that silently is the same failure as silently ignored fields. Focus, due and needs-you sorting are unchanged |
| Duplicate siblings | Settled 2026-11-27 (SPOKE-260): an unfinished node may not share its title with an unfinished sibling. Titles compare as search normalizes them (case, accents, punctuation, spacing ignored). `add` (nested children included), `move` and `update title` are refused, naming the existing node; done and dropped siblings never block, and `set_status` is never refused for it. Checked in the write's transaction (MCP writes share one connection), not by a unique index, so there is no schema change and the rule can use the search normalization. `add` also lists up to three likely duplicates tree-wide (same title, alias match, most of the same words, or a closed same-title sibling), even with hints off. The agent label stays optional; `add` without one gets a hint asking for it |

Still open:

- [ ] **Recurring tasks.** They are the last milestone and cost one nullable column until then, so nothing depends on deciding now. If you keep them, the default is reset in place: same ID, history in the event log. If you drop them, remove `recur` and the Recurring tasks section; nothing else changes.

## Sources

- [The 2026-07-28 MCP specification](https://blog.modelcontextprotocol.io/posts/2026-07-28/): stateless core, deprecation of HTTP+SSE, SDK support
- [Get started with custom connectors using remote MCP](https://support.claude.com/en/articles/11175166): connectors are reached from Anthropic's cloud
- [Connector authentication](https://claude.com/docs/connectors/building/authentication.md): authless servers and request headers
- [Add an unlisted connector](https://claude.com/docs/connectors/custom/add-unlisted.md): static credentials via request headers
