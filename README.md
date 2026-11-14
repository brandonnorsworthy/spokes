# Spokes Todo MCP

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
| M4, time and reach | Recurrence, `defer_until`, staleness, nightly backup. Done when a restore from backup works | Not started (**current**) |
| After v1 | Auth and multiple users (OAuth, `user_id` already on every row), links (blocks/related), notifications, read-only web view, semantic search | Not started |

### What works today (M0 to M3)

All twelve tools: `next`, `get`, `search`, `add`, `update`, `set_status`, `move`, `focus`, `claim`, `release`, `ask`, `answer`.

- `add`: one node, or a whole plan in one call with nested `children` (any depth up to 8, at most 200 nodes, one transaction). Returns one line with every new id, e.g. `Added SPOKE-5 task  Deploy  (under SPOKE-4 Skyforge), with 6 beneath it in the order given: SPOKE-6..SPOKE-11`. A bad child names itself (`children[3] ("Push"): ...`) and nothing is written.
- `get`: the node's header line, its breadcrumb (`Hobbies / Programming / Skyforge`), who holds it (`claimed by opus-red until 14:20`, `stalled: claim by ... expired ...`, `waiting on the user: "..." (asked by sonnet-2)`, `answered 14:02, ready to claim`) and an outline `depth` levels deep (default 1, max 10), indented per level. Ordered children are numbered and the actionable step is marked `<- next`; unordered children are bulleted. The outline is capped at 60 lines and ends with `(+N more ...)` lines for what was cut by the cap or lies below the depth, plus a count of hidden dropped nodes. A child whose due date is later than a parent's shows `(after parent's Nov 20)`. `notes: true` appends the body; `history: true` appends the node's last 10 events, newest first (`14:02  claimed by opus-red: until ...`).
- Levels: `priority` 1 to 4 on any node; unset nodes inherit the nearest ancestor's level. `get` shows a node's own level; `next` and `search` show the effective one.
- `update`: title, kind, ordered, priority (`0` clears), due / defer_until (`""` clears), aliases (`[]` clears), `notes` (replace; `""` clears) or `notes_append` (adds a paragraph). Kind changes keep the rule that areas only live under areas and never finish.
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
  - Refused, with what to do next: areas and the root, done/dropped tasks, waiting tasks (answer first), a task held by another agent with a live lease (`SPOKE-142 is claimed by opus-red until 14:20. Pick another task, or ask the user.`, times in `SPOKES_TZ`), a task inside another agent's claimed subtree, or one with a step beneath it claimed by another agent. The holder may claim steps inside its own claimed project.
  - Claiming again as the same agent renews (`Renewed ...`); claiming a lapsed lease takes it over and logs `took over from <agent>, whose lease expired ...`.
  - Any write (`add`, `update`, `move`, `set_status`, `focus`) passing the holder's `agent` on the node or beneath it renews the lease to at least 60 minutes from now (a longer lease is kept; a lapsed claim the agent still holds is revived).
  - Expiry is evaluated when `next` or `get` is read; there is no background job. A lapsed claim shows under NEEDS YOU as `stalled  Title  (claim by opus-red expired 2h ago)`.
  - `release(id, agent, note)` puts it back to `open`; the note is appended to the notes (`Released by opus-red (2026-11-14): ...`) and logged. Only the holder can release a live claim; anyone can release a lapsed one. The asking agent can also withdraw its question this way.
  - `set_status done/dropped/open` clears any claim, question and answered mark.
- Questions (`ask`, `answer`):
  - `ask(id, agent, question)` sets `waiting` with the question in `waiting_on`; NEEDS YOU shows `SPOKE-88  waiting  Set up emulator: "Install to D: or E:?" (sonnet-2, 3h ago)` (age from the `asked` event). Refused on areas, closed tasks, another agent's live claim or another agent's open question.
  - `answer(id, text)` appends `Question from sonnet-2 (date): ...` / `Answer from the user (date): ...` to the notes (searchable), clears the question and claim, and reopens the task.
  - Design choice for "back near the top": an answered task stays under NEEDS YOU, after questions and stalled claims, as `answered  Title  (answered 5m ago; claim it and read the notes)` until an agent claims it (or it is closed or asked again). It is stored as `answered_at` (schema step 3). Claiming it says to read the notes.
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
| `SPOKES_LOG` | `info` | `error`, `warn`, `info`, `debug` or `trace` |

There is no auth in v1: run it only on the LAN or a private Tailscale network.

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
cargo clippy --all-targets -- -D warnings
cargo test
```

`tests/mcp_http.rs` starts the real server on a random port with a temp database and drives `/mcp` over HTTP: the M0 skeleton (initialize, tools/list, tools/call, the 2026-07-28 stateless form, Origin refusal, restart persistence), the M1 tree (a six-step plan written and walked in three calls, depth, notes, update, move, error paths) M2 (search normalization, typos, short keywords, ranking, scope and include_done over a tree with decoys; level inheritance; focus placement and errors; due ordering and every `next` section; `SPOKES_TZ`) and M3 (two clients reading `next` and claiming different tasks; claim refusals and lease renewal by every write tool; 24 concurrent claims over HTTP with exactly one winner; ask, NEEDS YOU, answer, takeover of an expired lease, `get` history; the M2 retro regression where a project and its due sub-project both pointed at the same step). Lease expiry and question age are tested by moving timestamps into the past through a second connection to the database file. `src/store/claims.rs` also races 8 separate SQLite connections (as separate processes would) on one claim.

### Layout

| Path | What |
| --- | --- |
| `src/main.rs` | Process entry: config, logging, signals, `healthcheck` subcommand |
| `src/config.rs` | Environment variables |
| `src/server.rs` | axum router: `/mcp` (rmcp Streamable HTTP, stateless) and `/health` |
| `src/tools/` | MCP surface: tool definitions, argument schemas, server instructions |
| `src/store/` | One file per operation (`add`, `get`, `next`, `search`, `focus`, `status`, `update`, `moves`, `claims` for claim/release/ask/answer), `fts.rs` (keeps the search index in step), plus shared queries |
| `src/text.rs` | Search normalization, keywords and fuzzy matching |
| `src/db/` | Opening SQLite (WAL), migrations (`PRAGMA user_version` steps), root seed, fractional position keys |
| `src/model.rs`, `src/format.rs` | Domain types and the one-line-per-node text format |

Schema changes: append a step to `STEPS` in `src/db/migrations.rs` (SQL, or a Rust function for backfills); never edit a shipped step. Step 2 creates `node_fts` and backfills it; step 3 adds `nodes.answered_at`. The index is written from Rust (`store::fts::reindex`) by `add` and `update`, not by triggers, because the text is normalized in Rust; status and position live only in `nodes`, which search joins.

The MCP SDK (`rmcp`) is pinned to an exact version; read its changelog before bumping.
