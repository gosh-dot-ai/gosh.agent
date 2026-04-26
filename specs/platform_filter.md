# Platform Filter for Agent Setup

## Problem

`gosh-agent setup` discovers all installed coding CLIs (Claude, Codex, Gemini) and registers hooks for all of them. There is no way to limit which CLIs an agent integrates with.

Use cases:
- User wants separate agents per CLI (different memory scopes, different configs)
- User wants to disable integration with a specific CLI
- Multi-agent setup: agent-claude handles Claude, agent-codex handles Codex

## Solution

Add `--platform` flag to `gosh-agent setup` (repeatable). When specified, only register hooks and MCP proxy for the listed platforms. When omitted, keep current behavior (all detected CLIs).

### CLI interface

```bash
# Only Claude
gosh-agent setup --name myagent --platform claude

# Claude + Codex
gosh-agent setup --name myagent --platform claude --platform codex

# All detected (default, current behavior)
gosh-agent setup --name myagent
```

### Accepted values

- `claude`
- `codex`
- `gemini`

Invalid values produce an error with the list of valid platforms.

### Changes

**gosh-ai-agent:**
- `src/plugin/setup.rs` — accept `--platform` Vec, filter detected CLIs
- `src/main.rs` — add `--platform` arg to `Setup` command

**gosh-ai-cli:**
- `src/commands/agent/setup.rs` — pass `--platform` flags through to `gosh-agent setup`

### Multi-agent example

```bash
# Create two agents
gosh agent create agent-claude
gosh agent setup --platform claude

gosh agent create agent-codex
gosh agent setup --platform codex

# Start both
gosh agent --instance agent-claude start
gosh agent --instance agent-codex start
```

Each agent captures from its own CLI. Both write to memory — data is shared at the memory level (scoped by agent principal if needed).
