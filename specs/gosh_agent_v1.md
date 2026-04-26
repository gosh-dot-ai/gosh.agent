# SPEC: GoshAgent v1.0

**Status:** Draft
**Date:** 2026-03-16
**Repository:** github.com/gosh-runtime/gosh.agent (AGPL-3.0-only)
**Depends on:** SPEC-memory-mcp-v1.6 (gosh.memory MCP interface), SPEC-context-v1 (ready payload from recall), Toolbox MCP (optional)
**License:** AGPL-3.0-only
**Copyright:** 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.

---

## 1. Overview

GoshAgent is an autonomous task executor exposed via MCP. It receives an agent identity plus a task reference, loads its per-agent config from memory, resolves the authoritative task fact from memory, retrieves context from memory, executes with the appropriate configured profile, reviews its own result, and writes outputs back to memory.

GoshAgent does not accumulate conversation history. It does not manage a context window. Each session is a complete, self-contained execution cycle driven by memory.

**Three external dependencies — all via MCP:**

```
gosh.memory  →  task facts + context + complexity signal + artifact storage
Toolbox      →  optional tool execution
LLM backends →  API models and official coding CLIs, routed by per-agent config plus `memory_recall` payload metadata
```

---

## 2. Architecture

```
Caller
  │
  │  MCP: agent_start(agent_id, swarm_id, task_id, budget_shell, connection_id?)
  ▼
┌─────────────────────────────────────────────────────┐
│                    GoshAgent                         │
│                                                      │
│  ┌──────────────┐    ┌─────────────────────────┐    │
│  │   Session    │    │    ModelSwitchRouter     │    │
│  │   Manager   ─────▶  complexity_hint → model  │    │
│  └──────┬───────┘    └────────────┬────────────┘    │
│         │                         │                  │
│  ┌──────▼──────────────────────────▼──────────────┐  │
│  │              Execution Loop                     │  │
│  │                                                 │  │
│  │  memory_recall → model call → tool calls        │  │
│  │       └──────────────────────────────┐          │  │
│  │                              Review (mandatory)  │  │
│  │                                 │               │  │
│  │                         retry / done            │  │
│  └─────────────────────────────────────────────────┘  │
│                                                      │
│  ┌──────────────────────────────────────────────────┐ │
│  │              Budget Controller                   │ │
│  │  shell_spent tracking, hard stop at limit        │ │
│  └──────────────────────────────────────────────────┘ │
└─────────────────────────────────────────────────────┘
  │
  │  MCP response: task_id + status + shell_spent + artifacts_written
  ▼
Caller
```

---

## 3. MCP Interface

GoshAgent exposes two MCP tools and one SSE endpoint.

```
POST /mcp          →  agent_start  (initiate session, blocking until done)
POST /mcp          →  agent_status (query running session, non-blocking)
GET  /agent/sse    →  telemetry stream (ephemeral, see §6)
```

---

### 3.1 `agent_start`

Initiates an agent session. Current implementation is **blocking** for direct/manual callers. This is suitable for local/private control paths and CLI usage, but it is not a durable resumable network-session protocol.

**Request:**

```json
{
  "method": "agent_start",
  "params": {
    "agent_id":      "agent_a",
    "swarm_id":      "swarm_1",
    "task_id":       "task_xyz",
    "budget_shell":  50,
    "connection_id": "uuid-from-sse-connect"
  }
}
```

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `agent_id` | string | yes | Agent identity. Used for `agent-private` memory scope. Must be unique per agent instance within a swarm. |
| `swarm_id` | string | yes | Swarm context. Used for `swarm-shared` memory scope. |
| `task_id` | string | yes | Task reference. May be a persisted memory fact ID or an external stable task ID. Agent resolves it to the authoritative task fact before execution. |
| `budget_shell` | integer | yes | Max execution budget in SHELL. `1 SHELL = $0.01` at maximum model rate. Agent will never exceed this. Min: 1. |
| `connection_id` | string | no | SSE connection ID obtained from `GET /agent/sse`. If absent — telemetry silently disabled, session runs normally. |

**Response (terminal — any status):**

```json
{
  "task_id":           "task_xyz",
  "status":            "done",
  "shell_spent":       12,
  "artifacts_written": ["art_xxx", "art_yyy"]
}
```

| Field | Type | Description |
|-------|------|-------------|
| `task_id` | string | Echo of input `task_id` |
| `status` | enum | Terminal status. See §3.3. |
| `shell_spent` | integer | Total SHELL consumed this session. Always `≤ budget_shell`. |
| `artifacts_written` | string[] | IDs of canonical artifacts written to memory during this session. |

---

### 3.2 `agent_status`

Query the current state of a running session. **Non-blocking.** Returns immediately.

**Request:**

```json
{
  "method": "agent_status",
  "params": {
    "task_id": "task_xyz"
  }
}
```

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `task_id` | string | yes | Session to query |

**Response (running):**

```json
{
  "task_id":      "task_xyz",
  "state":        "running",
  "phase":        "execution",
  "iteration":    3,
  "shell_spent":  8,
  "shell_budget": 50,
  "model_current": "claude-sonnet-4-6"
}
```

**Response (terminal):**

```json
{
  "task_id":    "task_xyz",
  "state":      "done",
  "shell_spent": 12
}
```

| Field | Type | Description |
|-------|------|-------------|
| `task_id` | string | Echo of input |
| `state` | enum | `running` \| `done` \| `partial_budget_overdraw` \| `too_complex` \| `failure` |
| `phase` | string | Current phase if `running`: `bootstrap` \| `execution` \| `review` |
| `iteration` | integer | Current loop iteration if `running` |
| `shell_spent` | integer | SHELL consumed so far |
| `shell_budget` | integer | Total budget from `agent_start` |
| `model_current` | string | Model active in current iteration |

**Error — session not found:**

```json
{
  "error": "session not found",
  "code":  "SESSION_NOT_FOUND",
  "task_id": "task_xyz"
}
```

---

### 3.3 Status / State Enum

| Value | Terminal | Meaning |
|-------|----------|---------|
| `running` | no | Session active |
| `done` | yes | Task completed, result passes review |
| `partial_budget_overdraw` | yes | Budget exhausted before completion. Partial result in memory under `task_id`. |
| `too_complex` | yes | `complexity_score` exceeds what `budget_shell` allows. No LLM calls made. Zero SHELL spent. |
| `failure` | yes | Unrecoverable error. Error artifact written to memory under `task_id`. |

---

### 3.4 Error Codes

| Code | HTTP | When |
|------|------|------|
| `SESSION_NOT_FOUND` | 404 | `agent_status` on unknown `task_id` |
| `SESSION_ALREADY_RUNNING` | 409 | `agent_start` called with a task reference that resolves to an in-process active session |
| `INVALID_BUDGET` | 400 | `budget_shell < 1` |
| `MEMORY_UNAVAILABLE` | 503 | gosh.memory MCP unreachable at bootstrap |
| `NO_TASK_CONTEXT` | 404 | Memory returned nothing for `task_id` — caller did not place task context before calling `agent_start` |

---

### 3.5 What Caller Must Prepare in Memory

Before calling `agent_start`, caller **must** have written the authoritative task fact to gosh.memory:

| Field | kind | Required | Description |
|-------|------|----------|-------------|
| Task fact | `task` | yes | Authoritative task fact with top-level `kind = "task"`, top-level `target = ["agent:<id>"]`, and stable external `metadata.task_id`. |
| Context artifacts | any | no | Specs, decisions, constraints, prior work relevant to the task. |
| Input data | `fact` | no | Data the agent needs to process. |

If `task_id` resolves to nothing in memory → `agent_start` returns `NO_TASK_CONTEXT` immediately.

---

## 4. Session Lifecycle

### 4.1 Step 1 — Exact Task Resolve + Memory Bootstrap (no LLM, $0)

Agent first resolves the task reference to the authoritative task fact using exact memory APIs:

- `memory_get(fact_id=...)` if the reference is already a persisted fact ID
- target-aware `memory_query(...)` fallback by external `metadata.task_id`

After exact resolve, agent calls `memory_recall` using the resolved task content as the semantic query:

```json
{
  "key":       "swarm_1",
    "query":     "<resolved task fact text>",
  "agent_id":  "agent_a",
  "swarm_id":  "swarm_1",
  "query_type": "auto"
}
```

Memory returns:

```json
{
  "context":          "...",
  "retrieved_count":  N,
  "query_type":       "synthesize",
  "complexity_hint": {
    "score":   0.8,
    "signals": ["multi_hop", "cross_scope", "low_top_score"]
  }
}
```

This stage returns **two things simultaneously**:

1. **Task context** — everything relevant that memory can retrieve for the resolved task.
2. **`complexity_hint`** — by-product of the retrieval pipeline. Not a separate call. Not an LLM call. Memory computes it internally from `query_type`, retrieval paths fired, score distribution, and artifact count.

If memory config is configured per `SPEC-context-v1`, `memory_recall` may also return:

- `recommended_profile`
- `payload`
- `payload_meta`

Tool availability is **not** defined by a tool list stored under the task in memory. Tool policy belongs to runtime configuration and optional task constraints, not to an ad hoc memory-side tool catalog.

### 4.2 Step 2 — Complexity Check & Model Selection

**`too_complex` guard:**

```
estimated_effort = base_cost(complexity_score) × context_tokens
too_complex = estimated_effort > (budget_shell × 0.8)
```

`base_cost` per complexity tier (SHELL per 1K context tokens, at max configured model rate):

| complexity_score | tier | base_cost |
|---|---|---|
| 0.0 – 0.3 | `fast` | 0.03 |
| 0.3 – 0.6 | `balanced` | 0.30 |
| 0.6 – 1.0 | `strong` | 1.50 |

If `estimated_effort > budget_shell × 0.8` → return `too_complex` immediately. Zero SHELL spent. The 0.8 factor reserves 20% for review. Both `base_cost` and the 0.8 factor are configurable in runtime config.

Routing maps `complexity_hint` to **execution tiers**, not to Anthropic-only model names:

| complexity_score | query_type examples | Tier |
|---|---|---|
| 0.0 – 0.3 | `lookup`, `procedural`, `prospective` | `fast` |
| 0.3 – 0.6 | `aggregate`, `current`, `temporal` | `balanced` |
| 0.6 – 1.0 | `synthesize`, multi-hop, cross-scope | `strong` |

The actual backend/model/profile bound to `fast`, `balanced`, `strong` is configuration-driven and may be multi-provider.

### 4.3 Step 3 — Execution Loop

```
loop:
  1. Build prompt from memory context + tool results so far
  2. Check budget: shell_spent + estimated_next_call > budget_shell → partial_budget_overdraw
  3. LLM call (model selected by Router)
  4. If response contains tool calls → execute via Toolbox MCP → append results → continue loop
  5. If response contains NO tool calls → final answer produced → exit loop
```

**Stopping condition:** model response with zero tool calls = final answer. The model signals completion by not requesting any more tools. No other signal needed.

**Prompt structure (per iteration):**

```
[system]
You are an autonomous task executor. Complete the task described below.
Use the tools available to you. When you have a final answer, respond
without calling any tools.

[user]
Task context (from memory):
{memory_context}

Available tools: {runtime tool surface}

{if retry: "Previous attempt failed review: {review_reason}. Fix it."}

{if iteration > 1: "Tool results so far:\n{tool_trace}"}
```

Tool availability is runtime-owned. The agent may expose built-in memory tools and optionally Toolbox-backed tools, but it must not depend on a task-specific tool list being written into memory before startup.

SHELL cost per call:

```
shell_cost = ceil(tokens × model_rate_per_token / 0.01)
```

`model_rate_per_token` taken at **maximum rate** for the model — conservative, never over-spends.

### 4.4 Step 4 — Review (mandatory)

After the execution loop produces a result, agent runs a mandatory review pass:

1. Select review profile via configured review policy. Default rule: review should use the configured review profile, which should be same-tier-or-stronger than the execution profile.
2. Send: original task context + execution result + tool trace.
3. Review model returns: `{verdict: "ok" | "retry", reason: "..."}`.
4. If `retry` and budget remains → loop back to Step 3 with `reason` as additional context.
5. If `retry` and budget exhausted → `partial_budget_overdraw`.
6. If `ok` → proceed to Step 5.

Review profile may differ from execution profile, but review selection must be explicit in configuration rather than an implicit hardcoded provider assumption.

**Review prompt structure:**

```
[system]
You are a strict reviewer. Evaluate whether the result fully satisfies
the task requirements. Respond only with JSON:
{"verdict": "ok" | "retry", "reason": "..."}
"ok" — task is complete and correct.
"retry" — result is incomplete or incorrect. reason must explain exactly what is wrong.

[user]
Original task requirements (from memory):
{memory_context}

Execution result:
{result}

Tool trace:
{tool_trace}
```

### 4.5 Step 5 — Write to Memory & Return

All terminal outputs are written to memory as canonical artifacts linked by:

- `metadata.task_fact_id`
- `metadata.task_id`

At minimum:

- **Result artifact** — canonical `kind: task_result`
- **Session artifact** — canonical `kind: task_session`
- **Partial result** if applicable — clearly marked by status/error fields

MCP response sent to caller.

---

## 5. Budget Controller

Single source of truth for SHELL accounting within a session.

```
budget_shell:    50       ← from MCP call
shell_spent:      0       ← increments after each LLM call
shell_remaining: 50       ← checked before every LLM call
```

Hard rules:
- Every LLM call is pre-authorized: `estimated_cost ≤ shell_remaining` — if not, stop.
- Estimate uses **maximum model rate** (conservative, never over-spends).
- Review step is pre-budgeted: Router estimates review cost before starting execution loop, reserves 20% of `budget_shell`. Execution loop gets `budget_shell × 0.8`.
- If review reserve proves insufficient → `partial_budget_overdraw` (review attempted, inconclusive).

Session state is **in-process runtime state** during execution. Memory receives terminal or durable artifacts, not every ephemeral state change. This avoids extreme write amplification.

Current duplicate/session protection is therefore only process-local. Durable restart recovery, leases, and idempotent session reclamation are out of scope for this spec version.

---

## 6. Configuration (`src/agent/config.rs`)

Current implementation is Rust-based. Runtime defaults live in `src/agent/config.rs`, while the target architecture is canonical per-agent config loaded from memory plus canonical memory config loaded through `memory_get_config`.

Conceptual configuration concerns:

- routing tiers:
  - `fast`
  - `balanced`
  - `strong`
- review profile
- extraction profile
- budget policy
- concurrency policy
- backend capabilities
- CLI runtime overrides

Provider/model mapping must be provider-neutral and profile-driven.

### 6.1 API Keys

API credentials must **not** be stored as normal memory facts.

Reasons:

- secrets should not be indexed or embedded
- memory is not the right storage layer for provider API keys
- bootstrap would become circular

Credentials should come from local secret storage or explicit process injection:

- environment variables
- local secret files
- OS keychain / external secret manager

`gosh.memory` may have its own credentials for its own inference path, but that is separate from agent credential handling.

---

## 7. Telemetry

Delivered via SSE endpoint (not Courier, not stored in memory):

```
GET /agent/sse?task_id=task_xyz&connection_id=<uuid>  →  text/event-stream
```

**Ephemeral.** No persistence. Caller connects → sees live events. Caller disconnects → events are lost. Agent does not know or care if anyone is connected.

### Event Format

```json
{
  "task_id":   "task_xyz",
  "agent_id":  "agent_a",
  "swarm_id":  "swarm_1",
  "event_type": "...",
  "ts":        "2026-03-16T10:00:00Z",
  "payload":   {}
}
```

### Event Types

| event_type | payload | when |
|---|---|---|
| `session_started` | `{complexity_score, complexity_signals[], model_selected}` | after memory bootstrap |
| `model_switched` | `{from, to, reason}` | every Router decision mid-session |
| `iteration` | `{n, action, tokens_used, shell_spent_total}` | every execution loop step |
| `tool_called` | `{tool_name, shell_cost_estimate}` | before tool execution |
| `tool_result` | `{tool_name, success, tokens_returned}` | after tool execution |
| `review_started` | `{review_model}` | before review LLM call |
| `review_result` | `{verdict, reason}` | after review |
| `budget_warning` | `{shell_spent, shell_remaining, pct_used}` | at 80% budget consumed |
| `session_ended` | `{status, shell_spent, artifacts_written[]}` | on any terminal state |

---

## 8. Memory Contract

GoshAgent interacts with gosh.memory exclusively via MCP tools defined in SPEC-memory-mcp-v1.6.

**Reads:**
- `memory_get_config` — fetch canonical memory-owned config
- `memory_query` — targeted task lookup / exact structured lookup
- `memory_get` — fetch specific fact by ID
- `memory_recall` — context bootstrap and routing signal

**Writes:**
- `memory_ingest_asserted_facts` — canonical task/result/session artifacts
- `memory_store` — optional semantic context enrichment

**Scope rules inherited from gosh.memory:**
- Agent reads: `agent-private` (own) + `swarm-shared` (own swarm) + `system-wide`
- Agent writes: `agent-private` by default; `swarm-shared` only if explicitly set in task context
- External-facing agents: restricted to task-attached artifacts only (enforced by MemoryService)

Tool policy is runtime/config-owned. Memory may carry task constraints, but runtime tool availability must not depend on a per-task tool list being prewritten into memory.

---

## 9. Repository Structure

```
gosh.agent/
├── src/
│   ├── main.rs                           # startup, watch mode, server bootstrap
│   ├── watcher.rs                        # courier + poll fallback
│   ├── courier.rs                        # SSE listener and dispatch
│   ├── agent/
│   │   ├── run.rs                        # execution lifecycle
│   │   ├── router.rs                     # complexity → routing tier
│   │   ├── budget.rs                     # SHELL accounting
│   │   ├── config.rs                     # built-in/default profile config
│   │   ├── resolve.rs                    # exact task resolution
│   │   ├── cli.rs                        # official coding CLI executors
│   │   └── extract.rs                    # extraction path
│   ├── client/
│   │   └── memory.rs                     # gosh.memory MCP client wrapper
│   └── server/
│       ├── state.rs                      # in-process runtime state
│       └── handlers/mcp/
│           ├── start.rs
│           ├── status.rs
│           └── create_task.rs
│
└── tests/
    └── Rust unit/integration tests under `src/tests.rs` and module-local test blocks
```

---

## 10. What GoshAgent Does Not Do

- Does not manage conversation history — memory is the only state
- Does not accumulate context across sessions — each session is fresh
- Does not decompose tasks — caller is responsible for task atomicity
- Does not define what "done" means — the task context in memory defines success criteria
- Does not expose internal LLM calls — only telemetry SSE and final MCP response are visible
- Does not store telemetry — SSE stream is ephemeral by design
