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
| M1, the tree | Ordered/unordered children, `move`, `update`, notes apart from listings, nested `add`, breadcrumb paths, the full compact output format. Done when a model can write and walk a six-step plan in three calls | Not started (**current**) |
| M2, finding and ranking | `search` (aliases, trigram FTS5), priority inheritance, `focus`, due dates, the full `next` ordering | Not started |
| M3, agents | `claim`, `release`, `ask`, `answer`, leases, event log surfaced. Done when two terminals asking "what's next" get different tasks | Not started |
| M4, time and reach | Recurrence, `defer_until`, staleness, nightly backup. Done when a restore from backup works | Not started |
| After v1 | Auth and multiple users (OAuth, `user_id` already on every row), links (blocks/related), notifications, read-only web view, semantic search | Not started |

### What M0 does today

- `add`: one node under a parent (default the root), with `kind`, `ordered`, `priority`, `due`, `defer_until`, `aliases`, `notes`, `agent`.
- `get`: a node, its breadcrumb and its children (`depth` up to 5, `notes: true` for the body).
- `next`: questions/stalled claims first, then top-level open tasks (not inside another task) sorted by priority, due date and age, with progress and the next step. Respects ordered parents and `defer_until`.
- `set_status`: `done`, `dropped` or `open` on several ids at once. Areas cannot be done. Suggests closing a parent when its last step is done.
- IDs are shown as `SPOKE-<n>`; `SPOKE-12`, `12` and `"12"` are all accepted.
- Every write logs an event with the optional `agent` label.

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

`tests/mcp_http.rs` starts the real server on a random port with a temp database and drives `/mcp` over HTTP (initialize, tools/list, tools/call, error paths, the 2026-07-28 stateless form, Origin refusal, restart persistence).

### Layout

| Path | What |
| --- | --- |
| `src/main.rs` | Process entry: config, logging, signals, `healthcheck` subcommand |
| `src/config.rs` | Environment variables |
| `src/server.rs` | axum router: `/mcp` (rmcp Streamable HTTP, stateless) and `/health` |
| `src/tools/` | MCP surface: tool definitions, argument schemas, server instructions |
| `src/store/` | One file per operation (`add`, `get`, `next`, `status`) plus shared queries |
| `src/db/` | Opening SQLite (WAL), migrations (`PRAGMA user_version` steps), root seed, fractional position keys |
| `src/model.rs`, `src/format.rs` | Domain types and the one-line-per-node text format |

Schema changes: append a step to `STEPS` in `src/db/migrations.rs`; never edit a shipped step.

The MCP SDK (`rmcp`) is pinned to an exact version; read its changelog before bumping.
