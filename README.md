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
# Bounded for loops with range()
def graph(items: list):
    total = 0
    for i in range(len(items)):
        total = total + 1
    return total
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

## Pause / resume

Long-running graphs (loops, big input sets) can be paused at node granularity and
resumed later — even from a different process, because the full execution state is
persisted:

```
$ tinypipe-cli execute my_loop '{"x": 0}' --pause-after 3
⏸ Execution paused at 4 nodes (id: <uuid>)
$ tinypipe-cli resume <uuid>            # run to completion
$ tinypipe-cli resume <uuid> --max-nodes 2   # or resume in steps
```

Paused executions are listed by `executions list <id>` and picked up automatically
by the scheduler, which steps them forward in rounds (`--max-nodes` per round) until
they complete:

```
$ tinypipe-cli scheduler run            # finish everything in one round
$ tinypipe-cli scheduler run --max-nodes 2   # step each execution by 2 nodes/round
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
tinypipe-vm/         — DAG interpreter + pause/resume + parallel tool execution
tinypipe-scheduler/  — resumes paused executions from checkpoints
tinypipe-cli/        — binary with all commands
benches/             — baseline benchmarks
```

## State of things

- The compiler works end-to-end for the restricted subset
- The VM executes plans correctly (22 μs for a 3-node graph)
- Versioning, deploy, and rollback all work with full audit trail
- Pause/resume with persisted checkpoints, scheduler, and threaded PARALLEL blocks
- Scope isolation and subscript restrictions are enforced
- What's missing: real tool implementations (only MockToolRegistry exists), network proxy, persistent execution scheduling, tier-2/3 sandboxing (KVM fork, wasm)

## License

MIT / Apache 2.0
