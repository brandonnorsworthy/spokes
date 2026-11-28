# Spokes Todo MCP

[![ci](https://github.com/brandonnorsworthy/spokes/actions/workflows/ci.yml/badge.svg)](https://github.com/brandonnorsworthy/spokes/actions/workflows/ci.yml) [![codeql](https://github.com/brandonnorsworthy/spokes/actions/workflows/codeql.yml/badge.svg)](https://github.com/brandonnorsworthy/spokes/actions/workflows/codeql.yml)

One small self-hosted server that holds one tree of to-dos, which any MCP-capable model can read, plan into and work from. A fresh chat can ask "what should I do next?" and get a short, ranked answer. Parallel agents can each take a task without colliding.

The full product spec is [docs/spec.md](docs/spec.md).

## End goal

The finished v1 is for one person (Brandon) and the models and agents working for him: Claude Code, OpenCode and open models on the home network.

- **One node type in one tree.** Home, Work, a project and a step in a deploy pipeline are all the same record with a parent. Depth is unlimited.
- **Areas never finish; tasks do.** Areas (Home, House maintenance) hold work forever. Tasks get done or dropped. Nothing is deleted; an append-only event log records who did what.
- **Ordered or unordered children.** Ordered parents release one step at a time.
- **Stable short IDs** like `SPOKE-142`, global and never reused.
- **Two-tier reads.** Listings are one compact plain-text line per node. Long notes come back only when asked for.
- **A ranked queue (`next`)**: what needs you (questions, stalled claims), the focus list, what is due, priority levels, and a few stale items.
- **Search** that copes with half-remembered names: aliases written at creation, normalization, trigram matching.
- **Claims for parallel agents.** A claimed task leaves the queue; `ask` brings it back to the top as needing you.
- **Time:** due dates, `defer_until`, recurring tasks reset in place, staleness nudges, nightly backup of the database file.
- **Fourteen tools:** `next`, `get`, `search`, `recent`, `add`, `update`, `set_status`, `move`, `focus`, `claim`, `release`, `ask`, `answer`.

## Phases

| Phase | Scope | Status |
| --- | --- | --- |
| **M0, walking skeleton** | `add`, `get`, `next`, `set_status` on SQLite (WAL), stateless Streamable HTTP on `/mcp`, Docker image. Done when a fresh chat can add three areas and list them | **Done** |
| **M1, the tree** | Ordered/unordered children, `move`, `update`, notes apart from listings, nested `add`, breadcrumb paths, the full compact output format. Done when a model can write and walk a six-step plan in three calls | **Done** |
| **M2, finding and ranking** | `search` (aliases, normalization, trigram FTS5), priority inheritance, `focus`, due dates with `SPOKES_TZ`, the full `next` ordering. Done when "priority tasks for my server" finds the right project from a blank context | **Done** |
| **M3, agents** | `claim`, `release`, `ask`, `answer`, leases, event log surfaced. Done when two terminals asking "what's next" get different tasks | **Done** |
| **M4, time and reach** | Recurrence (reset in place), `defer_until` everywhere, staleness, nightly in-process backup, `spokes backup` / `spokes restore`. Done when a restore from backup works | **Done** |
| After v1 | Auth and multiple users (OAuth, `user_id` already on every row), links (blocks/related), notifications, read-only web view, semantic search | In progress (**current**): the read-only web view's API half is done (`/api`, see [Read-only JSON API](#read-only-json-api)); the viewer itself is the companion repo `spokes-view`. The rest is not started |

### What works today (M0 to M4, plus the read-only API)

All fourteen tools: `guide`, `next`, `get`, `search`, `recent`, `add`, `update`, `set_status`, `move`, `focus`, `claim`, `release`, `ask`, `answer`. Plus a read-only JSON API on `/api` (`GET /api/tree`, `GET /api/nodes/{id}`, from 0.4.0 `GET /api/history`, from 0.5.0 `GET /api/search`, and from 0.15.0 `GET /api/tracked`) for the web view; see [Read-only JSON API](#read-only-json-api).

- `add`: one node, or a whole plan in one call with nested `children` (any depth up to 8, at most 200 nodes, one transaction). Returns one line with every new id, e.g. `Added SPOKE-5 task  Deploy  (under SPOKE-4 Skyforge), with 6 beneath it in the order given: SPOKE-6..SPOKE-11`. New nodes go last under their parent, unless the call passes `before` or `after` a sibling id (0.30.0, SPOKE-459): then the node is placed there in the same call, `parent` may be left out (it is the sibling's parent), and the reply says where it landed (`Added SPOKE-6 task  M0.5  (under SPOKE-2 Ladder, after SPOKE-3 M0, 2 of 4)`, counted like `move`, dropped siblings left out). When the new node becomes an ordered parent's next step ahead of one that was open, the reply ends `Next: SPOKE-6 is now the next step of SPOKE-2 Ladder.` Passing both, or a sibling that is not under the given `parent`, is refused and nothing is written. A bad child names itself (`children[3] ("Push"): ...`) and nothing is written. A field `add` does not take (SPOKE-222), at the top or on any child, is refused by name with what to do instead and nothing is written: `children[1] ("Push"): add does not take status, so nothing was added. To record work that is already finished, add it, then set_status done with the new ids.` Others with a pointer: `after`/`before` on a child (children go in the order given), `position`/`rank` (use `before`/`after`), `parent` or `agent` on a child, `id`, `claimed_by`, `focus`, `notes_append`. Duplicates (0.29.0, SPOKE-260): a node may not share its title with an unfinished sibling (compared as search normalizes text: `agent-feedback`, `Agent Feedback` and `agentfeedback` are the same), so a second one is refused with the existing node's id and nothing is written; two children of one call with the same title are refused too, and so are `move` and an `update` title that would make one. Done and dropped siblings never block, and `set_status` is never refused for it. The reply then lists up to three likely duplicates anywhere in the tree (`Possible duplicates of SPOKE-9: SPOKE-182 agent-feedback (same title, under SPOKE-5); ...`: same title, alias match, similar title, or a closed same-title sibling that could be reopened), even with hints off. Without an `agent` label it ends with a hint asking for one.
- `get`: the node's header line, its breadcrumb with the parent's id after the last name (`Hobbies / Programming / Skyforge (SPOKE-4)`, so it can go straight into `add`), who holds it (`claimed by opus-red until 14:20`, `stalled: claim by ... expired ...`, `waiting on the user: "..." (asked by sonnet-2)`, `answered 14:02, ready to claim`) and an outline `depth` levels deep (default 1, max 10), indented per level. Ordered children are numbered and the actionable step is marked `<- next`; unordered children are bulleted. The outline is capped at 60 lines and ends with `(+N more ...)` lines for what was cut by the cap or lies below the depth, plus a count of hidden dropped nodes. A child whose due date is later than a parent's shows `(after parent's Nov 20)`. `notes: true` appends the body; `history: true` appends the node's last 10 events, newest first (`14:02  claimed by opus-red: until ...`).
- Type and refinement: optional labels on any node, set by `add` (per node, nested children included) and `update`. `type` is `bug`, `feature`, `chore`, `idea`, `epic` or `spike` (research that ends in a written answer in notes); `refinement` says how ready work is to pick up: `idea`, `brainstormed`, `understood`, `planned` or `ready`. Input is case-insensitive. Each node shows only its own value (nothing is inherited). Refinement is separate from status, which tracks execution, and neither changes the other. The old argument name `stage` is refused. A done or dropped node shows no refinement (in `get`, `search`, `/api`) and matches no refinement filter, because refinement is about starting work; the stored value is kept and shows again if the node is reopened (0.10.2). Its type still shows. `get` lines show them after the priority, e.g. `SPOKE-8 task p1 bug ready +notes  Title`; `search` lines show them after the level, `next` lines after level, due and repeats.
- `guide`: one page explaining every field and state; it changes nothing. The connect-time instructions carry a short summary and point to it.
- Levels: `priority` 1 to 4 on any node; unset nodes inherit the nearest ancestor's level. `get` shows a node's own level; `next` and `search` show the effective one.
- `source` (0.7.0): the user's own words that led to a node, exactly as typed. Pass it on `add` (any node of a nested plan) whenever a person's message started the node. It is stored apart from notes, set once, and can only grow: `update` takes `source_append` (a later message, added after a blank line) and refuses any attempt to replace or clear `source`. `get` with `notes: true` prints it as a `SOURCE (the user's words)` block above `NOTES`; listings and `search` show `+source`; the text is searchable like notes; `/api` carries `has_source` and `node.source`. Up to 100,000 bytes. Why: if a model summarises an idea wrongly, you can start again from what was typed.
- Tracked (0.15.0, schema 6): `add` (per node) and `update` take `tracked: true` to mark a node the user follows, usually an epic (`false` clears it; tracking twice keeps the first time, and it is logged as `tracked` / `untracked`). It changes nothing about status, priority, focus or `next`. Lines show a `tracked` flag, `get` shows `Tracked since 2026-11-27` (in the user's time zone), `search` takes `tracked: true` (with an empty `query` it lists every tracked node), and spokes-view lists them with their progress via `GET /api/tracked`. It is set only through MCP; the API stays read-only.
- `update`: title, kind, ordered, priority (`0` clears), type and refinement (`""` clears), tracked, due / defer_until (`""` clears), aliases (`[]` clears), recur (`""` stops it), `notes` (replace; `""` clears) or `notes_append` (adds a paragraph), `source_append` (adds the user's words to `source`). Kind changes keep the rule that areas only live under areas and never finish.
- `move`: reparent (`parent`) and/or reorder (`before` / `after` a sibling; without either it goes last). Writes one row via fractional positions. Refuses moving the root, cycles, areas under tasks, closed parents, and an unfinished node into a parent that already has an unfinished child with the same title.
- `next`: the queue, in sections, one line per work item, each node and each actionable step at most once:
  `NEEDS YOU` (questions with agent and age, then stalled claims whose lease ran out, then answered tasks waiting for an agent), `FOCUS` (the ranked list, numbered by global rank), `DUE SOON` (overdue first, then due within 7 days), `UP NEXT` (priority level 1 to 4, then unset, oldest first) and `STALE` (up to 3 work items untouched for 30+ days, a nudge outside `limit`). Work items are top-level tasks (directly under areas), deeper tasks that carry their own level or due date, and anything on the focus list; later steps of an ordered parent are held back. A due or up-next line whose `next:` step (or, for a leaf, the node itself) is already shown higher up is dropped, so a project and its due sub-project never both point at the same step; focus entries are always kept. A project whose open steps are all claimed or waiting is not offered. Lines show the effective level (own, or inherited from the nearest ancestor), `due Nov 20` / `due today` / `overdue Nov 10`, progress and `next: SPOKE-7 ...` (the next actionable step). Columns are aligned as in the spec sample. `scope` limits it to a subtree; `limit` (default 5) caps the first four sections, ending with `(+N more open. Pass scope or limit.)`. Warns when the focus list passes 10 entries. `type` / `refinement` keep only lines whose own node matches, in every section.
- `search`: several keywords in one `query`, matched as alternatives (`server homelab nas proxmox`). A keyword that is a node id (`958`, `#958`, `SPOKE-958`, `spoke 958`) puts that node first, marked `(id)`, even when it is done or dropped (scope and the type/refinement filters still apply); text matches follow. Text is normalized (lowercase, accents folded, punctuation stripped, plus a squashed no-space form, so `home-lab` / `home lab` / `homelab` match) and indexed in an FTS5 trigram table. Typos still hit (edit distance 1-2 or shared trigrams); keywords under 3 letters (`pc`, `tv`) match whole words only; filler words (`for`, `my`, `tasks`) are ignored, but a whole multi-word title or alias found in the query (`my server`) earns a bonus. Ranking (0.13.0): each keyword counts by how rare it is among the matches (a word half the tree mentions says little), times how well and where it matched (title > aliases > notes; notes-only hits add up to about one title word at most, so a note that quotes the whole query can't outrank a node the words name); a parent whose title or aliases hold a third of the keywords gets a small lift, more when its own descendants match too (the "where do I add this" node); ties by level. `match`: `best` (default) leaves out matches scoring under 40% of the best when there are two or more keywords (always keeping the first 3) and says how many with `(N weaker matches ... left out; pass match: "any" ...)`; `any` lists every node matching any keyword; `all` only nodes matching every keyword (an id hit always stays). A result that matched only some of the keywords says which: `(matched 3 of 7: youtube, video, channel)` (a keyword no node has is not counted). Each line has flags, the effective level, `focus #N`, the path (`in … / Projects (SPOKE-4) / Spokes (SPOKE-5)`: the last 3 ancestors below the root, each with its id, and `… / ` in front when ancestors were cut; a top-level node shows `in To do (top level)`; `full_paths: true` shows the whole chain with ids; `/api/search` is unchanged) and, when the title did not match, why it did (`(alias "nas")`, `(in notes)`). Default 10 results (`limit` up to 50); `offset` (0.14.0, 0 to 10,000) skips that many for the next page: the header then says `(showing 11-20)` and the footer `(+N more matches. Pass offset: 20 for the next page ...)`; `scope`, `include_done` (done/dropped nodes and everything under them are hidden by default, with a count). `type` / `refinement` filter by the node's own value, and `tracked: true` keeps only tracked nodes; with an empty `query` and at least one filter it lists every matching node in scope (closed hidden unless `include_done`), by level then id, headed `N matches for type bug, refinement ready:`.
- `search` by state (0.27.0, SPOKE-1029): lists leftover work, including what `next` hides under a held or waiting parent. Leave `query` out (or empty) and pass `state` (`status` is accepted too): `stalled` (the claim's lease ran out), `claimed` (live claim), `in_progress` (both), `waiting` (on the user), `open` (nobody holds it), `children_done` (nothing open at any depth beneath it, at least one direct subtask done, itself unfinished) or, from 0.28.0, `under_closed` (unfinished tasks under a done or dropped node, naming it; only data written before the close rule has any, so after an upgrade fix what it lists until it finds nothing; like every state filter it does not list areas); `claimed_by` (what one agent label holds: claimed, stalled or waiting); `untouched_days` (1 to 3650: no activity on or beneath it for that long, deferred tasks left out, oldest first without keywords). They match unfinished tasks only (never areas), combine with each other, keywords, `type`, `refinement`, `tracked` and `scope`, and page with `limit`/`offset`. Each line ends with `(held by opus-red until 14:20)`, `(claim by sonnet-1 expired 2h ago)`, `(sonnet-2 asked: "Which vault?")` or `(untouched 41d)`. Matches under done or dropped nodes are counted and shown with `include_done: true` (`under_closed` shows them by itself). A listing that shows a stalled claim, or finished parents, ends with a `Next: ` hint. `/api/search` is unchanged.
- `recent`: the most recently added nodes, newest first by creation time, one line each with id, kind, title, path and when it was added (`added 3h ago, 16:03`). `limit` (default 10, max 50), `scope`, `include_done`. For "start the spoke I just added".
- `focus`: put a node at any depth (task or area) on the global ranked list: append, `rank` (1 = top, clamps to the end), or `before` / `after` another entry; `remove: true` takes it off. Fractional `focus_rank` keys, one row per write. Closing a node (`done`/`dropped`) takes it off the list. Warns past 10 entries.
- `set_status`: `done`, `dropped` or `open` on several ids at once. Areas cannot be done. Unfinished work never sits under closed work (0.28.0, SPOKE-345): `done` and `dropped` are refused while anything under the node, at any depth, is unfinished (claimed, waiting, deferred and repeating nodes count), and the refusal names the first ten, gives the other ids (with 100 or more under it, the `search` calls that list them), says which are claimed by another agent or waiting on the user, and says to close only what is finished or unwanted (ids and holders from 0.28.1). Passing parents and children in one call closes the whole branch, in any order. `open` is refused under a done or dropped node unless that node is reopened in the same call. A repeating task needs its steps finished or dropped before `done`, and its reset leaves dropped branches alone. `add` and `move` already refused closed parents. After closing a step it names the next step of an ordered parent (`Next: SPOKE-4 Two, the next step of SPOKE-2 Plan.`), or, when it was the last open step, suggests closing the parent (it never closes it itself).
- Hints: a reply may end with lines starting `Next: `, the next useful step worked out from the node's current state (SPOKE-738). All of them come from one module, `src/hints.rs`: a pure function of state and action, so adding a hint is one small change plus one test. A hint fires only when the state calls for it, and none suggests working on or finishing an area or a closed node. Claim hints (0.17.0, SPOKE-1006): `get` on an open task nobody holds that is the one to start (no open children, not a later step of an ordered parent) ends with `Next: Working on this? Claim it first ...`; `next` ends with `Next: Claim the one you pick before starting.` when it lists work; `update notes_append` on such a task ends with `Next: Recording progress? Claim it ...` unless the caller holds an ancestor (an orchestrator); and the `set_status` next-step hint adds `Claim it before starting.` when that step is open and unclaimed. `guide` explains them. Write hints (0.19.0 to 0.25.0, SPOKE-1079): `add` names the first step of an ordered plan, a task placed directly under the root and new tasks without aliases; `claim` points at the user's own words when there are no notes, and a claim refused because another agent holds it says where to find other work; `release` with no note, and no notes written since the claim, asks for one; `ask` says to record the reply with `answer`; `answer` says to claim before continuing; `move` names an ordered parent's new next step, or says the parent it left has no unfinished steps; (0.28.0 turned the `set_status dropped` hint about work left underneath into the refusal above). `SPOKES_HINTS=0` makes no hint lines (for benchmarks and scripts that parse replies); text the user wrote, such as a note line starting `Next: `, always comes back unchanged.
- Notes live in their own table and never appear in listings; `+notes` says a body exists.
- `unplanned` marks a task with no children and no notes, except steps of an ordered task (the parent is their plan).
- IDs are shown as `SPOKE-<n>`; `SPOKE-12`, `12` and `"12"` are all accepted, and numeric arguments also accept numeric strings (`"depth": "3"`).
- Claims (`claim`, `release`, lease renewal):
  - `claim(id, agent, lease_minutes=60)` sets `in_progress` with a lease (1 to 1440 minutes) in one conditional `UPDATE` (an `IMMEDIATE` transaction), so two agents cannot both win. The node and its whole subtree leave `next` for everyone.
  - Refused, with what to do next: areas and the root, done/dropped tasks, waiting tasks (answer first), or a task that another agent holds with a live lease (`SPOKE-142 is claimed by opus-red until 14:20. Pick another task, or ask the user.`, times in `SPOKES_TZ`). Only that node counts: claims above or beneath it do not block it.
  - Orchestrator and workers: claiming a step inside another agent's claimed project is allowed (the spec's orchestrator flow: the orchestrator claims the project and writes the plan, each worker claims its own step under its own label). The reply says whose project it is part of; the step stays hidden from `next` because its ancestor is claimed; `get` shows `claimed by <worker>` on the step and `in_progress by <worker>` in the parent's outline. Writes renew only the writer's own claims (the worker's step, the orchestrator's project). A worker's question (`ask`) inside a claimed project still shows under NEEDS YOU.
  - The order does not matter: an orchestrator can claim a project after a worker has taken a step in it. A claim on a task with work held beneath it, at any depth, names that work in the reply: ` Other agents hold work under it, which stays theirs: SPOKE-7 Roof (blue, until 14:20).` (three named, then `+N more`; lapsed claims are not listed).
  - Claiming again as the same agent renews (`Renewed ...`); claiming a lapsed lease takes it over and logs `took over from <agent>, whose lease expired ...`.
  - Any write (`add`, `update`, `move`, `set_status`, `focus`) passing the holder's `agent` on the node or beneath it renews the lease to at least 60 minutes from now (a longer lease is kept; a lapsed claim the agent still holds is revived).
  - Expiry is evaluated when `next` or `get` is read; there is no background job. A lapsed claim shows under NEEDS YOU as `stalled  Title  (claim by opus-red expired 2h ago)`.
  - `release(id, agent, note)` puts it back to `open`; the note is appended to the notes (`Released by opus-red (2026-11-14): ...`) and logged. Only the holder can release a live claim; anyone can release a lapsed one. The asking agent can also withdraw its question this way.
  - `set_status done/dropped/open` clears any claim, question and answered mark.
- Questions (`ask`, `answer`):
  - `ask(id, agent, question)` sets `waiting` with the question in `waiting_on`; NEEDS YOU shows `SPOKE-88  waiting  Set up emulator: "Install to D: or E:?" (sonnet-2, 3h ago)` (age from the `asked` event). Refused on areas, closed tasks, another agent's live claim or another agent's open question.
  - `answer(id, text)` appends `Question from sonnet-2 (date): ...` / `Answer from the user (date): ...` to the notes (searchable), clears the question and claim, and reopens the task.
  - Design choice for "back near the top": an answered task stays under NEEDS YOU, after questions and stalled claims, as `answered  Title  (answered 5m ago; claim it and read the notes)` until an agent claims it (or it is closed or asked again). It is stored as `answered_at` (schema step 3). Claiming it says to read the notes.
- Recurrence (`recur` on `add` / `update`): `{"every": 2, "unit": "week", "from": "completion"}` (unit `day`, `week`, `month`, `year`; `from` `completion`, the default, or `schedule`), or short text: `"every 2 weeks"`, `"weekly"`, `"monthly from schedule"`. `update recur: ""` stops it repeating. Lines show `repeats`; `get` adds `repeats every 2 weeks, counted from completion`. Repeating tasks are never flagged `unplanned`.
  - `set_status done` on a repeating task resets it in place (same id, focus place kept): logs a `recurred` event, reopens it with the next due date, clears claim, question and answered mark, reopens its finished steps (a repeating checklist), and replies `SPOKE-7 done  Check oil  (repeats every 2 weeks, counted from completion: back in the queue Nov 25, due Nov 28)`. `dropped` ends it (`(it stops repeating)`).
  - Next due date: `completion` counts from today (in `SPOKES_TZ`); `schedule` counts from the previous due date (from today if it has none) and skips periods already gone by when done late, so it never comes back overdue. Months clamp (Jan 31 + 1 month = Feb 28/29); a `schedule` rule remembers its day of month (`day` in the stored JSON), so the 31st comes back on Mar 31, not Mar 28.
  - Defer window: after a reset the task is hidden until a lead time before the next due date of a quarter of the interval, at most 14 days (daily: the due day; weekly: 1 day before; every 2 weeks: 3; monthly: 7; yearly: 14). If that day has already come it shows at once.
  - Areas cannot repeat, and neither can a step of an ordered parent (a step that reset itself would hold every later step back forever): `add`, `update` (including making a parent `ordered`) and `move` refuse it and say to put `recur` on the parent instead, whose steps reopen each time it repeats.
- `defer_until` (YYYY-MM-DD, in `SPOKES_TZ`) hides a node and its subtree from `next`, from `next:` step hints, from `<- next` in `get` and from STALE until that day. In an ordered parent a deferred step still holds the later steps back. Questions (`waiting`) and stalled claims show under NEEDS YOU even when deferred. Lines show `deferred until Nov 20`.
- Staleness: STALE lists up to 3 open work items (tasks, never areas) that are not deferred and whose `touched_at` (which bubbles up from anything beneath) is 30+ days old, oldest first, as `SPOKE-67  untouched 41d  Workbench build`, outside `limit` and never repeating a line shown above. Any write beneath a project, deferring it or dropping it stops the nudge.
- `get history: true` shows every timestamp, including those inside event details (`claimed by red: until 03:01`), as wall-clock time in `SPOKES_TZ`.
- Every write logs an event (`created`, `updated`, `status`, `moved`, `focus`, `claimed`, `released`, `asked`, `answered`) with the `agent` label.

## Running locally

Needs the Rust toolchain (`~/.cargo/bin` on `PATH`).

```sh
cargo run --release
# or, explicitly:
SPOKES_BIND=127.0.0.1:8787 SPOKES_DB_PATH=data/spokes.db cargo run --release
```

The database is created at `data/spokes.db` (gitignored) with the root node "To do" (`SPOKE-1`). Stop it with Ctrl-C. Check it with `curl http://127.0.0.1:8787/health`.

### Docker

```sh
docker build -t spokes:dev .
docker run -d --name spokes -p 8787:8787 -v spokes-data:/data spokes:dev
```

The image runs as uid 65532 (distroless, no shell), stores the database in the `/data` volume, and has a `HEALTHCHECK` that runs `spokes healthcheck`. Ports and volumes are chosen by whoever deploys it (the compose file in `stacks/`), not by this repo.

### Configuration

Environment variables only:

| Variable | Default | Meaning |
| --- | --- | --- |
| `SPOKES_BIND` | `127.0.0.1:8787` (image: `0.0.0.0:8787`) | Listen address |
| `SPOKES_DB_PATH` | `data/spokes.db` (image: `/data/spokes.db`) | SQLite file; its directory is created if missing |
| `SPOKES_ALLOWED_HOSTS` | unset (any `Host`) | Comma-separated `Host` header values to accept on `/mcp` and `/api`, e.g. `spokes.lan,192.168.5.100`. `*` or unset accepts any |
| `SPOKES_ALLOWED_ORIGINS` | unset (refuse any `Origin`) | Comma-separated browser origins to accept on `/mcp` and `/api`. By default every request that carries an `Origin` header is refused (403), which blocks browser-driven attacks such as DNS rebinding. MCP clients like Claude Code and OpenCode send no `Origin` |
| `SPOKES_TZ` | `UTC` | IANA time zone name (e.g. `America/Chicago`). Decides what "today" is for due dates, `overdue` and `defer_until`. An unknown name stops startup with an error |
| `SPOKES_BACKUP_DIR` | unset (no backups) | Directory for automatic backups (created if missing), e.g. `/data/backups` inside the volume |
| `SPOKES_BACKUP_KEEP` | `14` | How many backup files to keep; older ones are deleted after each backup |
| `SPOKES_BACKUP_INTERVAL_SECS` | `86400` | **Test only.** Seconds between backups; shorten it to watch a backup happen |
| `SPOKES_HINTS` | `1` | `0` (or `false`, `off`, `no`) leaves the `Next: ` hint lines out of every tool reply (notes and source are never touched). Any other value stops startup with an error |
| `SPOKES_LOG` | `info` | `error`, `warn`, `info`, `debug` or `trace` |

There is no auth in v1: run it only on the LAN or a private Tailscale network.

## Backup and restore

With `SPOKES_BACKUP_DIR` set, the server backs up its database once a day by itself (an in-process timer; the only scheduled job in spokes, since recurrence, deferral, leases and staleness are all evaluated when read). Each backup is a consistent snapshot taken with SQLite's `VACUUM INTO` on a separate read-only connection, so tool calls are not blocked, written as `spokes-YYYYMMDD-HHMMSS.db` (UTC) via a `.partial` file and a rename. After each one the oldest files beyond `SPOKES_BACKUP_KEEP` are deleted; only `spokes-*.db` / `spokes-*.partial` files are ever touched. On start it backs up at once unless the newest backup is younger than the interval. Results are logged (`backup written path=... bytes=... removed=...`, or `backup failed: ...`). Keep the backup directory on the same volume as the database, and copy it offsite if you want protection from disk loss.

A one-off backup, safe while the server runs:

```sh
spokes backup /path/to/dir-or-file.db
docker exec spokes /usr/local/bin/spokes backup /data/backups   # in the container
```

Restore (the server must be stopped; `spokes restore` refuses while `/health` answers on `SPOKES_BIND`):

```sh
# 1. stop the server (Ctrl-C, or: docker stop spokes)
# 2. restore; it checks the file (integrity_check, a nodes table, a schema this build knows),
#    moves the current database and its -wal/-shm aside as spokes.db.before-restore-<time>,
#    then puts the backup in place
spokes restore data/backups/spokes-20261114-080113.db
docker run --rm -v spokes-data:/data spokes:dev restore /data/backups/spokes-20261114-080113.db
# 3. start the server again
```

To undo a restore, stop the server and move the `before-restore` file(s) back.

### Copying old source blocks into `source` (`migrate-source`)

Before 0.7.0, agents kept the user's words in a `## Source (verbatim, DATE)` block at the top of notes. `spokes migrate-source` copies those blocks into `source`. It only adds: notes are never edited (see `docs/invariants.md`), so the words then appear in both places. A node whose `source` is already set is skipped, so it is safe to run twice.

```sh
docker exec spokes /usr/local/bin/spokes migrate-source           # dry run: lists the nodes, writes nothing
docker exec spokes /usr/local/bin/spokes migrate-source --apply   # backup, then copy; one `updated` event per node
```

The words under each `## Source (verbatim` heading are copied, up to the next `## ` heading; the heading line itself is left out, so a migrated source reads like one written natively (the dated block stays in the notes). A note with several blocks gets them joined by a blank line. `--apply` first writes `pre-migrate-source-<time>.db` next to the other backups (the rotation never touches that name).

## Deployment

The homelab runs the published image from the `spokes/` stack in the Arcane `stacks` repo (`~/dev/stacks/spokes`, `/srv/stacks/spokes` on the server; its README covers start/stop, clients and backups): `ghcr.io/brandonnorsworthy/spokes:<X.Y.Z>` pinned, port `10050` -> `8787`, UID `10010`, `./data` bind mount for the database and backups. The server never builds; it pulls the tag CI published.

## CI and releases

- `.github/workflows/ci.yml` (check name `ci`): `cargo fmt --check`, `cargo clippy --all-targets -D warnings`, `cargo test --locked`, and a non-pushing `docker build`, on every PR, push to `main` and weekly. Dependency review on PRs.
- `.github/workflows/codeql.yml`: CodeQL for Rust and for the workflows (`actions`), on PRs, `main` and weekly.
- `.github/workflows/release.yml`: on push to `main` and on `v*` tags, reruns `ci`, then builds and pushes `ghcr.io/brandonnorsworthy/spokes` (`latest` and `sha-<short>` from `main`; `X.Y.Z` and `sha-<short>` from a tag) and scans it with Trivy (HIGH/CRITICAL fail the job).
- `.github/dependabot.yml`: weekly cargo, GitHub Actions and Docker base image updates. Actions are pinned to commit SHAs.

Release process: the version in `Cargo.toml` (it is what `initialize` reports as `serverInfo.version`) is bumped by the git hooks, not by hand (see Development). Push to `main`, wait for green, then tag that commit `vX.Y.Z` with the version it carries and push the tag. Deploy by bumping the pinned tag in `stacks/spokes/compose.yml`. Rolling back is usually pinning the previous tag, **except across a schema change**: a server refuses a database whose schema is newer than it knows (0.7.0 moved it to version 4, the release with `type` and `refinement` to version 5), so going back to 0.6.x means `spokes restore` of a backup taken before the upgrade (writes made since are lost).

## Connecting clients

The endpoint is `http://<host>:8787/mcp` (Streamable HTTP, stateless; both the 2026-07-28 protocol and older initialize-based clients work).

Claude Code:

```sh
claude mcp add --transport http spokes http://127.0.0.1:8787/mcp
```

OpenCode (`opencode.json`):

```json
{
  "mcp": {
    "spokes": { "type": "remote", "url": "http://127.0.0.1:8787/mcp", "enabled": true }
  }
}
```

Then ask: "add Home, Work and Hobbies, then tell me what's next".

## Feedback loop: how Spokes keeps improving

Spokes is improved by the agents that use it. The loop is simple and always on:

1. **Agents report.** After using Spokes, an agent adds **one** piece of feedback: the one thing it would change to make the tool better. It goes as a task under the `agent-feedback` area (`SPOKE-182`, in Spokes). Title is the change, notes say what happened, which tool, and which agent/model.
2. **Brandon collects.** New items under `agent-feedback` are the inbox. Ask any agent "what's new in agent-feedback?" (`get SPOKE-182`).
3. **Pick and build.** Take an item, build it as a normal commit, and mark it done in Spokes. A reply that surprised an agent last week should not surprise the next one.
4. **Repeat.** Every agent session is a chance to feed it.

To leave feedback (agents, do this at the end of a session):

```text
add under SPOKE-182: "<the change, in one line>"
  notes: what you did, what went wrong or felt clumsy, what you expected instead
```

Rules of thumb: one change per item, add under the existing SPOKE-182 (a second `agent-feedback` under the same parent is refused), search it first so you do not duplicate an item, and describe the problem you hit, not just the fix you imagine.

## Read-only JSON API

For the web view (the companion repo `spokes-view`), whose backend calls it server to server. Four `GET` endpoints under `/api` (`/api/history` arrived in 0.4.0 and `/api/search` in 0.5.0, both purely additive: no schema migration, no MCP tool's output changed, and the earlier endpoints answer exactly as before) on the same port as `/mcp`. It is strictly read-only: it reads through its own SQLite connection opened read-only (`SQLITE_OPEN_READ_ONLY` and `query_only`), so it cannot write, renew a lease or log an event, and since WAL lets it read while MCP writes, it never holds the MCP connection's lock. Each response is built inside one read transaction (a consistent snapshot) on the blocking pool, from a handful of queries whatever the size of the tree.

### Rules for every request

- **GET only.** Any other method gets `405`; an unknown path under `/api` gets `404`.
- **Origin and Host, as on `/mcp`.** A request that carries an `Origin` header is refused with `403` unless the origin is in `SPOKES_ALLOWED_ORIGINS` (same matching as `/mcp`: scheme and host must match; an entry with `:*` or with no port matches any port, an explicit port only that port). With `SPOKES_ALLOWED_HOSTS` set, the `Host` header must match one entry (an entry without a port matches any port), else `403`. No CORS headers are sent: the API is meant for a backend, not for a browser calling it directly.
- **Errors** are JSON, `{"error": "..."}`, with `400` (bad parameter: unknown or repeated name, bad id, depth out of range, a boolean that is not `true`/`false`/`1`/`0`), `403`, `404` (unknown node or endpoint; an unknown endpoint's message starts with `no such endpoint`, which `spokes-view` uses to spot a server older than 0.4.0), `405` or `500`.
- **Caching.** Every `200` carries a weak `ETag` (`W/"..."`) and `Cache-Control: no-cache`. Send it back as `If-None-Match` (a single tag, a list, or `*`) and an unchanged answer is `304` with no body, which costs two small queries. The tag covers the query, the revision (any write), SQLite's change counter for the API's connection (any commit, even one made outside spokes), "today" in `SPOKES_TZ` (dates and deferrals roll over at midnight) and the number of lapsed claims (a lease running out turns a node `stalled` with no write), plus a per-start id, so a restart or a restore never answers `304` from an old tag. Treat the tag as opaque.
- **Compression.** With `Accept-Encoding: gzip` the body is gzipped (a 2000-node slice is about 1.1 MB of JSON and 15 KB gzipped, since nodes repeat the same keys).

### `GET /api/tree`

A slice of the tree, for lazy loading: load a few levels, then fetch a branch when the user opens it.

| Parameter | Default | Meaning |
| --- | --- | --- |
| `root` | `1` | The node to start from: `SPOKE-12` or `12` |
| `depth` | `3` | Levels below `root` to return, `0` (the root alone) to `10` |
| `include_closed` | `false` | `true` also returns done nodes and everything under them, and dropped ones too unless `include_dropped` says otherwise. When `false` done nodes and their whole subtrees are left out. The `root` itself is always returned, even when it is closed |
| `include_dropped` | follows `include_closed` | `true` or `false`, on its own: `include_closed=true&include_dropped=false` shows done nodes but leaves out dropped ones and their subtrees (spokes-view sends both, one per checkbox). `include_closed=false&include_dropped=true` shows only the dropped ones. Since 0.6.0 |

The response holds at most **2000 nodes** (`node_limit`). Nodes are expanded breadth-first, so the shallow levels fill first, and a node's children are sent all together or not at all. At the first node whose children do not fit, expansion stops and `truncated` is `true`; that node and every node not expanded have `children_loaded: false`, so fetch them with `root=<id>`. The one exception: when the root alone has more visible children than fit, its first 1999 are sent (in order) with `children_loaded: false` on the root; this version has no paging past that.

Top-level fields:

| Field | Meaning |
| --- | --- |
| `rev` | Revision: the largest event id. Every write logs an event, so it changes whenever anything is written. Compare for equality only |
| `version` | The server version (`Cargo.toml`) |
| `time_zone` | `SPOKES_TZ`, e.g. `America/Chicago` |
| `today` | Today in that zone, `YYYY-MM-DD` (what `deferred` and overdue are judged against) |
| `root`, `depth`, `include_closed`, `include_dropped` | The query as applied |
| `node_limit` | The cap, `2000` |
| `path` | The root's ancestors, top first, each `{"id", "title"}`, starting with `{"id": 1, "title": "To do"}`. Empty for the root node |
| `truncated` | `true` when the cap stopped expansion |
| `nodes` | The nodes in pre-order (a node, then its children in their stored position order, each followed by its own subtree). Rebuild the tree from `parent` |

Each node (every field is always present; `null` when unset):

| Field | Type | Meaning |
| --- | --- | --- |
| `id` | number | `12` for `SPOKE-12` |
| `parent` | number or null | Parent id; `null` only for the root node `1` |
| `kind` | string | `area` or `task` |
| `ordered` | bool | Its children are sequential steps |
| `title` | string | |
| `status` | string | `open`, `in_progress`, `waiting`, `done`, `dropped` |
| `priority` | 1-4 or null | Its own level |
| `type` | string or null | `bug`, `feature`, `chore`, `idea` or `epic`; its own value |
| `refinement` | string or null | `idea`, `brainstormed`, `understood`, `planned` or `ready`; its own value, separate from `status` |
| `tracked` | bool | The user tracks it (0.15.0; see `GET /api/tracked`) |
| `effective_priority` | 1-4 or null | Its own level, or the nearest ancestor's (as `next` and `search` show it) |
| `focus` | number or null | 1-based rank on the focus list |
| `due`, `defer_until` | `YYYY-MM-DD` or null | As stored |
| `deferred` | bool | `defer_until` is after `today` |
| `claimed_by`, `claim_expires` | string or null | The claiming agent and the lease end (RFC 3339 UTC) |
| `stalled` | bool | `in_progress` and the lease has run out |
| `waiting_on` | string or null | The agent's open question (`status` is `waiting`) |
| `answered` | bool | The user answered a question and no agent has claimed it since |
| `repeat` | string or null | The repeat rule in words, e.g. `every 2 weeks, counted from completion` |
| `has_notes` | bool | A notes body exists (bodies are never in `/api/tree`) |
| `has_source` | bool | The user's raw words are stored on the node (0.7.0; the words are in `/api/nodes/{id}` only) |
| `unplanned` | bool | An open task with no children and no notes, as `+unplanned` in the tools |
| `is_step` | bool | Its parent is an ordered task |
| `is_next` | bool | It is the actionable step of its ordered parent (`<- next` in `get`): the first unfinished child, if that one is open and not deferred |
| `progress` | `{"done", "total"}` or null | For a task with children: done / not-dropped direct children (`(2/6)`) |
| `created_at`, `updated_at`, `touched_at` | string | RFC 3339 UTC. `touched_at` bubbles up from anything beneath |
| `child_count` | number | Direct children visible under this query (done ones count only with `include_closed=true`, dropped ones only with `include_dropped`, which follows `include_closed` when omitted) |
| `descendant_count` | number | All visible nodes beneath it, at any depth |
| `closed_child_count` | number | Direct children that are done or dropped, whether or not they are shown |
| `dropped_child_count` | number | Direct children that are dropped, whether or not they are shown (0.6.0) |
| `children_loaded` | bool | All of its visible children are in this response (`true` for a node with none). `false` means fetch `root=<id>` to see them |

Example (trimmed):

```sh
curl -s 'http://127.0.0.1:8787/api/tree?depth=2'
```

```json
{"rev":10,"version":"0.2.0","time_zone":"UTC","today":"2026-11-15","root":1,"depth":2,
 "include_closed":false,"node_limit":2000,"path":[],"truncated":false,"nodes":[
  {"id":1,"parent":null,"kind":"area","ordered":false,"title":"To do","status":"open",
   "priority":null,"effective_priority":null,"focus":null,"due":null,"defer_until":null,
   "deferred":false,"claimed_by":null,"claim_expires":null,"stalled":false,"waiting_on":null,
   "answered":false,"repeat":null,"has_notes":false,"unplanned":false,"is_step":false,
   "is_next":false,"progress":null,"created_at":"2026-11-15T19:08:55Z",
   "updated_at":"2026-11-15T19:08:55Z","touched_at":"2026-11-15T19:08:56Z",
   "child_count":1,"descendant_count":5,"closed_child_count":0,"children_loaded":true},
  {"id":2,"parent":1,"kind":"area","title":"Home","priority":2,"effective_priority":2,
   "child_count":1,"descendant_count":4,"closed_child_count":0,"children_loaded":true, "...": "..."},
  {"id":3,"parent":2,"kind":"task","ordered":true,"title":"Deploy","effective_priority":2,
   "focus":1,"repeat":"every 2 weeks, counted from completion","has_notes":true,
   "progress":{"done":1,"total":3},"child_count":2,"descendant_count":3,
   "closed_child_count":1,"children_loaded":false, "...": "..."}
 ]}
```

### `GET /api/nodes/{id}`

One node in full: `{id}` is `SPOKE-12` or `12`. Optional `include_closed` (default `false`) and `include_dropped` (default: follows `include_closed`) decide how its counts treat closed children, as above.

| Field | Meaning |
| --- | --- |
| `rev`, `version`, `time_zone`, `today` | As in `/api/tree` |
| `path` | Its ancestors, top first, `{"id", "title"}` |
| `node` | Every node field above, plus `notes`: the notes body, or `null`, and `source` (0.7.0): the user's raw words, or `null` |
| `events` | Its last 10 events, newest first (what `get history: true` shows), each `{"id", "at", "agent", "type", "detail"}`: `at` is RFC 3339 UTC, `agent` may be `null`, `type` is one of `created`, `updated`, `status`, `moved`, `focus`, `claimed`, `released`, `asked`, `answered`, `recurred`; `detail` is the full text (timestamps in it are UTC) |

```sh
curl -s http://127.0.0.1:8787/api/nodes/SPOKE-3
```

```json
{"rev":10,"version":"0.2.0","time_zone":"UTC","today":"2026-11-15",
 "path":[{"id":1,"title":"To do"},{"id":2,"title":"Home"}],
 "node":{"id":3,"parent":2,"kind":"task","title":"Deploy", "...": "...", "notes":"the plan"},
 "events":[{"id":10,"at":"2026-11-15T19:08:56Z","agent":null,"type":"focus","detail":"#1 of 1"},
           {"id":3,"at":"2026-11-15T19:08:56Z","agent":null,"type":"created","detail":"Deploy"}]}
```

### `GET /api/history`

Everything a viewer needs to replay a subtree's history from the start: every node under `root` and every event of those nodes. Read-only like the rest.

| Parameter | Default | Meaning |
| --- | --- | --- |
| `root` | `1` | The subtree's top: `SPOKE-12` or `12`. Unknown `root` is `404`; any other parameter, or `root` twice, is `400` |

| Field | Meaning |
| --- | --- |
| `rev`, `version`, `time_zone`, `today` | As in `/api/tree` |
| `root` | The root's id (number) |
| `path` | The root's ancestors, top first, `{"id", "title"}` (as in `/api/tree`) |
| `node_limit`, `event_limit` | The caps: `5000` nodes, `20000` events |
| `truncated` | `true` when either cap cut something |
| `nodes` | The root and everything under it at any depth, **including done and dropped nodes and what lies under them**, in pre-order with siblings in stored order. Each node has the same fields as in `/api/tree` as if `include_closed=true` (counts cover closed children; `children_loaded` is `true` when all of a node's children are in the response). Only the first 5000 in pre-order are sent |
| `events` | Every event whose node is in `nodes`, oldest first (by `id`), at most the first 20000. Each: `{"id", "node", "at", "agent", "type", "detail", "from_parent", "to_parent", "from_status", "to_status"}`. `node` is the node id; `at`, `agent`, `type`, `detail` are raw as in `/api/nodes/{id}` |

The last four event fields are read out of `detail` and are `null` unless: `moved` with `SPOKE-a -> SPOKE-b` gives `from_parent` and `to_parent` (a plain reorder, `reordered under SPOKE-x`, gives none); `status` with `x -> y` gives `from_status` and `to_status`; `recurred` with `x -> done -> open; ...` gives `from_status` `x` and `to_status` `"open"`. Statuses are one of `open`, `in_progress`, `waiting`, `done`, `dropped`; anything unparseable is `null`, never an error. The ETag tag string is `h.<root>`. The cost grows with the subtree (a few queries, not one per node).

```sh
curl -s 'http://127.0.0.1:8787/api/history?root=SPOKE-5'
```

```json
{"rev":296,"version":"0.4.0","time_zone":"UTC","today":"2026-11-23","root":5,
 "path":[{"id":1,"title":"To do"}],"node_limit":5000,"event_limit":20000,"truncated":false,
 "nodes":[{"id":5,"parent":1,"kind":"area","title":"Work","...":"..."}],
 "events":[{"id":167,"node":108,"at":"2026-11-16T05:53:53Z","agent":null,"type":"moved",
            "detail":"SPOKE-14 -> SPOKE-13","from_parent":14,"to_parent":13,
            "from_status":null,"to_status":null}]}
```

### `GET /api/search`

The `search` tool's ranked matches as data, so a viewer can find a node by a half-remembered name and jump to it (0.5.0+). It runs the same code as the MCP tool (normalization, trigrams, typos, aliases and notes, ranking title > aliases > notes, ties by level), so both always agree; the tool's text output is unchanged.

| Parameter | Default | Meaning |
| --- | --- | --- |
| `q` | (required) | The keywords, as alternatives (`fuzy serch`). Trimmed; 1 to 200 characters and at least one word, else `400` |
| `limit` | `10` | Results to send, `1` to `50` |
| `offset` | `0` | Skip this many ranked matches (`0` to `10000`), for the next page. Past the end is an empty `results`. Since 0.14.0 |
| `include_closed` | `false` | Also match done nodes and what lies under them (and dropped ones, unless `include_dropped` says otherwise) |
| `include_dropped` | follows `include_closed` | Also match dropped nodes and what lies under them. Since 0.6.0 |
| `scope` | `1` | Only nodes under this one (`SPOKE-12` or `12`; the node itself is not a match). Unknown is `404` |
| `match` | `best` | `best`, `any` or `all`, as for the `search` tool (case-insensitive; anything else is `400`). Since 0.13.0 |

Any other parameter, or one given twice, is `400`.

| Field | Meaning |
| --- | --- |
| `rev`, `version`, `time_zone`, `today` | As in `/api/tree` |
| `query`, `scope`, `include_closed`, `include_dropped` | What was asked (`query` trimmed) |
| `keywords` | The words searched for after normalization (filler words dropped, at most 12) |
| `match` | How keywords combined (`best`, `any`, `all`). Since 0.13.0 |
| `offset` | Where this page starts. Since 0.14.0 |
| `total` | Matches found, before `offset` and `limit` |
| `hidden_closed` | Matches hidden because they are closed or under a closed node (0 when both are shown) |
| `weaker_hidden` | Matches left out by `match=best` (not in `total`). Since 0.13.0 |
| `results` | Best first, each `{"id", "title", "kind", "status", "effective_priority", "focus", "path", "via"}`. `path` is the ancestors from the root down (`{"id", "title"}`, the root included, the node itself not), so a viewer can load the branches down to it. `via` says why a node matched when its title did not (`alias "nas"`, `in notes`), else `null` |

The ETag tag string is `s.<hash of q>.<limit>.<offset>.<include_closed><include_dropped>.<scope>.<match>`. Nothing found is a `200` with empty `results`.

```sh
curl -s 'http://127.0.0.1:8787/api/search?q=fuzy%20serch&limit=1'
```

```json
{"rev":314,"version":"0.5.0","time_zone":"UTC","today":"2026-11-24","query":"fuzy serch",
 "scope":1,"include_closed":false,"keywords":["fuzy","serch"],"total":11,"hidden_closed":3,
 "results":[{"id":172,"title":"Fuzzy-find search in the Spokes View","kind":"task","status":"open",
             "effective_priority":1,"focus":null,
             "path":[{"id":1,"title":"To do"},{"id":2,"title":"Personal"}],"via":null}]}
```

### `GET /api/tracked`

The nodes the user tracks (set through MCP with `update tracked: true`), newest tracked first, for a "Tracked" list (0.15.0). No parameters (any is `400`); closed nodes stay listed. Purely additive.

| Field | Meaning |
| --- | --- |
| `rev`, `version`, `time_zone`, `today` | As in `/api/tree` |
| `nodes` | Each `{"node", "path", "progress", "tracked_at"}`: `node` is the same JSON as in `/api/nodes/{id}`, `path` the ancestors root-first as there, `tracked_at` the UTC instant tracking began, `progress` `{"done", "in_progress", "total"}` over the node's descendant tasks (areas, the node itself and dropped tasks are not counted; `total` is every non-dropped descendant task) |

The ETag tag string is `k`.

```json
{"rev":402,"version":"0.15.0","time_zone":"UTC","today":"2026-11-27",
 "nodes":[{"node":{"id":1017,"title":"...","tracked":true,"...":"..."},
           "path":[{"id":1,"title":"To do"}],
           "progress":{"done":3,"in_progress":1,"total":9},
           "tracked_at":"2026-11-27T10:00:00Z"}]}
```

### Size and speed

Measured on the dev PC with the release build and a seeded tree of 20,411 nodes (10 areas, 400 projects, 20,000 tasks): `GET /api/tree` (depth 3 from the root, capped at 1,961 nodes, `truncated: true`) answers in about 30 ms; a branch (`root=<project>&depth=1`) in about 4 ms; a `304` in under 1 ms. A slice reads the skeleton of the root's whole subtree (id, parent, status) to get exact counts, so its cost grows with the subtree, not with the response.

## Development

Turn the version hooks on once per clone:

```sh
git config core.hooksPath .githooks
```

They bump the version in `Cargo.toml` and `Cargo.lock` from the commit message and fold the bump into the commit: `feat:` bumps the minor version, `feat!:` (any `type!:`, or a `BREAKING CHANGE:` line) the major, any other commit that changes code the patch. Commits that touch only docs, `.github/` or `.githooks/` do not bump, and neither do merges, rebases, or a commit that already changes the version. CI fails a pull request that changes code without changing the version (`.githooks/check-version-bump`; Dependabot PRs are exempt).

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

`tests/mcp_http.rs` starts the real server on a random port with a temp database and drives `/mcp` over HTTP: the M0 skeleton (initialize, tools/list, tools/call, the 2026-07-28 stateless form, Origin refusal, restart persistence), the M1 tree (a six-step plan written and walked in three calls, depth, notes, update, move, error paths) M2 (search normalization, typos, short keywords, ranking, scope and include_done over a tree with decoys; level inheritance; focus placement and errors; due ordering and every `next` section; `SPOKES_TZ`) M3 (two clients reading `next` and claiming different tasks; claim refusals and lease renewal by every write tool; 24 concurrent claims over HTTP with exactly one winner; ask, NEEDS YOU, answer, takeover of an expired lease, `get` history; the M2 retro regression where a project and its due sub-project both pointed at the same step) and M4 (recurrence in both `from` modes with month clamping, the day-of-month anchor, late completions, the defer window, repeating checklists and the refusals; `defer_until` everywhere; STALE ordering, cap and how deferring, dropping and activity stop it; the orchestrator/worker claim pattern, including the next-step hint a worker gets inside the orchestrator's claimed ordered project; history times in `SPOKES_TZ`). `tests/api_http.rs` drives `/api` the same way: slices by root and depth (pre-order, stored order after a `move`, every count, `path`), closed nodes hidden and shown, every node field (inherited level, focus rank, deferral, `is_step`/`is_next`, claims, questions, answers, repeats, notes, progress), the 2000-node cap (no node ever sends part of its children; a root with 2500 children), node detail with notes and the last 10 events, ETag/304 in every `If-None-Match` form (a write, a direct database change and a lease running out each change the tag), Origin and Host rules, 405/404, every bad parameter, that reading never changes a row, `/api/history` (moves, closed nodes, recurrence, reorders, scoping, the 5000-node cap, tag and 304, every bad parameter), `/api/search` (typos, alias and notes matches with `via`, breadcrumb ids, closed nodes hidden and counted, `limit`/`total`, `scope`, tag and 304 per query, every bad parameter), and a timed depth-3 slice of a 20,410-node tree seeded by SQL (about 160 ms in the debug test build). `tests/backup.rs` runs the server with a 1-second backup interval and `keep: 2`, checks rotation, wipes the database, restores the newest backup and reads the data back over MCP. Unit tests cover the recurrence date arithmetic (`src/recur.rs`) and backup rotation and restore validation (`src/backup.rs`). Lease expiry and question age are tested by moving timestamps into the past through a second connection to the database file. `src/store/claims.rs` also races 8 separate SQLite connections (as separate processes would) on one claim.

### Layout

| Path | What |
| --- | --- |
| `src/main.rs` | Process entry: config, logging, signals, `healthcheck`, `backup` and `restore` subcommands |
| `src/backup.rs` | Backups (`VACUUM INTO`), rotation, the daily timer, restore |
| `src/recur.rs` | Repeat rules: parsing, next due date, defer window |
| `src/config.rs` | Environment variables |
| `src/server.rs` | axum router: `/mcp` (rmcp Streamable HTTP, stateless), `/api` and `/health` |
| `src/api.rs` | The read-only JSON API: routes, parameters, Host/Origin guard, ETag/304, gzip; its own read-only SQLite connection |
| `src/tools/` | MCP surface: tool definitions, argument schemas, server instructions |
| `src/store/` | One file per operation (`add`, `get`, `next`, `search`, `focus`, `status`, `update`, `moves`, `claims` for claim/release/ask/answer), `fts.rs` (keeps the search index in step), `view.rs` (the API's tree slices, node detail and subtree history, as JSON-ready structs), plus shared queries |
| `src/text.rs` | Search normalization, keywords and fuzzy matching |
| `src/db/` | Opening SQLite (WAL), migrations (`PRAGMA user_version` steps), root seed, fractional position keys |
| `src/model.rs`, `src/format.rs` | Domain types and the one-line-per-node text format |

Schema changes: append a step to `STEPS` in `src/db/migrations.rs` (SQL, or a Rust function for backfills); never edit a shipped step. Step 2 creates `node_fts` and backfills it; step 3 adds `nodes.answered_at`; step 4 adds `nodes.source` (the index folds it into its notes column, so it needed no new index column); step 5 adds the nullable `nodes.work_type` and `nodes.refinement` columns (existing nodes keep both null; a build older than step 5 refuses a database at version 5); step 6 adds the nullable `nodes.tracked_at` (0.15.0; a 0.14.x server refuses a database at version 6, so rolling back restores a backup). The index is written from Rust (`store::fts::reindex`) by `add` and `update`, not by triggers, because the text is normalized in Rust; status and position live only in `nodes`, which search joins.

The MCP SDK (`rmcp`) is pinned to an exact version; read its changelog before bumping.
