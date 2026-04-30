// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// SPDX-License-Identifier: MIT

/// Convert a *bind* host (the value `gosh agent setup --host` left in
/// `GlobalConfig`) into a *client* authority part — already URL-safe
/// — suitable for direct interpolation into
/// `format!("http://{host}:{port}")`.
///
/// Mirrors `<gosh.cli>/src/utils/net.rs::client_host_for_local`. We
/// keep the duplicate here rather than introduce a shared crate
/// because the helper is four lines of logic and the two repos have
/// no other shared utilities yet — adding that machinery for a
/// single helper would be more weight than the duplication.
///
/// Two transforms:
///
/// 1. Bind placeholders → loopback. `0.0.0.0` / `::` / `[::]` are valid bind
///    addresses meaning "listen on every interface", but not portable client
///    destinations. Map them to their loopback equivalents (`127.0.0.1` /
///    `[::1]`).
/// 2. IPv6 literals → URI-bracketed. RFC 3986 §3.2.2 requires IPv6 literals in
///    URIs to be wrapped in `[...]` so the trailing `:port` is unambiguous.
///
/// Concrete IPv4 addresses, hostnames, and already-bracketed IPv6
/// pass through unchanged.
pub fn client_host_for_local(bind: &str) -> String {
    match bind {
        "0.0.0.0" => return "127.0.0.1".to_string(),
        "::" | "[::]" => return "[::1]".to_string(),
        _ => {}
    }
    if bind.starts_with('[') {
        return bind.to_string();
    }
    if bind.contains(':') {
        return format!("[{bind}]");
    }
    bind.to_string()
}

/// True iff a daemon bound to `host` is reachable from the same
/// machine via a loopback destination. Drives the decision in
/// `gosh-agent setup` of whether to generate local-MCP configs
/// (`.mcp.json` for Claude, `claude mcp add -s user`, `codex mcp
/// add`, Gemini `settings.json`) for the coding-CLI on this host.
///
/// Background: the stdio mcp-proxy is bearerless — it relies on
/// the daemon's `/mcp` middleware bypassing the OAuth gate when
/// the request comes from a direct-loopback peer. That bypass
/// fires only if (a) the kernel actually delivers the connection
/// through a loopback interface, and (b) the request carries no
/// `X-Forwarded-*` headers (the proxy doesn't set any).
///
/// - Unspecified binds (`0.0.0.0`, `::`, `[::]`) cover loopback too, so the
///   proxy's `client_host_for_local`-rewritten `127.0.0.1` / `[::1]` target
///   lands on the loopback interface the daemon is also listening on. Bypass
///   fires. ✓
/// - Explicit loopback binds (`localhost`, `127.x.x.x`, `::1`, `[::1]`) — same
///   story. ✓
/// - Concrete non-loopback binds (`192.168.1.50`, `2001:db8::1`,
///   `agent.internal`) — the daemon listens *only* on that interface; the proxy
///   can't dial loopback (no listener there), and dialling the concrete IP
///   makes the daemon see a non-loopback peer, which fails the bypass and
///   demands a Bearer the bearerless proxy can't supply. ✗
///
/// Setup detects the third case and refuses to write a local-MCP
/// config that would 401 on every call. Found in the
/// post-v0.8.0+1 review.
pub fn is_local_mcp_compatible_bind(host: &str) -> bool {
    // Exact-match shortcut for bind strings that have no `:port`
    // suffix shape to worry about. Covers the unspecified binds
    // (`0.0.0.0` / `::` / `[::]`) plus bare-form loopbacks
    // (`::1` — splitting that on `:` would yield an empty
    // string and miss).
    if matches!(host, "0.0.0.0" | "::" | "[::]" | "::1") {
        return true;
    }
    // Strip the optional `:port` suffix. IPv6 literals are
    // bracketed (`[::1]:8767`) so we take everything up to and
    // including `]`; for IPv4 / hostnames we split on the first
    // `:`.
    let bare = if host.starts_with('[') {
        match host.find(']') {
            Some(end) => &host[..=end],
            None => host,
        }
    } else {
        host.split(':').next().unwrap_or(host)
    };
    bare == "localhost" || bare.starts_with("127.") || bare == "[::1]"
}

#[cfg(test)]
mod tests {
    use super::client_host_for_local;

    #[test]
    fn ipv4_unspecified_rewrites_to_loopback() {
        assert_eq!(client_host_for_local("0.0.0.0"), "127.0.0.1");
    }

    #[test]
    fn ipv6_unspecified_rewrites_to_bracketed_loopback() {
        assert_eq!(client_host_for_local("::"), "[::1]");
        assert_eq!(client_host_for_local("[::]"), "[::1]");
    }

    #[test]
    fn ipv6_loopback_gets_bracketed_for_url_safety() {
        assert_eq!(client_host_for_local("::1"), "[::1]");
    }

    #[test]
    fn concrete_ipv6_gets_bracketed() {
        assert_eq!(client_host_for_local("2001:db8::1"), "[2001:db8::1]");
    }

    #[test]
    fn already_bracketed_ipv6_passes_through() {
        assert_eq!(client_host_for_local("[::1]"), "[::1]");
        assert_eq!(client_host_for_local("[2001:db8::1]"), "[2001:db8::1]");
    }

    #[test]
    fn ipv4_concrete_hosts_pass_through_untouched() {
        assert_eq!(client_host_for_local("192.168.1.50"), "192.168.1.50");
        assert_eq!(client_host_for_local("127.0.0.1"), "127.0.0.1");
    }

    #[test]
    fn hostnames_pass_through_untouched() {
        assert_eq!(client_host_for_local("localhost"), "localhost");
        assert_eq!(client_host_for_local("agent.example.com"), "agent.example.com");
    }

    use super::is_local_mcp_compatible_bind;

    #[test]
    fn local_mcp_compatible_for_unspecified_binds() {
        assert!(is_local_mcp_compatible_bind("0.0.0.0"));
        assert!(is_local_mcp_compatible_bind("::"));
        assert!(is_local_mcp_compatible_bind("[::]"));
    }

    #[test]
    fn local_mcp_compatible_for_explicit_loopback_binds() {
        assert!(is_local_mcp_compatible_bind("localhost"));
        assert!(is_local_mcp_compatible_bind("127.0.0.1"));
        assert!(is_local_mcp_compatible_bind("127.255.255.254"));
        assert!(is_local_mcp_compatible_bind("::1"));
        assert!(is_local_mcp_compatible_bind("[::1]"));
        // With port suffix.
        assert!(is_local_mcp_compatible_bind("localhost:8767"));
        assert!(is_local_mcp_compatible_bind("127.0.0.1:8767"));
        assert!(is_local_mcp_compatible_bind("[::1]:8767"));
    }

    #[test]
    fn local_mcp_incompatible_for_concrete_non_loopback_binds() {
        // Regression: post-v0.8.0+1 review found that
        // `gosh-agent setup --host 192.168.1.50` produced an MCP
        // config that the bearerless mcp-proxy couldn't use —
        // /mcp middleware demands Bearer for non-loopback peers,
        // proxy has none. Setup must refuse to write the config
        // for this bind shape; the helper drives that refusal.
        assert!(!is_local_mcp_compatible_bind("192.168.1.50"));
        assert!(!is_local_mcp_compatible_bind("192.168.1.50:8767"));
        assert!(!is_local_mcp_compatible_bind("10.0.0.1"));
        assert!(!is_local_mcp_compatible_bind("agent.internal"));
        assert!(!is_local_mcp_compatible_bind("agent.example.com:8767"));
        assert!(!is_local_mcp_compatible_bind("[2001:db8::1]"));
        assert!(!is_local_mcp_compatible_bind("[2001:db8::1]:8767"));
        // Public IPs likewise.
        assert!(!is_local_mcp_compatible_bind("203.0.113.5"));
    }
}
