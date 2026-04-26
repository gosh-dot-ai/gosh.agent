# PROMPT: Unify Agent Config With Memory-Owned Payloads

**Status:** Draft implementation prompt
**Date:** 2026-03-26
**Repos:** `gosh.memory` + `gosh.agent`
**Primary dependency:** [SPEC-context-v1](../../gosh.memory/specs/context/SPEC-context-v1.md) conceptually; if local relative link is not valid in tooling, use the copy in `gosh.memory`

---

## 1. Goal

Implement the missing architecture that the current system still lacks:

- `gosh.memory` already owns `recall()` / `ask()` payload assembly per `SPEC-context-v1`
- `gosh.agent` still owns a separate local profile-routing system from startup flags
- agent startup config still lives primarily in process flags instead of memory
- execution ownership is implicit in Rust code instead of explicit in persisted config
- concurrency and cooldown policy are still runtime-coded instead of declarative

The correct target architecture is:

- **memory has its own canonical memory config**
- **memory owns provider-final `payload` / `payload_meta` generation**
- **memory stores canonical per-agent configuration**
- **each agent loads its own configuration from memory**
- **agent execution behavior follows that config**
- **startup flags become bootstrap/override only**

This prompt is for implementing that architecture across **two repos**:

1. `/media/futurizt/Store/Git/gosh.memory`
2. `/media/futurizt/Store/Git/gosh.agent`

Do not keep hidden built-in profile logic as the primary source of truth.
The final architecture must have two explicit persisted config surfaces:

- one memory config for memory-owned runtime concerns
- one per-agent config for agent-owned runtime concerns

---

## 2. Non-Negotiable Architectural Rules

### 2.1 `SPEC-context-v1` is canonical for payload ownership

You must preserve the key contract from `SPEC-context-v1`:

- when profiles are configured, `recall()` returns:
  - `payload`
  - `payload_meta`
- `payload` is provider-final wire payload
- `payload_meta` is inspection-only metadata
- `ask()` is a thin wrapper around the same payload-building logic
- without profiles, `recall()` returns only context + complexity signal

Do **not** redesign this away.
The point is not to remove memory-owned payloads.
The point is to make them part of the canonical persisted config boundary.

### 2.2 Two canonical persisted configs, no hidden third source of truth

After this change there must not be:

- one hidden built-in profile system in `gosh.memory`
- another hidden built-in profile system in `gosh.agent`
- plus separate persisted config objects that are ignored in practice

There must be exactly two explicit persisted config surfaces:

- memory config
- per-agent config

Both are stored in memory and versioned explicitly.

### 2.3 Agent behavior must come from memory config

The agent must not be primarily configured by:

- `--fast-profile`
- `--balanced-profile`
- `--strong-profile`
- `--review-profile`
- `--extraction-profile`

Those may remain as bootstrap/dev overrides.
But canonical behavior must come from configuration stored in memory.

### 2.4 One canonical execution contract

The agent must not silently behave one way because the current Rust code happens to do so.

Per `SPEC-context-v1`, the canonical contract is:

- agent exact-fetches the task
- agent calls `memory recall()`
- when canonical profiles are configured, memory returns `payload` + `payload_meta`
- the agent executes using that memory-owned payload contract

Local fallback behavior may still exist for development/bootstrap when profiles are absent,
but that is not a second equal architecture.

### 2.5 Concurrency and cooldown must be declarative

Do not hardcode operational policy into invisible Rust-only rules.

The following must be representable in configuration:

- maximum parallel tasks for the agent
- maximum parallel executions per profile/backend
- cooldown after execution
- whether cooldown is per-profile or global
- whether a backend is single-flight only

Provider/runtime minimum constraints may exist.
Config may tighten them, but must not weaken them.

Safe defaults are good.
Hidden policy is not.

### 2.6 Boundary contract with `gosh.memory` must be explicit

This task must define the exact boundary between agent and memory.

At minimum specify:

- which exact MCP tools the agent uses to fetch:
  - agent config
  - memory config
  - task facts
- startup behavior if memory is unavailable
- startup behavior if canonical config is missing
- who writes initial config into memory
- config schema versioning and compatibility handling

---

## 3. Current Problems To Fix

### 3.1 Separate `memory-side profiles` concept

Today `gosh.memory` stores:

- profile level mapping
- profile configs
- `recommended_profile`
- `payload`
- `payload_meta`

But `gosh.agent` ignores that as canonical runtime ownership and instead uses local `fast / balanced / strong / review / extraction` flags.

This is the core architectural split to remove.

### 3.2 Watcher runtime still bypasses memory-owned payload execution

Today watcher execution effectively does:

- exact task fetch
- `memory recall()`
- read `complexity_hint`
- local Rust routing
- local API/CLI execution

That means the runtime is not actually aligned with `SPEC-context-v1`.

### 3.3 Agent config is not memory-backed

Today join/bootstrap only gives:

- memory URL
- token
- TLS pinning

The actual execution config still lives in process flags and defaults.
That must change.

### 3.4 Artifact contract is too weak

`task_result` and `task_session` are currently mostly text carriers with thin linkage.
That is not enough for a clean long-term swarm runtime.

This task must begin fixing that.

---

## 4. Required End State

### 4.1 Canonical persisted agent config

Each agent identity must have exact config stored in memory.

Minimum required information:

```json
{
  "kind": "agent_config",
  "target": ["agent:planner"],
  "metadata": {
    "schema_version": 1,
    "agent_id": "planner",
    "swarm_id": "swarm-alpha",
    "key": "planner-e2e",
    "enabled": true,
    "fast_profile": "claude_code_cli",
    "balanced_profile": "codex_cli",
    "strong_profile": "gemini_cli",
    "review_profile": "anthropic_sonnet_api",
    "extraction_profile": "anthropic_haiku_api",
    "max_parallel_tasks": 4,
    "global_cooldown_secs": 0
  }
}
```

The exact field names may differ, but these capabilities must exist.

### 4.2 Per-agent execution config must include agent-owned profile definitions

Do not introduce a separate global model-profile registry for this task.

Instead, the canonical per-agent config must contain everything that agent
needs for execution/review/fallback routing, including:

- profile bindings for:
  - `fast`
  - `balanced`
  - `strong`
  - `review`
  - `extraction`
- profile definitions for the profiles that agent may actually invoke
- provider/backend capabilities
- concurrency policy
- cooldown policy
- provider minimums / safety constraints

Example shape:

```json
{
  "kind": "agent_config",
  "target": ["agent:planner"],
  "metadata": {
    "schema_version": 1,
    "agent_id": "planner",
    "swarm_id": "swarm-alpha",
    "key": "planner-e2e",
    "enabled": true,
    "profiles": {
      "fast": "claude_code_cli",
      "balanced": "codex_cli",
      "strong": "gemini_cli",
      "review": "anthropic_sonnet_api",
      "extraction": "anthropic_haiku_api"
    },
    "profile_configs": {
      "claude_code_cli": {
        "backend_type": "cli",
        "provider_family": "anthropic",
        "model_id": "claude-code",
        "max_concurrency": 1,
        "cooldown_secs": 600,
        "cooldown_scope": "global_cli",
        "provider_min_cooldown_secs": 600
      },
      "codex_cli": {
        "backend_type": "cli",
        "provider_family": "openai",
        "model_id": "codex-cli",
        "max_concurrency": 1,
        "cooldown_secs": 600,
        "cooldown_scope": "global_cli",
        "provider_min_cooldown_secs": 0
      }
    }
  }
}
```

Deployment-specific details such as:

- binary path
- wrapper path
- args prefix

must not live in canonical agent config profile objects.
Those belong to local agent runtime/deployment config on the machine that runs the agent.

### 4.3 Canonical execution ownership

The agent must use one canonical execution contract driven by memory-owned payloads.

Required behavior:

- the agent exact-fetches the authoritative task fact
- the agent calls `memory recall()`
- if canonical profiles are configured, the agent consumes `payload` + `payload_meta`
- local fallback when `payload` is absent is allowed only as bootstrap/dev compatibility

Do not leave “recall + local routing” as the hidden primary architecture.

### 4.4 Structured artifact contract

Move `task_result` / `task_session` toward structured artifacts.

Minimum result fields:

- `task_fact_id`
- `task_id`
- `status`
- `profile_used`
- `backend_used`
- `summary`
- `result_text`
- `error`
- `started_at`
- `finished_at`

Minimum session fields:

- `task_fact_id`
- `task_id`
- `status`
- `phase`
- `iteration`
- `shell_spent`
- `profile_used`
- `backend_used`

Session/status details must not be encoded only in human-readable text.

### 4.5 Explicit boundary contract

You must define the exact fetch/update surface between repos.

At minimum:

- task fetch:
  - exact `memory_get(fact_id=...)`
  - target-aware `memory_query(...)` fallback by `metadata.task_id`
- config fetch:
  - exact structured query for `kind = "agent_config"`
  - exact `memory_get_config(...)` for memory-owned config

Startup failure behavior:

- if memory is unavailable at startup -> fail fast
- if memory is available but canonical config is missing:
  - use explicit bootstrap compatibility path
  - log loudly
  - do not silently pretend config exists

Initial config writing:

- must be supported by an operator-facing path
- likely via `gosh.cli`
- not by manual ad hoc fact editing

Schema versioning:

- canonical config objects must carry `schema_version`
- incompatible version -> explicit startup error, not silent partial load

---

## 5. Repo 1: `gosh.memory` Required Changes

Work in:

- `/media/futurizt/Store/Git/gosh.memory`

### 5.1 Replace isolated profile storage with canonical memory config + agent config storage

Refactor the current profile persistence model so it can store:

- canonical memory config
- canonical agent configs

Do **not** remove `SPEC-context-v1` behavior.
Instead, make `recall()` and `ask()` read from canonical memory config.

### 5.2 Keep `payload` / `payload_meta` contract intact

`recall()` must continue to:

- build provider-final payloads
- return `payload_meta`
- expose `recommended_profile`

But profile resolution must now come from canonical memory config.

### 5.3 Add exact config/profile management surface

Add or extend exact config APIs for:

- set/get canonical memory config
- set/update agent config
- get/list agent config

This must be exact and structured, not semantic.

Exact API naming can vary, but it must be operator-usable and agent-usable.

If reusing existing tools, document the exact filter/shape contract.
If adding dedicated tools, keep them explicit and exact.

### 5.4 Keep no-profiles fallback

Preserve the existing fallback contract:

- if no effective profiles exist, `recall()` still returns context + complexity only
- `payload` / `payload_meta` stay absent

### 5.5 Expose execution-related metadata cleanly

`payload_meta` should remain inspection-only, but it must contain enough information for the agent to behave deterministically when executing from `recall()`.

At minimum, make sure the agent can reliably read:

- `profile_used`
- `provider`
- `provider_family`
- `use_tool`
- token/budget estimates

---

## 6. Repo 2: `gosh.agent` Required Changes

Work in:

- `/media/futurizt/Store/Git/gosh.agent`

### 6.1 Load canonical config from memory at startup

Agent startup flow must become:

1. bootstrap connection identity from flags / join token
2. connect to memory
3. load canonical agent config from memory
4. compute effective runtime config from:
   - memory-owned config
   - optional local override flags
5. run using the effective config

Flags remain bootstrap/dev overrides only.

### 6.2 Replace local-only profile ownership

Refactor `AgentConfig` / execution profile resolution so the agent no longer assumes Rust builtins are the only source of truth.

Persisted per-agent config from memory must be supported as effective runtime config.

### 6.3 Replace hidden local routing with memory-owned payload execution

Refactor the watcher/runner code so canonical execution follows the
`SPEC-context-v1` contract.

Required:

- no more hidden “watcher runtime = recall + local routing” primary architecture
- when `recall()` returns `payload` / `payload_meta`, the agent must use them as the canonical inference input
- local fallback without payload may remain only as explicit compatibility/bootstrap path

### 6.4 Make concurrency/cooldown read from config

Replace hardcoded operational policy with effective-config-driven enforcement.

This includes:

- task-level parallelism limit
- profile/backend-level parallelism
- cooldown behavior
- cooldown scope

Hard safety constraints may still exist for certain backends, but those constraints must be represented through the same effective config model.

Provider minimums win over weaker config:

- config may tighten provider limits
- config may not weaken provider minimums

### 6.5 Begin structured artifact migration

Refactor result/session persistence so canonical artifacts carry structured outcome fields.

Human-readable summary text can remain, but must stop being the only machine-meaningful content.

### 6.6 Migration path is required

Do not make this a flag day with no operator path.

You must provide a migration path for current startup-flag-driven agents.

At minimum support one of:

- explicit `gosh.cli` config bootstrap command
- explicit `gosh.agent` migrate/bootstrap command
- first-start bootstrap flow that writes canonical config into memory

This path must be documented and testable.

---

## 7. Shared Design Constraints

### 7.1 Do not create another parallel abstraction layer

Do not solve this by introducing:

- one new “agent runtime config”
- plus old memory profiles
- plus built-in Rust profiles

There must be two explicit persisted sources of truth:

- memory config for memory-owned behavior
- per-agent config for agent-owned behavior

There must not be a hidden third built-in source of truth that silently wins.

### 7.2 Builtins may remain only as bootstrap defaults

Built-in defaults are acceptable as:

- fallback seed values
- local dev defaults
- bootstrap path when memory has no config yet

They must not remain the primary architecture.

### 7.3 Preserve targeted task contract

Do not regress the current targeted task model:

- top-level `kind = "task"`
- top-level `target = ["agent:<id>"]`
- stable external ID in `metadata.task_id`
- exact task resolution by persisted fact ID or target-aware external task ID lookup

### 7.4 Preserve current context-spec contract

Do not break `SPEC-context-v1` by:

- removing provider-final payloads
- downgrading `payload` into a provider-neutral intermediate object
- moving inspection fields into wire payload

The point is unification, not rollback.

---

## 8. Non-Goals

Do not solve these in this task:

- full planner decomposition
- distributed leases / CAS / fencing
- blockchain / inter-swarm trust
- perfect final artifact schema for every possible workflow
- total replacement of current task protocol

This task is about making the current runtime coherent.

---

## 9. Budget Semantics

Concurrency policy and budget policy are related but not the same.

For this task:

- each task keeps its own existing per-task `budget_shell`
- concurrency limits control how many tasks may run concurrently
- this task does **not** need to introduce a shared aggregate budget pool

Do not imply that `max_parallel_tasks` creates global budget enforcement unless
you actually implement one.

---

## 10. Test Strategy

This task must include explicit tests.

At minimum:

- unit tests:
  - config schema validation
  - profile schema validation
  - provider-minimum cooldown enforcement
  - startup config resolution precedence
- integration tests:
  - memory stores and returns canonical memory config
  - memory stores and returns canonical agent config
  - agent startup fetches config from memory
  - agent runs using memory-owned payload when available
  - local bootstrap fallback path works when config is absent
- migration tests:
  - current startup flags can be converted into canonical memory config
  - version mismatch fails explicitly

---

## 11. Acceptance Criteria

This task is done when:

1. canonical agent config is stored in memory and loaded by the agent
2. canonical memory config is stored in memory and used by `recall()` / `ask()`
3. per-agent execution config is stored in memory and used by the agent
4. `SPEC-context-v1` payload ownership remains intact
5. agent startup has an explicit boundary contract for config fetch and failure behavior
6. canonical runtime uses memory-owned payload execution when profiles are configured
7. local startup flags are no longer the primary source of execution truth
8. concurrency/cooldown policy is loaded from effective config and cannot weaken provider minimums
9. canonical `task_result` and `task_session` carry structured machine-readable fields, not only human-readable text
10. an operator-usable migration/bootstrap path exists
11. unit and integration tests cover the new config/runtime boundary

---

## 12. Suggested Implementation Order

Implement in this order:

1. define canonical persisted schemas for:
   - memory config
   - agent config
2. implement config storage + retrieval in `gosh.memory`
3. refactor `recall()` / `ask()` to read canonical memory config
4. define bootstrap/migration path from startup flags into memory config
5. refactor `gosh.agent` startup to load effective config from memory
6. refactor `gosh.agent` runtime to use memory-owned payload execution
7. refactor concurrency/cooldown enforcement to read config
8. upgrade canonical result/session artifacts

Do not start by patching random runtime behavior first.
Fix ownership and config loading first.
