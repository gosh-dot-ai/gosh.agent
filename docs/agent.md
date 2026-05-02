<!--
  Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
  SPDX-License-Identifier: MIT
-->

# gosh.agent

`gosh-agent` runs the local agent runtime and connects it to `gosh.memory`
through the configured control plane.

## Common Commands

```bash
gosh-agent serve --help
```

Use `gosh-agent serve` for the long-running agent process. Configure it through
the CLI-generated bootstrap/config files rather than embedding tokens in shell
history or scripts.

`gosh-agent setup --log-level <error|warn|info|debug|trace>` persists the
normal daemon verbosity in the per-instance config. `RUST_LOG` still overrides
that value for targeted diagnostics. HTTP access logs are emitted through the
`gosh_agent::http` tracing target and omit headers, bodies, and query strings.
Re-running setup with only `--log-level` preserves the saved memory `key` and
`swarm_id`; use `--no-swarm` when you explicitly want agent-private capture.

## Release Contents

The public package intentionally includes this `docs/` directory, the root MIT
`LICENSE`, Rust formatting/lint configuration, and runtime source files. The
private `specs/` directory is not part of the public release artifact.
