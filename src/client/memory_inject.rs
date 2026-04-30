// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// SPDX-License-Identifier: MIT

use serde_json::Value;

/// Check whether the MCP tool name is part of memory's data plane.
///
/// Memory tools are identified by the `memory_` prefix by convention.
/// If memory ever adds a data-plane tool without that prefix, callers
/// of this helper will silently miss injection — keep the prefix
/// convention.
pub fn is_memory_tool_name(name: &str) -> bool {
    name.starts_with("memory_")
}

/// Set `args.key` to `default_key` only if `key` is not already present.
///
/// This is the per-call-scoping primitive the daemon's `memory_*`
/// forwarder uses: an explicit `key` from the LLM wins, a missing one
/// falls back to the configured default. Agents are multi-swarm /
/// multi-namespace by design, so binding a default scope to identity
/// would lock that out — the if-absent semantics let one agent serve
/// several scopes legitimately. Operates directly on the
/// already-extracted `arguments` object so daemon code that has
/// `params.arguments` to hand doesn't need to reconstruct a request.
pub fn set_default_key_if_absent(args: &mut Value, default_key: &str) {
    if let Some(obj) = args.as_object_mut() {
        obj.entry("key".to_string()).or_insert_with(|| Value::String(default_key.to_string()));
    }
}

/// Set `args.swarm_id` to `default_swarm` only if `swarm_id` is not
/// already present. See `set_default_key_if_absent` for rationale.
pub fn set_default_swarm_id_if_absent(args: &mut Value, default_swarm: &str) {
    if let Some(obj) = args.as_object_mut() {
        obj.entry("swarm_id".to_string())
            .or_insert_with(|| Value::String(default_swarm.to_string()));
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use serde_json::Value;

    use super::is_memory_tool_name;
    use super::set_default_key_if_absent;
    use super::set_default_swarm_id_if_absent;

    #[test]
    fn is_memory_tool_name_matches_memory_prefix_only() {
        assert!(is_memory_tool_name("memory_recall"));
        assert!(is_memory_tool_name("memory_write"));
        assert!(is_memory_tool_name("memory_anything_at_all"));
        assert!(!is_memory_tool_name("agent_create_task"));
        assert!(!is_memory_tool_name("auth_login"));
        assert!(!is_memory_tool_name("courier_subscribe"));
        assert!(!is_memory_tool_name(""));
    }

    #[test]
    fn set_default_key_if_absent_does_not_overwrite_existing() {
        // Per-call scoping: an LLM-provided `key` wins. The daemon's
        // forwarder only fills in the default when the call omitted it.
        let mut args = json!({ "key": "caller-supplied", "query": "x" });

        set_default_key_if_absent(&mut args, "fallback");

        assert_eq!(args.get("key").and_then(|v| v.as_str()), Some("caller-supplied"));
    }

    #[test]
    fn set_default_key_if_absent_inserts_when_caller_omits() {
        let mut args = json!({ "query": "x" });

        set_default_key_if_absent(&mut args, "fallback");

        assert_eq!(args.get("key").and_then(|v| v.as_str()), Some("fallback"));
    }

    #[test]
    fn set_default_swarm_id_if_absent_does_not_overwrite_existing() {
        let mut args = json!({ "swarm_id": "caller-supplied", "query": "x" });

        set_default_swarm_id_if_absent(&mut args, "fallback");

        assert_eq!(args.get("swarm_id").and_then(|v| v.as_str()), Some("caller-supplied"));
    }

    #[test]
    fn set_default_swarm_id_if_absent_inserts_when_caller_omits() {
        let mut args = json!({ "query": "x" });

        set_default_swarm_id_if_absent(&mut args, "fallback");

        assert_eq!(args.get("swarm_id").and_then(|v| v.as_str()), Some("fallback"));
    }

    #[test]
    fn set_default_helpers_no_op_on_non_object_args() {
        // Defensive: if a caller passes an array or null where args
        // should be an object, we silently do nothing rather than
        // panic. The MCP server downstream will handle the malformed
        // shape with its own error response.
        let mut args = json!([1, 2, 3]);
        set_default_key_if_absent(&mut args, "fallback");
        set_default_swarm_id_if_absent(&mut args, "fallback");
        assert_eq!(args, json!([1, 2, 3]));

        let mut args = Value::Null;
        set_default_key_if_absent(&mut args, "fallback");
        assert_eq!(args, Value::Null);
    }
}
