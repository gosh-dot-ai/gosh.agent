# Fix: multi-agent state directory isolation

## Problem

`state_dir()` in `src/plugin/config.rs:165` returns a hardcoded path `~/.gosh/agent/` regardless of which agent instance is running. When multiple agents run simultaneously, they share the same `buffer/pending.jsonl` and `offsets/` directory.

This causes:
- **Buffer corruption** — two agents appending to the same `pending.jsonl` concurrently
- **Offset collision** — agents overwriting each other's session offsets if session IDs overlap

## Affected code

- `src/plugin/config.rs:165` — `state_dir()` returns `base_dir()` without instance name
- `src/plugin/buffer.rs:14` — `buffer_file()` uses `state_dir().join("buffer")`
- `src/plugin/offset.rs:18` — `offset_path()` uses `state_dir().join("offsets")`

## Fix

`state_dir()` must include the agent instance name:

```
~/.gosh/agent/state/{name}/buffer/pending.jsonl
~/.gosh/agent/state/{name}/offsets/{session}.json
```

### Implementation

1. **Add `--name` flag to `capture` and `replay-buffer` commands** in `src/main.rs`. Required — CLI knows the agent name and passes it.

2. **Change `state_dir()` signature** to accept instance name:

```rust
pub fn state_dir(name: &str) -> PathBuf {
    base_dir().join("state").join(name)
}
```

3. **Update callers:**
   - `buffer.rs:buffer_file()` — accept name parameter
   - `offset.rs:offset_path()` — accept name parameter
   - `capture.rs` — pass name from CLI args
   - `replay-buffer` command — pass name from CLI args

## Tests

- Two agents with different names write to separate buffer files
- Two agents with different names write to separate offset files
