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
- **Twelve tools:** `next`, `get`, `search`, `add`, `update`, `set_status`, `move`, `focus`, `claim`, `release`, `ask`, `answer`.

## Phases

| Phase | Scope | Status |
| --- | --- | --- |
| **M0, walking skeleton** | `add`, `get`, `next`, `set_status` on SQLite (WAL), stateless Streamable HTTP on `/mcp`, Docker image. Done when a fresh chat can add three areas and list them | **Done** |
| **M1, the tree** | Ordered/unordered children, `move`, `update`, notes apart from listings, nested `add`, breadcrumb paths, the full compact output format. Done when a model can write and walk a six-step plan in three calls | **Done** |
| **M2, finding and ranking** | `search` (aliases, normalization, trigram FTS5), priority inheritance, `focus`, due dates with `SPOKES_TZ`, the full `next` ordering. Done when "priority tasks for my server" finds the right project from a blank context | **Done** |
| **M3, agents** | `claim`, `release`, `ask`, `answer`, leases, event log surfaced. Done when two terminals asking "what's next" get different tasks | **Done** |
| **M4, time and reach** | Recurrence (reset in place), `defer_until` everywhere, staleness, nightly in-process backup, `spokes backup` / `spokes restore`. Done when a restore from backup works | **Done** |
| After v1 | Auth and multiple users (OAuth, `user_id` already on every row), links (blocks/related), notifications, read-only web view, semantic search | Not started (**current**) |

### What works today (M0 to M4)

All twelve tools: `next`, `get`, `search`, `add`, `update`, `set_status`, `move`, `focus`, `claim`, `release`, `ask`, `answer`.

- `add`: one node, or a whole plan in one call with nested `children` (any depth up to 8, at most 200 nodes, one transaction). Returns one line with every new id, e.g. `Added SPOKE-5 task  Deploy  (under SPOKE-4 Skyforge), with 6 beneath it in the order given: SPOKE-6..SPOKE-11`. A bad child names itself (`children[3] ("Push"): ...`) and nothing is written.
- `get`: the node's header line, its breadcrumb (`Hobbies / Programming / Skyforge`), who holds it (`claimed by opus-red until 14:20`, `stalled: claim by ... expired ...`, `waiting on the user: "..." (asked by sonnet-2)`, `answered 14:02, ready to claim`) and an outline `depth` levels deep (default 1, max 10), indented per level. Ordered children are numbered and the actionable step is marked `<- next`; unordered children are bulleted. The outline is capped at 60 lines and ends with `(+N more ...)` lines for what was cut by the cap or lies below the depth, plus a count of hidden dropped nodes. A child whose due date is later than a parent's shows `(after parent's Nov 20)`. `notes: true` appends the body; `history: true` appends the node's last 10 events, newest first (`14:02  claimed by opus-red: until ...`).
- Levels: `priority` 1 to 4 on any node; unset nodes inherit the nearest ancestor's level. `get` shows a node's own level; `next` and `search` show the effective one.
- `update`: title, kind, ordered, priority (`0` clears), due / defer_until (`""` clears), aliases (`[]` clears), recur (`""` stops it), `notes` (replace; `""` clears) or `notes_append` (adds a paragraph). Kind changes keep the rule that areas only live under areas and never finish.
- `move`: reparent (`parent`) and/or reorder (`before` / `after` a sibling; without either it goes last). Writes one row via fractional positions. Refuses moving the root, cycles, areas under tasks and closed parents.
- `next`: the queue, in sections, one line per work item, each node and each actionable step at most once:
  `NEEDS YOU` (questions with agent and age, then stalled claims whose lease ran out, then answered tasks waiting for an agent), `FOCUS` (the ranked list, numbered by global rank), `DUE SOON` (overdue first, then due within 7 days), `UP NEXT` (priority level 1 to 4, then unset, oldest first) and `STALE` (up to 3 work items untouched for 30+ days, a nudge outside `limit`). Work items are top-level tasks (directly under areas), deeper tasks that carry their own level or due date, and anything on the focus list; later steps of an ordered parent are held back. A due or up-next line whose `next:` step (or, for a leaf, the node itself) is already shown higher up is dropped, so a project and its due sub-project never both point at the same step; focus entries are always kept. A project whose open steps are all claimed or waiting is not offered. Lines show the effective level (own, or inherited from the nearest ancestor), `due Nov 20` / `due today` / `overdue Nov 10`, progress and `next: SPOKE-7 ...` (the next actionable step). Columns are aligned as in the spec sample. `scope` limits it to a subtree; `limit` (default 5) caps the first four sections, ending with `(+N more open. Pass scope or limit.)`. Warns when the focus list passes 10 entries.
- `search`: several keywords in one `query`, matched as alternatives (`server homelab nas proxmox`). Text is normalized (lowercase, accents folded, punctuation stripped, plus a squashed no-space form, so `home-lab` / `home lab` / `homelab` match) and indexed in an FTS5 trigram table. Typos still hit (edit distance 1-2 or shared trigrams); keywords under 3 letters (`pc`, `tv`) match whole words only; filler words (`for`, `my`, `tasks`) are ignored, but a whole multi-word title or alias found in the query (`my server`) earns a bonus. Ranked title > aliases > notes, ties by level. Each line has flags, the effective level, `focus #N`, the breadcrumb (`in Hobbies / Homelab`) and, when the title did not match, why it did (`(alias "nas")`, `(in notes)`). Default 10 results (`limit` up to 50), `scope`, `include_done` (done/dropped nodes and everything under them are hidden by default, with a count).
- `focus`: put a node at any depth (task or area) on the global ranked list: append, `rank` (1 = top, clamps to the end), or `before` / `after` another entry; `remove: true` takes it off. Fractional `focus_rank` keys, one row per write. Closing a node (`done`/`dropped`) takes it off the list. Warns past 10 entries.
- `set_status`: `done`, `dropped` or `open` on several ids at once. Areas cannot be done. After closing a step it names the next step of an ordered parent, or, when it was the last open step, suggests closing the parent (it never closes it itself).
- Notes live in their own table and never appear in listings; `+notes` says a body exists.
- `unplanned` marks a task with no children and no notes, except steps of an ordered task (the parent is their plan).
- IDs are shown as `SPOKE-<n>`; `SPOKE-12`, `12` and `"12"` are all accepted, and numeric arguments also accept numeric strings (`"depth": "3"`).
- Claims (`claim`, `release`, lease renewal):
  - `claim(id, agent, lease_minutes=60)` sets `in_progress` with a lease (1 to 1440 minutes) in one conditional `UPDATE` (an `IMMEDIATE` transaction), so two agents cannot both win. The node and its whole subtree leave `next` for everyone.
  - Refused, with what to do next: areas and the root, done/dropped tasks, waiting tasks (answer first), a task held by another agent with a live lease (`SPOKE-142 is claimed by opus-red until 14:20. Pick another task, or ask the user.`, times in `SPOKES_TZ`), or one with a step beneath it claimed by another agent.
  - Orchestrator and workers: claiming a step inside another agent's claimed project is allowed (the spec's orchestrator flow: the orchestrator claims the project and writes the plan, each worker claims its own step under its own label). The reply says whose project it is part of; the step stays hidden from `next` because its ancestor is claimed; `get` shows `claimed by <worker>` on the step and `in_progress by <worker>` in the parent's outline. Writes renew only the writer's own claims (the worker's step, the orchestrator's project). A worker's question (`ask`) inside a claimed project still shows under NEEDS YOU.
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
| `SPOKES_ALLOWED_HOSTS` | unset (any `Host`) | Comma-separated `Host` header values to accept, e.g. `spokes.lan,192.168.5.100`. `*` or unset accepts any |
| `SPOKES_ALLOWED_ORIGINS` | unset (refuse any `Origin`) | Comma-separated browser origins to accept. By default every request that carries an `Origin` header is refused (403), which blocks browser-driven attacks such as DNS rebinding. MCP clients like Claude Code and OpenCode send no `Origin` |
| `SPOKES_TZ` | `UTC` | IANA time zone name (e.g. `America/Chicago`). Decides what "today" is for due dates, `overdue` and `defer_until`. An unknown name stops startup with an error |
| `SPOKES_BACKUP_DIR` | unset (no backups) | Directory for automatic backups (created if missing), e.g. `/data/backups` inside the volume |
| `SPOKES_BACKUP_KEEP` | `14` | How many backup files to keep; older ones are deleted after each backup |
| `SPOKES_BACKUP_INTERVAL_SECS` | `86400` | **Test only.** Seconds between backups; shorten it to watch a backup happen |
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

## Deployment

The homelab runs the published image from the `spokes/` stack in the Arcane `stacks` repo (`~/dev/stacks/spokes`, `/srv/stacks/spokes` on the server; its README covers start/stop, clients and backups): `ghcr.io/brandonnorsworthy/spokes:<X.Y.Z>` pinned, port `10050` -> `8787`, UID `10010`, `./data` bind mount for the database and backups. The server never builds; it pulls the tag CI published.

## CI and releases

- `.github/workflows/ci.yml` (check name `ci`): `cargo fmt --check`, `cargo clippy --all-targets -D warnings`, `cargo test --locked`, and a non-pushing `docker build`, on every PR, push to `main` and weekly. Dependency review on PRs.
- `.github/workflows/codeql.yml`: CodeQL for Rust and for the workflows (`actions`), on PRs, `main` and weekly.
- `.github/workflows/release.yml`: on push to `main` and on `v*` tags, reruns `ci`, then builds and pushes `ghcr.io/brandonnorsworthy/spokes` (`latest` and `sha-<short>` from `main`; `X.Y.Z` and `sha-<short>` from a tag) and scans it with Trivy (HIGH/CRITICAL fail the job).
- `.github/dependabot.yml`: weekly cargo, GitHub Actions and Docker base image updates. Actions are pinned to commit SHAs.

Release process: bump `version` in `Cargo.toml` (it is what `initialize` reports as `serverInfo.version`), merge to `main`, wait for green, then tag that commit `vX.Y.Z` and push the tag. Deploy by bumping the pinned tag in `stacks/spokes/compose.yml`; roll back by pinning the previous tag.

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

## Development

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

`tests/mcp_http.rs` starts the real server on a random port with a temp database and drives `/mcp` over HTTP: the M0 skeleton (initialize, tools/list, tools/call, the 2026-07-28 stateless form, Origin refusal, restart persistence), the M1 tree (a six-step plan written and walked in three calls, depth, notes, update, move, error paths) M2 (search normalization, typos, short keywords, ranking, scope and include_done over a tree with decoys; level inheritance; focus placement and errors; due ordering and every `next` section; `SPOKES_TZ`) M3 (two clients reading `next` and claiming different tasks; claim refusals and lease renewal by every write tool; 24 concurrent claims over HTTP with exactly one winner; ask, NEEDS YOU, answer, takeover of an expired lease, `get` history; the M2 retro regression where a project and its due sub-project both pointed at the same step) and M4 (recurrence in both `from` modes with month clamping, the day-of-month anchor, late completions, the defer window, repeating checklists and the refusals; `defer_until` everywhere; STALE ordering, cap and how deferring, dropping and activity stop it; the orchestrator/worker claim pattern, including the next-step hint a worker gets inside the orchestrator's claimed ordered project; history times in `SPOKES_TZ`). `tests/backup.rs` runs the server with a 1-second backup interval and `keep: 2`, checks rotation, wipes the database, restores the newest backup and reads the data back over MCP. Unit tests cover the recurrence date arithmetic (`src/recur.rs`) and backup rotation and restore validation (`src/backup.rs`). Lease expiry and question age are tested by moving timestamps into the past through a second connection to the database file. `src/store/claims.rs` also races 8 separate SQLite connections (as separate processes would) on one claim.

### Layout

| Path | What |
| --- | --- |
| `src/main.rs` | Process entry: config, logging, signals, `healthcheck`, `backup` and `restore` subcommands |
| `src/backup.rs` | Backups (`VACUUM INTO`), rotation, the daily timer, restore |
| `src/recur.rs` | Repeat rules: parsing, next due date, defer window |
| `src/config.rs` | Environment variables |
| `src/server.rs` | axum router: `/mcp` (rmcp Streamable HTTP, stateless) and `/health` |
| `src/tools/` | MCP surface: tool definitions, argument schemas, server instructions |
| `src/store/` | One file per operation (`add`, `get`, `next`, `search`, `focus`, `status`, `update`, `moves`, `claims` for claim/release/ask/answer), `fts.rs` (keeps the search index in step), plus shared queries |
| `src/text.rs` | Search normalization, keywords and fuzzy matching |
| `src/db/` | Opening SQLite (WAL), migrations (`PRAGMA user_version` steps), root seed, fractional position keys |
| `src/model.rs`, `src/format.rs` | Domain types and the one-line-per-node text format |

Schema changes: append a step to `STEPS` in `src/db/migrations.rs` (SQL, or a Rust function for backfills); never edit a shipped step. Step 2 creates `node_fts` and backfills it; step 3 adds `nodes.answered_at`. The index is written from Rust (`store::fts::reindex`) by `add` and `update`, not by triggers, because the text is normalized in Rust; status and position live only in `nodes`, which search joins.

The MCP SDK (`rmcp`) is pinned to an exact version; read its changelog before bumping.
