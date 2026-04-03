# gosh.agent

Autonomous task executor for the GOSH AI system. Rust binary exposing MCP tools for task lifecycle management.

## Setup

```bash
cargo build --release
```

## Running

```bash
./target/release/gosh-agent \
  --port 8767 \
  --host 127.0.0.1 \
  --memory-url http://127.0.0.1:8765/mcp \
  --memory-token YOUR_TOKEN
```

Or via gosh.cli: `gosh start` (starts memory first, then agent).

If your deployment uses join tokens, prefer:

```bash
./target/release/gosh-agent --join YOUR_JOIN_TOKEN
```

## Local Development

```bash
# Run tests
cargo test -q

# Lint
cargo clippy -- -D warnings
cargo fmt --check
```

## MCP Tools

| Tool | Description |
|------|-------------|
| `agent_start` | Start a task (blocking until terminal state) |
| `agent_status` | Query task status (non-blocking) |
| `agent_create_task` | Create task via agent extraction |
| `agent_courier_subscribe` | Subscribe to courier notifications for watch mode |
| `agent_courier_unsubscribe` | Stop the active courier subscription |

## Task Lifecycle

1. **Bootstrap** — fetch task context from memory via `memory_recall`
2. **Complexity check** — evaluate complexity, select model, check budget
3. **Execution loop** — LLM calls with tool use until final answer
4. **Review** — mandatory review pass
5. **Write-back** — persist `task_result` and `task_session` to memory

## Agent Status

```bash
gosh agent planner task status task-abc123 --key my-namespace
```

Returns status, shell_spent, profile_used, backend_used, and task result/session artifacts. JSON rendering depends on the CLI version in use.

For the full telemetry contract and operator workflow, see [gosh.docs](https://github.com/Futurizt/gosh.docs).

## License

MIT. Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
