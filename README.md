# tinypipe

Execution graphs from Restricted Python. Compile your functions into a DAG of opcodes, run them through a lightweight VM, version them, deploy them, roll them back.

## What this is

You write a Python function inside `def graph(...):` using a restricted subset of the language — no imports, no classes, no `while` loops, no comprehensions. The compiler turns it into an execution plan (a directed graph of typed nodes). The VM interprets that plan.

Why? Because if your "function" is really a DAG, you can do things you can't do with a regular function call:

- Pause execution mid-way and resume later
- Run branches in parallel and merge results
- Replace individual steps without restarting the whole thing
- Audit every single operation with its input/output context
- Fork a graph, modify it, compare results side by side

It's not a sandboxed Python runtime. It's a compiler that takes Python-shaped input and produces something else entirely.

## Quick start

```
cargo build --release
```

```bash
# Validate your code
$ tinypipe-cli check "def graph(x: int):\n    return x"
✓ Code is valid.

# Create a graph
$ tinypipe-cli create hello "def graph(x: int):\n    return x"
✓ Graph created: aabbccdd-...
  Nodes: 3, Edges: 2
  Binary: 338 bytes (bincode), 512 bytes (FlatBuffers)

# Execute it
$ tinypipe-cli execute hello '{"x": 42}'
✓ Execution completed
  Duration: 22 μs
  Nodes executed: 3

  Output: 42

# Env variables + pre-execution env check
# `env.get` / `env.template` tools read environment variables.
# execute/resume/scheduler validate the root graph + all transitive subgraphs'
# required variables BEFORE execution; if any are missing, the run is aborted.
# Realistic example — pull orders from an API (base URL from env) and persist
# them via a subgraph (DB path from env):

$ cat .env
API_BASE_URL=https://jsonplaceholder.typicode.com
ORDER_DB_PATH=./tinypipe_orders.db

# store_orders — child graph: persists the fetched orders into SQLite
$ tinypipe-cli create store_orders 'def graph(orders: list):
    db = call("env.get", key="ORDER_DB_PATH")
    call("sqlite.query", db=db, query="CREATE TABLE IF NOT EXISTS orders(id INTEGER)")
    return call("array.len", array=orders)'

# order_sync — parent: builds the API URL from env, fetches, delegates to child
$ tinypipe-cli create order_sync 'def graph():
    base = call("env.template", value="${API_BASE_URL}/users")
    resp = call("http_request", url=base)
    orders = call("json.parse", json=resp.body)
    return call("subgraph:store_orders", orders=orders)'

# Missing vars are caught before execution — including the subgraph's:
$ tinypipe-cli execute order_sync
✗ Missing environment variables:
  order_sync.API_BASE_URL
  order_sync → store_orders.ORDER_DB_PATH
exit 1

$ tinypipe-cli execute order_sync --env-file .env
✓ Execution completed
  Output: 10

# Precedence: --env overrides → --env-file → OS env.
# --no-env-check: skips the check (for graphs using dynamic keys;
# missing variables then fail at runtime).

# Update to a new version (v1 → v2 → v3)
$ tinypipe-cli update hello "def graph(x: int):\n    return x + 1"
✓ Graph updated: hello (version 2)

$ tinypipe-cli update hello "def graph(x: int):\n    return x * 2"
✓ Graph updated: hello (version 3)

# See version history
$ tinypipe-cli versions hello
Version  Active   Created              Code
------------------------------------------------------------------------------------------
  v1                      1785102466502554     def graph(x: int):
  v2                      1785102466522726     def graph(x: int):
  v3             (latest) 1785102466544555     def graph(x: int):

# Deploy a specific version
$ tinypipe-cli deploy hello 2
✓ Deployed hello (version 2)
  Status: deployed — Active version: v2

# Rollback to v1 — creates v4 with v1's code, preserves all history
$ tinypipe-cli rollback hello 1
✓ Rolled back to v1 — new version is v4
  Code: def graph(x: int):
    return x

# History is intact, nothing is deleted
$ tinypipe-cli versions hello
Version  Active   Created              Code
------------------------------------------------------------------------------------------
  v1                      1785102466502554     def graph(x: int):
  v2                      1785102466522726     def graph(x: int):
  v3                      1785102466544555     def graph(x: int):
  v4             ◄── DEPLOYED 1785102466582054     def graph(x: int):

# List all graphs
$ tinypipe-cli list
Graphs:
ID                                    Name                 Ver  Status      CodeB
------------------------------------------------------------------------------------------
b5d2b135-0ca3-4ef6-ba95-ef52110a4587  hello                v 4  deployed       30

# Inspect the compiled plan (text, default)
$ tinypipe-cli plan hello
```

```text
Graph: hello (v1)
Format: FlatBuffers (508 bytes)

Nodes (3):
  [0] "n0" op=Input
      name = x
  [1] "n1" op=Calc
      expr = x
  [2] "n2" op=Act
      type = return

Edges (2):
  0 -> 1 [data]
  1 -> 2 [data]

Metadata: version=3 max_nodes=10000 max_time_ms=30000 max_mem_bytes=10485760
```

## What you can write

The compiler accepts a restricted subset of Python. You write a single function named `graph`:

```python
# Arithmetic, comparisons, if/else
def graph(x: int, y: int):
    if x > y:
        result = "bigger"
    elif x == y:
        result = "equal"
    else:
        result = "smaller"
    return result
```

```python
# Bounded for loops with range() + string concatenation in expressions
def graph(items: list):
    total = 0
    for i in range(len(items)):
        total = total + 1
    return total
```

```python
# Calling real tools: http_request + json.parse + array.len (see "Real-world example")
def graph():
    r = call("http_request", url="https://jsonplaceholder.typicode.com/users")
    users = call("json.parse", json=r.body)
    return call("array.len", array=users)
```

```python
# Parallel branches
def graph(x: int, y: int):
    with parallel() as p:
        p.act("process", value=x)
        p.act("process", value=y)
    return 0
```

```python
# Dict/array subscript with constant keys
def graph():
    d = {"key": 42}
    return d["key"]
```

```python
# Graph metadata — META(...) at module level, before `def graph`.
# Keyword arguments only; values must be constant literals.
# Metadata is stored as JSON alongside the graph (graph metadata, not execution).
META(title="Seed dashboard", sla_ms=5000, tags=["seeds", "demo"])

def graph(user_id: int):
    return user_id
```

```python
# Logical grouping — `with GROUP("..."):` wraps a statement block into a named
# group. Groups are purely structural: they show up in plan dumps (text/summary/
# mermaid) as labeled subgraphs and are collapsed in summary views.

def graph():
    with GROUP("Seeding"):
        users = call("subgraph:seed_users")
        posts = call("subgraph:seed_posts")
    with GROUP("Output"):
        return {"users": users.count, "posts": posts.count}
```

```python
# Parallel branches — `with PARALLEL() as p:` runs each p.act(...) in its own
# thread. Branches get isolated scopes: set() inside a branch stays local,
# MERGE strategies control how variables combine (default: last writer wins).

def graph(x: int, y: int):
    with parallel() as p:
        p.act("process", value=x)
        p.act("process", value=y)
    return 0
```

**What's forbidden:** `import`, `class`, `lambda`, `while`, `try`/`except`, `async`, comprehensions, f-strings, walrus operator (`:=`), `global`, `nonlocal`, `yield`, `del`, decorators, nested functions, dynamic subscript keys (`items[a + b]`).

**Money & numbers — kuruş-int rule:** the type system is `int`/`float` only; there is no `Decimal`/money type. Money amounts are always integer **kuruş** (100 kuruş = 1 TL): store, compute and return amounts as `int` — e.g. `price = 2499` means 24.99 TL — never as `float`. If a computation involves division, round back to integer kuruş at the boundary (`total = round(total / 1.18)` is fine as long as the money value itself stays `int`; the rule is that a money value must never *be* a float). Convention: name money variables with a `_kurus` (or `_cents`) suffix so the unit is explicit, e.g. `vat_kurus`, `total_cents`.

## Architecture

```
Restricted Python
    │
    ▼
┌─────────────┐     ┌──────────────┐     ┌────────────┐
│  Sanitizer   │────▶│  Transformer  │────▶│  Validator  │
│  (AST check) │     │  (opcode DAG) │     │  (cycle,    │
└─────────────┘     └──────────────┘     │  terminal)  │
                                         └──────┬─────┘
                                                │
                                         ┌──────▼──────┐
                                         │  Codegen     │
                                         │  (Compiled-  │
                                         │   Plan +     │
                                         │   Binary)    │
                                         └──────┬──────┘
                                                │
                                         ┌──────▼──────┐
                                         │  VM          │
                                         │  (interpret) │
                                         └─────────────┘
```

The compiler pipeline:

1. **Sanitizer** — parses the code and rejects anything outside the restricted subset. Scope isolation checks warn about variables modified inside `parallel()` blocks that leak to the outer scope.

2. **Transformer** — walks the AST and produces an `ExecutionPlan`: a list of typed nodes (`Input`, `Calc`, `Decide`, `Act`, `Parallel`, `Loop`, `Merge`, ...) connected by edges. Each edge carries an optional condition and variable mappings.

3. **Validator** — checks for cycles, dangling references, terminal completeness, and edge-condition correctness.

4. **Codegen** — converts the string-based `ExecutionPlan` into a `CompiledPlan` with `u32` indices. Outputs both [bincode](https://github.com/bincode-org/bincode) and [FlatBuffers](https://flatbuffers.dev/) binaries. FlatBuffers is the canonical format.

5. **VM** — interprets the `CompiledPlan`. Walks the DAG, dispatches each node to its handler, tracks variable context with scope isolation for parallel branches. Mock tool registry included for testing.

## CLI reference

| Command | Description |
|---------|-------------|
| `check <code>` | Validate Python code without saving |
| `create <name> <code>` | Compile and save a graph (v1) |
| `create --from-llm <name> <desc>` | Generate code from natural language via LLM |
| `update <id> <code>` | Create a new version |
| `deploy <id> [version]` | Set a version as active (default: latest) |
| `rollback <id> <version>` | Restore old code as a new version |
| `versions <id>` | List all versions |
| `execute <id> <json>` | Run a graph with input |
| `execute <id> <json> --pause-after N` | Run, pause after N nodes, save a checkpoint |
| `resume <execution_id> [--max-nodes N]` | Resume a paused execution from its checkpoint |
| `scheduler run [--max-nodes N]` | Resume all paused executions (budgeted loop mode with `--max-nodes`) |
| `executions list <id>` | List executions of a graph |
| `executions show <execution_id>` | Show execution details + per-node steps with real durations |
| `plan <id> [version] [--format text\|mermaid\|dot]` | Dump the compiled plan |
| `plan <id> --view full\|summary\|layers` | View level: full DAG / grouped summary / layer overview |
| `plan <id> --direction td\|lr` | Graph direction (top-down / left-right) |
| `plan <id> --profile <name>` | Apply a role profile's view + direction |
| `report [--profile <name>] [--env KEY=V]` | Role-based portfolio report (metrics + risk signals) |
| `profiles list / show / create / delete` | Manage role profiles (6 built-in, custom allowed) |
| `tools list` | List built-in + daemon tools |
| `tools test <name> '<json args>' [--kwargs '<json>'] [--env KEY=V]` | Run a tool directly |
| `daemon status [addr]` | Daemon health + registered worker tools |
| `list` | List all graphs |

IDs can be either a UUID or the graph name (the CLI resolves it).

## Roles & profiles

tinypipe ships 6 role profiles. Each profile bundles a `view`/`direction` pair
for `plan` dumps and a `focus` list that selects which sections the `report`
command renders. Built-in profiles are seeded into storage on first use and
cannot be overwritten or deleted.

| Profile | view | direction | report focus |
|---------|------|-----------|--------------|
| `pm` | full | td | executions, tools, structure, churn |
| `ba` | summary | td | duration, structure, endpoints, churn |
| `ceo` | summary | lr | portfolio, executions, reliability |
| `architect` | layers | td | structure, subgraphs, endpoints, tools |
| `senior` (default) | full | td | structure, tools, churn, reliability |
| `devops` | full | td | reliability, endpoints, env, duration |

```bash
# Plan with a role's view — explicit flags always win
$ tinypipe-cli plan dashboard_seeds --profile ceo --format mermaid
$ tinypipe-cli plan dashboard_seeds --profile architect --view full

# Role-based report (default profile: senior)
$ tinypipe-cli report --profile devops --env-file .env
tinypipe report — DevOps
...
## Reliability
dashboard_seeds           40.0% failed (6/15)
## External Endpoints
dashboard_seeds — jsonplaceholder.typicode.com
## Env
dashboard_seeds — API_BASE_URL, ORDER_DB_PATH
    ⚠ MISSING: ORDER_DB_PATH
```

The report scans every graph's compiled plan for tool calls (histogram),
external HTTP endpoints, subgraph dependencies, env dependencies (missing
required vars are flagged when an env is supplied), plus execution stats
(counts, failure rate, avg/p95 duration) and change churn (version count,
last deploy/rollback event).

Custom profiles extend the set:

```bash
$ tinypipe-cli profiles create auditor --label "Auditor" \
    --description "License auditor" --view summary --direction lr \
    --focus portfolio,churn
$ tinypipe-cli profiles list          # builtin + custom
$ tinypipe-cli profiles show auditor
$ tinypipe-cli profiles delete auditor # built-ins are protected
```

## Real-world example: User Dashboard

A single graph that uses **all six** [jsonplaceholder](https://jsonplaceholder.typicode.com)
endpoints with a bounded loop, string concatenation, and the built-in tools:

```python
def graph(user_id: int):
    users_resp = call("http_request",
                      url="https://jsonplaceholder.typicode.com/users")
    users = call("json.parse", json=users_resp.body)
    user_count = call("array.len", array=users)

    posts_resp = call("http_request",
                      url="https://jsonplaceholder.typicode.com/posts?userId=" + user_id)
    posts = call("json.parse", json=posts_resp.body)
    post_count = call("array.len", array=posts)

    total_comments = 0
    for i in range(10):
        c_resp = call("http_request",
                      url="https://jsonplaceholder.typicode.com/comments?postId=" + i)
        c = call("json.parse", json=c_resp.body)
        total_comments = total_comments + call("array.len", array=c)

    albums_resp = call("http_request",
                       url="https://jsonplaceholder.typicode.com/albums?userId=" + user_id)
    albums = call("json.parse", json=albums_resp.body)
    album_count = call("array.len", array=albums)

    photos_resp = call("http_request",
                       url="https://jsonplaceholder.typicode.com/photos?albumId=1")
    photos = call("json.parse", json=photos_resp.body)
    photo_count = call("array.len", array=photos)

    todos_resp = call("http_request",
                      url="https://jsonplaceholder.typicode.com/todos?userId=" + user_id)
    todos = call("json.parse", json=todos_resp.body)
    todo_count = call("array.len", array=todos)
    done_count = call("array.count_where", array=todos, key="completed", value=True)

    return {"users": user_count, "posts": post_count, "comments": total_comments,
            "albums": album_count, "photos": photo_count, "todos": todo_count,
            "done": done_count}
```

The loop `for i in range(10)` compiles to a `Loop` node whose body is executed
inline by the VM; `i` is injected per iteration, so `"..." + i` resolves to
`...?postId=0` … `...?postId=9` at runtime (no f-strings — string concatenation
with `+` is the DSL idiom).

```bash
$ tinypipe-cli create dashboard "<code>"
$ tinypipe-cli execute dashboard '{"user_id": 1}'
✓ Execution completed

  Output: {
    "albums": 10,
    "comments": 50,
    "done": 11,
    "photos": 50,
    "posts": 10,
    "todos": 20,
    "users": 10
  }
```

Pause the same graph mid-flight and resume it later:

```bash
$ tinypipe-cli execute dashboard '{"user_id": 1}' --pause-after 5
⏸ Execution paused at 5 nodes (id: 3f9d...)
$ tinypipe-cli resume 3f9d...
✓ Execution completed
```

## SQLite example: persisting the dashboard

The built-in `sqlite.query` tool runs SQL against an embedded SQLite database.
`db` defaults to `:memory:` (a fresh connection per call); pass a file path to
share state across calls. `SELECT` returns an array of row objects, DDL/DML
returns `{changes, last_insert_rowid}`.

```python
def graph(user_id: int):
    users = call("json.parse", json=call("http_request",
              url="https://jsonplaceholder.typicode.com/users").body)

    # DDL/DML with a shared on-disk database (./tinypipe_dashboard.db)
    call("sqlite.query", db="./tinypipe_dashboard.db",
         query="CREATE TABLE IF NOT EXISTS users(id INTEGER, name TEXT)")
    call("sqlite.query", db="./tinypipe_dashboard.db", query="DELETE FROM users")

    for i in range(len(users)):
        u = call("list.get", array=users, index=i)
        call("sqlite.query", db="./tinypipe_dashboard.db",
             query="INSERT INTO users VALUES (" + u.id + ", 'u')")

    # Aggregation: rows come back as objects, attributes via .n
    user_rows = call("sqlite.query", db="./tinypipe_dashboard.db",
                     query="SELECT COUNT(*) AS n FROM users")
    user_count = call("list.get", array=user_rows, index=0).n

    return {"users": user_count}
```

```bash
$ tinypipe-cli execute dashboard '{"user_id": 1}'
✓ Execution completed
  Output: {
    "users": 10
  }
$ sqlite3 tinypipe_dashboard.db "SELECT COUNT(*) FROM users"   # 10 — persisted
```

Note: loop bodies may end with a bare `call(...)` — the validator treats nodes
that feed a loop body (e.g. constant `"sqlite.query"`/`db` CALCs consumed by a
body statement) as terminating through the loop's execution, and the executor
runs them once in the main pass and caches their outputs for each iteration.

## Composing graphs: subgraph calls

Graphs can call other graphs with `call("subgraph:<name>", ...)` — each call
dispatches to a different stored graph, so every subgraph is a self-contained
business unit. A seed graph per domain:

```python
# seed_users — no input needed (fetches external data)
def graph():
    resp = call("http_request",
                url="https://jsonplaceholder.typicode.com/users")
    items = call("json.parse", json=resp.body)
    n = call("array.len", array=items)
    return {"count": n, "items": items}

# seed_comments — input comes from the caller via kwargs
def graph(post_id: int):
    resp = call("http_request",
                url="https://jsonplaceholder.typicode.com/comments?postId=" + post_id)
    items = call("json.parse", json=resp.body)
    n = call("array.len", array=items)
    return {"count": n, "items": items}
```

A parent graph composes them — each call runs a different graph's business:

```python
def graph(user_id: int):
    users = call("subgraph:seed_users")
    posts = call("subgraph:seed_posts", user_id=user_id)
    albums = call("subgraph:seed_albums", user_id=user_id)
    photos = call("subgraph:seed_photos")
    todos = call("subgraph:seed_todos", user_id=user_id)
    total_comments = 0
    for i in range(10):
        p = call("list.get", array=posts.items, index=i)
        c = call("subgraph:seed_comments", post_id=p.id)   # per-post subgraph call
        total_comments = total_comments + c.count
    return {"users": users.count, "posts": posts.count, "comments": total_comments,
            "albums": albums.count, "photos": photos.count, "todos": todos.count}
```

```bash
$ tinypipe-cli create seed_users "<code>" && tinypipe-cli create seed_comments "<code>"
$ tinypipe-cli create dashboard_seeds "<parent code>"
$ tinypipe-cli execute dashboard_seeds '{"user_id": 1}'
✓ Execution completed
  Output: {
    "albums": 10,
    "comments": 50,
    "photos": 50,
    "posts": 10,
    "todos": 20,
    "users": 10
  }
```

Semantics:

- **Input**: explicit kwargs are passed to the child graph's input context (and
  override inherited caller variables); the caller's full context is inherited
  otherwise.
- **Output**: the child's `return` value becomes the call expression's value
  (`users.count` above); the child's internal variables are also merged into the
  caller's context.
- **Recursion**: nesting depth is limited by `max_recursion_depth` (default 5);
  exceeding it fails with `RecursionLimitExceeded`.
- **Cycles**: the compiler validates subgraph targets and warns about
  self-cycles/nesting depth at build time.
- Graphs without INPUT nodes are legal (seeds that only fetch external data).

## Debugging

Logging uses [`tracing`](https://docs.rs/tracing) — level is controlled by `RUST_LOG`:

```bash
$ RUST_LOG=info tinypipe-cli execute dashboard '{"user_id": 1}'     # default: compiler/storage warnings+info
$ RUST_LOG=trace tinypipe-cli execute dashboard '{"user_id": 1}'    # VM node trace: per-node execution,
                                                                    # loop dispatch/iterations, arg resolution
$ RUST_LOG=tinypipe_vm=trace tinypipe-cli execute dashboard '{"user_id": 1}'  # only the VM
```

The trace level shows exactly which node runs, which are deferred/skipped, and how
loop bodies are identified — no `eprintln!` debugging needed.

## Pause / resume

Long-running graphs (loops, big input sets) can be paused at node granularity and
resumed later — even from a different process, because the full execution state is
persisted:

```
# Pause a long-running graph after 3 nodes — a checkpoint is saved
$ tinypipe-cli execute my_loop '{"x": 0}' --pause-after 3
⏸ Execution paused at 3 nodes (id: c2a23bea-...)
  Resume: tinypipe-cli resume c2a23bea-...

# See the paused execution
$ tinypipe-cli executions list my_loop
Executions for 'my_loop':
ID                                      Status      Started                Dur(μs)  Output
----------------------------------------------------------------------------------------------------
c2a23bea-...                            paused      ...

# Resume it — run to completion
$ tinypipe-cli resume c2a23bea-...
✓ Execution completed (id: c2a23bea-...)
  Total duration: 35 μs
  Nodes executed: 15
  Output: 5

# Or resume in steps (budgeted) — still paused after the budget
$ tinypipe-cli resume c2a23bea-... --max-nodes 2
⏸ Still paused at 5 nodes (id: c2a23bea-...)
  Resume again: tinypipe-cli resume c2a23bea-...
```

Paused executions are picked up automatically by the scheduler, which steps
them forward in rounds (`--max-nodes` per round) until they complete:

```
$ tinypipe-cli scheduler run            # finish everything in one round
Scheduler run complete:
  Processed:  1
  Completed:  1
  Still paused: 0
  Failed:     0

$ tinypipe-cli scheduler run --max-nodes 2   # step each execution by 2 nodes/round
Scheduler run complete:
  Processed:  1
  Completed:  1
  Still paused: 0
  Failed:     0
```

Checkpoints are stored as a BLOB on the `executions` row (JSON-encoded `Checkpoint`
with the full context, loop state, and node bookkeeping). `resume` and the scheduler
load the plan from the execution's immutable version, so later graph edits do not
affect in-flight executions.

## Remote tools & daemon

`tinypipe-daemon` lets you extend the built-in tool set with **workers in any
language**. Workers connect **outbound** (pull model) — no open ports, no
reverse proxies. The daemon registers their tools, dispatches calls from your
graphs, and forwards results back.

```
graph (CLI) ──► daemon (gRPC, 127.0.0.1:50051) ──► worker (Rust / Go / any)
                   ▲                                       │
                   └────────── task_id matched ◄───────────┘
```

- **Outbound registration**: the worker opens a single bidi stream and sends
  its tool definitions as the first message (`registered_tools`).
- **Fail-fast**: if a worker disconnects, pending tasks fail immediately
  (`worker disconnected`) and its tools disappear from listings.
- **Per-tool timeout**: `timeout_ms` in the tool definition (0 = daemon
  default, 30s via `TINYPIPE_DAEMON_DEFAULT_TIMEOUT_MS`).
- **Keepalive**: HTTP/2 + TCP keepalive (`TINYPIPE_DAEMON_KEEPALIVE_MS`,
  default 30s) catch half-open connections.
- **Round-robin**: multiple workers for the same tool name share load.

```bash
# Start the daemon
$ tinypipe-daemon                      # env: TINYPIPE_DAEMON_ADDR (default 127.0.0.1:50051)

# Start a worker (Go example ships in examples/go-worker)
$ cd examples/go-worker && go run .
```

**Worker auth (default ON):** the daemon requires a shared API key from every
worker. If you don't set one, the daemon generates a random key and logs it at
startup — copy it to the worker via env or CLI flag:

```bash
# Option A: shared key, explicit
$ TINYPIPE_DAEMON_API_KEY=s3cret tinypipe-daemon
$ TINYPIPE_DAEMON_API_KEY=s3cret go run .        # worker env

# Option B: generated key printed in the daemon log (WARN line)
$ tinypipe-daemon
WARN tinypipe_daemon: worker auth: NO key configured — generated random key;
    workers must set TINYPIPE_DAEMON_API_KEY=4f2c1a9e-...

# Option C: disable auth entirely (open network only — never on the internet)
$ tinypipe-daemon --no-auth        # or TINYPIPE_DAEMON_NO_AUTH=1
```

The Go worker SDK passes the key with `Worker.SetAPIKey(...)`; `examples/go-worker`
reads `TINYPIPE_DAEMON_API_KEY` automatically. A worker with a wrong/missing key is
rejected with `Unauthenticated` and its tools never register.

```bash
# See the remote tools next to built-ins
$ tinypipe-cli tools list
Built-in tools (16): array.len, echo, env.get, ...
Daemon tools (2 via 127.0.0.1:50051):
  send_email — Sends an email (stub). [timeout 5000ms]
  text.reverse — Reverses a string.

# Test a remote tool directly
$ tinypipe-cli tools test text.reverse '["hello"]'
✓ text.reverse → "olleh"

# Call it from a graph (kwargs convention; args pass via tools test)
$ tinypipe-cli create e2e 'def graph(s):
    x = call("text.reverse", value=s)
    return call("text.reverse", value=x)'
$ tinypipe-cli execute e2e '{"s": "tinypipe"}'
  Output: tinypipe

# Daemon status
$ tinypipe-cli daemon status
Daemon: OK (127.0.0.1:50051)
Registered tools: 2
```

Bridge rules:

- Built-in tool names always win over remote ones (`TINYPIPE_NO_DAEMON=1`
  skips daemon registration entirely).
- CLI passes `--kwargs '{"k": "v"}'` and `--env KEY=V` through to workers.
- Worker tools are registered lazily at execute/resume time; a dead daemon
  yields an actionable error (with retry/backoff in the Go SDK).

## HTTP server (`tinypipe-server`)

Exposes the full CLI surface over REST for headless deployments (e.g. a
Raspberry Pi serving graphs to the internet). Published graphs live at the
**site root** — no `/api/publish/` prefix — while the management API sits
under `/api`.

```bash
$ TINYPIPE_SERVER_TOKEN=secret tinypipe-server
# env:
#   TINYPIPE_SERVER_ADDR   listen address (default 127.0.0.1:8080)
#   TINYPIPE_SERVER_TOKEN  bearer token for mutating endpoints (optional)
#   TINYPIPE_SERVER_AUDIT  "1" writes every execution to the DB (default off)
#   TINYPIPE_DB            SQLite path (default ./tinypipe.db)
```

**Publishing a graph** — set `http_*` keys in the graph's `META(...)`:

```python
META(title="hello", http_route="hello", http_method="GET",
     http_public="true", http_timeout_ms=0, http_cache_ttl=30)

def graph(name):
    return "hello " + name
```

Then deploy it and it's live at `GET /hello?name=world`:

```
Publishing is all-or-nothing: if ANY http_* key is present, ALL five below
must be defined — there are no defaults or fallbacks, missing any fails the
deploy with the list of missing keys.

http_route       published path at the root (leading / optional).
                 Reserved prefixes are rejected: api, healthz, assets, static.
http_method      one of "GET" | "POST" | "PUT" | "PATCH" | "DELETE" | "HEAD"
                 (no default — required). "OPTIONS" is reserved — it is served
                 automatically (see below).
http_public      "true" skips token auth (META only accepts string literals,
                 so booleans are written as "true"/"false").
http_timeout_ms  VM timeout override for this route (0 = plan default).
http_cache_ttl   response cache in seconds for idempotent methods
                 (GET/HEAD/DELETE, 0 = off). Cache hits return
                 X-Tinypipe-Cache: HIT.
```

**Inputs per method:** GET/HEAD/DELETE take inputs from query params
(`?name=world`); POST/PUT/PATCH from a JSON body (`{"a": 1, "b": 2}`).
`HEAD` executes like GET but returns headers only (no body). `OPTIONS` never
executes the graph — every published path answers CORS preflight automatically
(`204` + `Allow` + `Access-Control-Allow-*`).

**Header/cookie isolation:** published routes never read request headers.
Inputs come only from query params or the JSON body, and the environment is
always empty — `Authorization`, `Cookie`, and any other header can never reach
your graph or its tools.

**Endpoint uniqueness:** the same (path, method) pair can never be actively
published twice. Create/update → `409`; deploy/rollback to a version whose
route conflicts is also rejected `409` *before* the mutation is written.
Different methods may share a path from different graphs (`GET /items` +
`PUT /items`). Unknown `http_*` keys and invalid `http_method` fail fast —
at startup for existing graphs, or as a `400`/`409` when creating/updating.

**Endpoint map** (CLI → REST):

| CLI | Endpoint | Auth |
| --- | --- | --- |
| `check` | `POST /api/check` | token |
| `create` | `POST /api/graphs` | token |
| `update` | `PUT /api/graphs/{id}` | token |
| `deploy` | `POST /api/graphs/{id}/deploy` | token |
| `rollback` | `POST /api/graphs/{id}/rollback` | token |
| `versions` | `GET /api/graphs/{id}/versions` | open |
| `list` | `GET /api/graphs` | open |
| `execute` | `POST /api/graphs/{id}/execute` | open |
| `resume` | `POST /api/executions/{id}/resume` | token |
| `scheduler run` | `POST /api/scheduler/run` | token |
| `executions list/show` | `GET /api/executions?graph_id=`, `GET /api/executions/{id}` | token |
| `plan` | `GET /api/graphs/{id}/plan?format=&view=&direction=&profile=` | token |
| `report` | `GET /api/report?profile=&env=K=V` | token |
| `profiles` | `GET/POST /api/profiles/{name}`, `DELETE /api/profiles/{name}` | token |
| `tools list` | `GET /api/tools` | open |
| `tools test` | `POST /api/tools/test` | token |
| `daemon status` | `GET /api/daemon/status?addr=` | open |
| — | `POST /api/run` (run raw code, cached LRU) | token |
| — | `GET /healthz` | open |

Execution responses carry `{status: completed|paused|failed, execution_id,
duration_us, nodes_executed, output?, error?}`. Paused executions and the
scheduler require `TINYPIPE_SERVER_AUDIT=1` (checkpoints live in the DB);
with audit off, no request ever writes to the database — good for a
read-heavy site at the cost of execution history.

Blocking tools (`sqlite.query`, `http_request`, ...) run on a blocking
thread pool, and a fresh execution never blocks another request.

## Storage

Graphs, versions, executions, and execution steps are stored in SQLite (`./tinypipe.db` by default, override with `TINYPIPE_DB` env var).

The `graphs` table tracks the current version and active deployment. Every update and rollback inserts a row into `graph_versions`, so nothing is ever overwritten or lost. A `last_event` marker records the latest lifecycle event (`deploy: v2`, `rollback: v1`, `fork: <parent>`) and drives the risk signals in `report` (rollback counts, deployed status).

## LLM integration (optional)

Pass `--features llm` at build time to enable natural-language-to-graph:

```
cargo build --release --features llm
tinypipe-cli create --from-llm hello "return the input value as-is"
```

It checks `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, then falls back to a local Ollama instance.

## Project structure

```
tinypipe-api/        — shared types (Context, Value, GraphId, Scope, Profile)
tinypipe-compiler/   — sanitizer, transformer, validator, optimizer, codegen
tinypipe-ir/         — ExecutionPlan + CompiledPlan + FlatBuffers schema
                       + plan_view/plan_dump: semantic plan rendering
                         (text/summary/layers/mermaid/dot, role-aware)
                       + env_deps: env dependency scanning (pure IR)
tinypipe-env/        — environment providers (OS, dotenv, static) + templates
tinypipe-storage/    — SQLite implementation of GraphStorage trait
tinypipe-tools/      — MockToolRegistry + built-in tools (each tool in its own file):
                       math.add/mul, string.len, echo, test.*, http_request (ureq),
                       json.parse, array.len, list.get, array.count_where,
                       postgres (blocking NoTLS) — heavy deps live only here
tinypipe-vm/         — DAG interpreter + pause/resume + parallel tool execution
tinypipe-scheduler/  — resumes paused executions from checkpoints
tinypipe-insight/    — role profiles (6 built-in) + metrics collection + reports
tinypipe-cli/        — binary with all commands (uses tinypipe-tools::default_tools)
tinypipe-daemon/     — gRPC daemon: worker registration + tool dispatch (tonic)
tinypipe-server/     — HTTP server (axum): full CLI surface over REST + root-level
                       published routes from META http_* keys
tinypipe-proto/      — generated gRPC stubs from proto/tinypipe.proto
examples/go-worker/  — Go worker SDK + sample tools (send_email, text.reverse)
benches/             — baseline benchmarks
```

## State of things

- The compiler works end-to-end for the restricted subset
- The VM executes plans correctly (22 μs for a 3-node graph)
- Versioning, deploy, and rollback all work with full audit trail
- Pause/resume with persisted checkpoints, scheduler, and threaded PARALLEL blocks
- Scope isolation and subscript restrictions are enforced
- Real tools via `tinypipe-tools`: `http_request` (GET/POST with headers/body), `json.parse`,
  `array.len`/`list.get`/`array.count_where`, and `postgres` (query with params) —
  dispatchable from DSL with `call("http_request", url=..., ...)`; loops with `range()`
  and string concatenation (`"..." + x`) are first-class DSL features
- Remote tools: `tinypipe-daemon` + workers in any language (Go SDK in
  `examples/go-worker`), outbound bidi registration, fail-fast, per-tool timeouts,
  keepalive, round-robin; `tinypipe-cli tools list/test`, `daemon status`
- HTTP server: `tinypipe-server` — the full CLI surface over REST, root-level
  published routes via META `http_*` keys, token auth, optional audit,
  GET response caching, dynamic code endpoint, daemon/tool proxies
- What's missing: network proxy, persistent execution scheduling, tier-2/3 sandboxing (KVM fork, wasm)

## License

MIT / Apache 2.0
