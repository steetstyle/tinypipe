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

  Output: {
    "x": 42
  }

# Env variables + pre-execution env check
# `env.get` / `env.template` tools read environment variables.
# execute/resume/scheduler validate the root graph + all transitive subgraphs'
# required variables BEFORE execution; if any are missing, the run is aborted:

$ tinypipe-cli create env_root 'def graph():
    u = call("env.get", key="ROOT_REQ")
    return u'
$ tinypipe-cli create env_child 'def graph():
    d = call("env.get", key="CHILD_REQ")
    return d'
$ tinypipe-cli create env_parent 'def graph():
    c = call("subgraph:env_child")
    return call("env.get", key="ROOT_REQ")'

$ tinypipe-cli execute env_parent
✗ Missing environment variables:
  env_parent.ROOT_REQ
  env_parent → env_child.CHILD_REQ
exit 1

$ tinypipe-cli execute env_parent --env ROOT_REQ=yes --env CHILD_REQ=dbprod
✓ Execution completed

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
    r = call("http_request", target="http_request", url="https://jsonplaceholder.typicode.com/users")
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

**What's forbidden:** `import`, `class`, `lambda`, `while`, `try`/`except`, `async`, comprehensions, f-strings, walrus operator (`:=`), `global`, `nonlocal`, `yield`, `del`, decorators, nested functions, dynamic subscript keys (`items[a + b]`).

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
| `plan <id> [version] [--format text\|mermaid\|dot]` | Dump the compiled plan (mermaid/dot graphs renderable in mermaid.live / graphviz) |
| `list` | List all graphs |

IDs can be either a UUID or the graph name (the CLI resolves it).

## Real-world example: User Dashboard

A single graph that uses **all six** [jsonplaceholder](https://jsonplaceholder.typicode.com)
endpoints with a bounded loop, string concatenation, and the built-in tools:

```python
def graph(user_id: int):
    users_resp = call("http_request", target="http_request",
                      url="https://jsonplaceholder.typicode.com/users")
    users = call("json.parse", json=users_resp.body)
    user_count = call("array.len", array=users)

    posts_resp = call("http_request", target="http_request",
                      url="https://jsonplaceholder.typicode.com/posts?userId=" + user_id)
    posts = call("json.parse", json=posts_resp.body)
    post_count = call("array.len", array=posts)

    total_comments = 0
    for i in range(10):
        c_resp = call("http_request", target="http_request",
                      url="https://jsonplaceholder.typicode.com/comments?postId=" + 1 + i)
        c = call("json.parse", json=c_resp.body)
        total_comments = total_comments + call("array.len", array=c)

    albums_resp = call("http_request", target="http_request",
                       url="https://jsonplaceholder.typicode.com/albums?userId=" + user_id)
    albums = call("json.parse", json=albums_resp.body)
    album_count = call("array.len", array=albums)

    photos_resp = call("http_request", target="http_request",
                       url="https://jsonplaceholder.typicode.com/photos?albumId=1")
    photos = call("json.parse", json=photos_resp.body)
    photo_count = call("array.len", array=photos)

    todos_resp = call("http_request", target="http_request",
                      url="https://jsonplaceholder.typicode.com/todos?userId=" + user_id)
    todos = call("json.parse", json=todos_resp.body)
    todo_count = call("array.len", array=todos)
    done_count = call("array.count_where", array=todos, key="completed", value=True)

    return {"users": user_count, "posts": post_count, "comments": total_comments,
            "albums": album_count, "photos": photo_count, "todos": todo_count,
            "done": done_count}
```

The loop `for i in range(10)` compiles to a `Loop` node whose body is executed
inline by the VM; `i` is injected per iteration, so `"..." + 1 + i` resolves to
`...?postId=1` … `...?postId=10` at runtime (no f-strings — string concatenation
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
  Output: {"users": 10}
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
    resp = call("http_request", target="http_request",
                url="https://jsonplaceholder.typicode.com/users")
    items = call("json.parse", json=resp.body)
    n = call("array.len", array=items)
    return {"count": n, "items": items}

# seed_comments — input comes from the caller via kwargs
def graph(post_id: int):
    resp = call("http_request", target="http_request",
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
  Output: {"albums": 10, "comments": 50, "photos": 50, "posts": 10, "todos": 20, "users": 10}
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

## Storage

Graphs, versions, executions, and execution steps are stored in SQLite (`./tinypipe.db` by default, override with `TINYPIPE_DB` env var).

The `graphs` table tracks the current version and active deployment. Every update and rollback inserts a row into `graph_versions`, so nothing is ever overwritten or lost.

## LLM integration (optional)

Pass `--features llm` at build time to enable natural-language-to-graph:

```
cargo build --release --features llm
tinypipe-cli create --from-llm hello "return the input value as-is"
```

It checks `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, then falls back to a local Ollama instance.

## Project structure

```
tinypipe-api/        — shared types (Context, Value, GraphId, Scope)
tinypipe-compiler/   — sanitizer, transformer, validator, optimizer, codegen
tinypipe-ir/         — ExecutionPlan + CompiledPlan + FlatBuffers schema
                       + plan_dump: text/mermaid/dot renderers (CLI'den bağımsız)
tinypipe-storage/    — SQLite implementation of GraphStorage trait
tinypipe-tools/      — MockToolRegistry + built-in tools (each tool in its own file):
                       math.add/mul, string.len, echo, test.*, http_request (ureq),
                       json.parse, array.len, list.get, array.count_where,
                       postgres (blocking NoTLS) — heavy deps live only here
tinypipe-vm/         — DAG interpreter + pause/resume + parallel tool execution
tinypipe-scheduler/  — resumes paused executions from checkpoints
tinypipe-cli/        — binary with all commands (uses tinypipe-tools::default_tools)
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
- What's missing: network proxy, persistent execution scheduling, tier-2/3 sandboxing (KVM fork, wasm)

## License

MIT / Apache 2.0
