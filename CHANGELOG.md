# Changelog

## [Unreleased]

## [0.8.2] - 2026-05-01

- feature: `GET /mcp` now exposes a Streamable HTTP SSE progress
  stream for MCP clients that keep a session open. The daemon binds
  `Mcp-Session-Id` to `task_fact_id` when `agent_create_task` returns
  (and when direct `agent_start` resolves a task), then emits
  structured `notifications/progress` events for the task lifecycle:
  queued, task resolution, memory recall, inference planning, backend
  selection, execution, review, persistence, and terminal completion.
  Terminal progress also covers early `busy` / `already_running`
  outcomes and bootstrap failures after task resolution, so external
  connectors and UI clients do not need to infer task state from log
  lines. The SSE route uses the same loopback-or-Bearer MCP auth
  middleware as `POST /mcp`; tests cover auth, event hub delivery,
  task/session binding, and the full HTTP path where `GET /mcp` receives
  the queued progress event produced by `POST /mcp agent_create_task`.
  Disconnected or idle SSE subscriptions are removed on stream drop, and
  unexpected post-binding execution errors now emit terminal failed
  progress before the JSON failure response is returned.

- fix: built-in `local_cli` providers no longer pass the memory/task prompt
  through subprocess argv. `claude`, `codex`, and `gemini` invocations now use
  stdin for the full prompt so private recall evidence is not exposed through
  process inspection and large context packs do not hit OS argv limits.

- feature: task execution now logs the selected inference backend at `info`
  level after memory planning resolves the task route. API-backed tasks include
  model/backend plus the secret reference name and scope; `local_cli` tasks
  include model, resolved local CLI, binary name, and workspace without logging
  secret values. The agent resolves host-local `claude` / `codex` / `gemini`
  execution itself instead of requiring memory to send binary paths or command
  arguments. This makes the memory-selected local CLI execution path visible in
  normal daemon logs. Spec:
  [`<gosh.cli>/specs/followup.md`](../gosh-ai-cli/specs/followup.md#document--surface-the-local-cli-execution-backend).

- fix: `gosh-agent setup` now preserves existing `GlobalConfig.key` and
  `GlobalConfig.swarm_id` when `--key` / `--swarm` are omitted. This keeps
  targeted updates such as `--log-level debug` from resetting capture scope;
  use `--no-swarm` to explicitly clear the saved swarm.

- feature: daemon logging is now operator-oriented by default. `GlobalConfig`
  carries a persisted `log_level` (legacy configs default to `info`), and
  `gosh-agent setup --log-level <error|warn|info|debug|trace>` updates it.
  `serve` initializes tracing from that value unless `RUST_LOG` is set, so
  one-off targeted filters still work. The HTTP router now emits structured
  access logs through target `gosh_agent::http` with `request_id`, method,
  path without query string, remote address, status, and latency; headers,
  bodies, and query strings are intentionally excluded. Each response carries
  `x-request-id` for correlation, and successful `/health` probes log only at
  `debug` to avoid noise. Spec: [`specs/daemon_logging.md`](specs/daemon_logging.md).

- bugfix: launchd/systemd autostart now writes daemon stdout and stderr to the
  same canonical `~/.gosh/run/agent_<name>.log` file that `gosh agent logs`
  tails. New setup runs no longer split output across
  `daemon.out.log` / `daemon.err.log`, so manual starts and autostart have the
  same operator-facing log path.

## [0.8.1] - 2026-05-01

- security: when `gosh-agent setup` skips local-MCP config
  generation because the daemon's bind isn't loopback-reachable
  (the post-v0.8.0+1 fix from earlier in this section), it now
  *also* strips any pre-existing `gosh-memory-{agent}` MCP entry
  for the same agent from Claude `.mcp.json` / `claude mcp add`
  user scope, the upstream `codex mcp` registration, and
  Gemini's `mcpServers` (both project + user scope, best-effort).
  Without this, an operator who re-ran setup on an instance that
  *previously* had a working loopback-compatible bind (so an MCP
  entry was written) but is now binding to a single-interface IP
  would have left the stale entry behind — Claude / Codex /
  Gemini would keep dialling the recorded loopback target until
  the operator noticed and edited the config files by hand. The
  cleanup runs in each per-CLI branch when
  `local_mcp_compatible == false`, with idempotent semantics
  (no-op when nothing was registered). One new regression test
  pins `remove_claude_mcp` strips a representative pre-existing
  entry while preserving unrelated `mcpServers` keys. Found in
  the post-v0.8.0+2 review (the prior round caught the
  fresh-install case; this round closes the
  installed-base-migration case).

- security: `gosh-agent setup` now refuses to generate local-MCP
  configs for coding CLIs (Claude / Codex / Gemini) when the
  daemon's bind host isn't reachable via loopback from the same
  machine. Pre-fix, `gosh-agent setup --host 192.168.1.50`
  emitted `mcp-proxy --daemon-host 192.168.1.50 ...` into
  `.mcp.json` / `claude mcp add` / `codex mcp add` / Gemini
  `settings.json`; the bearerless stdio mcp-proxy then dialled
  the concrete IP, the daemon saw a non-loopback peer, demanded
  a Bearer the proxy couldn't supply, and every coding-CLI tool
  call hit 401. Setup now skips the per-CLI MCP config for
  concrete non-loopback binds (capture hooks are still
  installed — they don't depend on `/mcp`) and prints an
  actionable warning suggesting `--host 0.0.0.0` (binds every
  interface, including loopback) or `--host 127.0.0.1`. New
  `is_local_mcp_compatible_bind` helper in `plugin::net`
  centralises the detection — true for unspecified
  (`0.0.0.0` / `::` / `[::]`), explicit loopback
  (`localhost` / `127.x.x.x` / `::1` / `[::1]` with optional
  `:port`), false for everything else. Three unit tests pin the
  matrix (compatible-unspecified, compatible-loopback,
  incompatible-concrete-non-loopback). Found in the
  post-v0.8.0+1 review; pairs with the agent-side mcp-proxy
  endpoint plumbing from the prior round (which fixed
  cross-instance routing for `0.0.0.0` binds but left the
  concrete-non-loopback case broken).

- security: `gosh-agent setup` now bakes the configured daemon
  host+port into the generated MCP configs (`.mcp.json` for
  Claude, `claude mcp add -s user`, `codex mcp add`, Gemini
  `settings.json`) as explicit `--daemon-host` / `--daemon-port`
  args on `gosh-agent mcp-proxy`. Without these, the proxy fell
  back to the historical `127.0.0.1:8767` defaults; on a host
  running two agents (or one agent on a non-default port) the
  coding-CLI proxy would dial the **wrong** daemon's `/mcp`,
  which then accepts the call as direct-loopback (bypassing
  Bearer) and executes under the wrong agent's namespace. As
  defence-in-depth the proxy also makes both flags optional and
  falls back to `GlobalConfig::load(--name)` when missing — old
  on-disk configs (pre-this-fix) keep working but resolve to the
  *right* daemon for the supplied `--name`. Bind→client host
  normalisation (`0.0.0.0` → `127.0.0.1`, `::` → `[::1]`, IPv6
  bracketing) is applied at both the setup-emission and proxy-
  fallback sites via a new `plugin::net::client_host_for_local`
  helper that mirrors the `<gosh.cli>` side. Eleven new unit
  tests pin the matrix (`build_mcp_proxy_args` for swarm/no-
  swarm + bind-host normalisation; `client_host_for_local` for
  loopback/concrete/bracketed). Found in the post-v0.8.0 review.

- bugfix: `/.well-known/oauth-authorization-server` issuer URL
  now falls back to `http` for **all** loopback host shapes
  (`localhost`, `127.x.x.x`, `[::1]`, with optional `:port`),
  not just `localhost*`. Default `gosh agent setup` (no
  `--host`, no TLS frontend) leaves the daemon on
  `127.0.0.1:8767`, which the previous heuristic misclassified
  as remote and advertised as `https://127.0.0.1:8767/...`. A
  client fetching that metadata over plain HTTP would then try
  TLS handshake against an HTTP socket and break discovery
  outright. New `is_loopback_host` helper centralises the
  detection (handles bracketed IPv6 + `:port` suffix); two new
  router-level tests pin the 127.0.0.1 and `[::1]` cases plus
  one helper-level test covering the full shape matrix. Found
  in the post-v0.8.0 review.

- bugfix: `serverInfo.version` in the daemon's MCP `initialize`
  response now reflects the actual `CARGO_PKG_VERSION` (sourced
  via `env!()` at compile time) rather than the historical
  hardcoded `"0.1.0"` literal. Pre-fix, every daemon build —
  including the v0.8.0 release — claimed to be 0.1.0 to
  connecting clients, which made connector-debugging needlessly
  confusing. Same fix applied to the daemon's outbound
  `clientInfo.version` when it talks to memory (parallel
  hardcode that the review flagged as the same class of stale
  metadata). Test pins both the field-population and the
  belt-and-suspenders "must not be 0.1.0" assertion. Found as
  the non-blocking note in the post-v0.8.0 review.

## [0.8.0] - 2026-04-30

- security: `POST /admin/oauth/clients` now requires the same
  non-empty + http(s)-scheme `redirect_uris` set that DCR
  `/oauth/register` already required, closing a re-review-discovered
  gap where the admin endpoint accepted `{ "name": "X" }` (with
  `redirect_uris` defaulted to `[]` via `#[serde(default)]`) and the
  `/oauth/authorize` exact-match check then refused every redirect
  URI for that client — silently producing a registered client that
  could *never* complete the authorize flow. Concretely this broke
  the documented `gosh agent setup --no-oauth-dcr` operator path,
  where the operator manually registers a client and pastes the
  id+secret into Claude.ai. The fix:
  - `POST /admin/oauth/clients` returns 400
    `invalid_redirect_uri` when `redirect_uris` is missing / empty
    or contains a malformed URI (empty, non-http(s), or with a
    fragment per RFC 6749 §3.1.2).
  - The `validate_redirect_uri` helper that 7e originally lived in
    `server/handlers/oauth/register.rs` (DCR) was moved up to
    `oauth/clients.rs` so both DCR + manual paths share one
    contract definition; pre-existing DCR tests follow it.
  - Three new regressions: admin reject empty, admin reject
    non-http, manual-register-then-authorize end-to-end (operator-
    flow happy path that the original re-review's "regression where
    a manually registered client can complete `/oauth/authorize`
    with its registered redirect URI" called out).
  - The pre-existing `admin_register_then_list_then_revoke_round_trip`
    test now uses the canonical `https://claude.ai/api/mcp/auth_callback`
    URI (the value Claude.ai actually advertises in DCR per the 7e
    log in `<gosh.cli>/specs/agent_mcp_unification.md`) so the
    happy path mirrors the production deployment shape.
  - Wire change: existing CLIs that only post `{ "name": "X" }`
    will now get a 400 with an actionable error description; this
    is a deliberate hard-fail rather than a silent dead-client.
    Coordinated CLI-side change in `<gosh.cli>` adds
    `--redirect-uri <URI>` to `gosh agent oauth clients register`.

- security: persist + exact-match-validate OAuth `redirect_uris`. Per
  the post-7e code review of the DCR + authorize surface: previously
  `POST /oauth/register` accepted `redirect_uris` and echoed them back
  in the response, but `OAuthClient` had no `redirect_uris` field and
  `GET /oauth/authorize` only checked `client_id` existence — any
  caller could substitute an attacker-controlled `redirect_uri` and
  get the authorization code delivered to an arbitrary URL
  (RFC 6749 §3.1.2.3 + RFC 7591 §2 violation). Fix: store
  `redirect_uris: Vec<String>` on `OAuthClient` (`#[serde(default)]`
  for back-compat with pre-7e on-disk records — those records'
  empty set means the operator must re-register before the authorize
  flow accepts them); DCR `register` now requires a non-empty list of
  http(s)-scheme, fragment-free URIs; `/admin/oauth/clients` admin
  register accepts an optional set; `/oauth/authorize` rejects any
  `redirect_uri` query param that isn't byte-equal to one of the
  registered values, and rejects without redirecting (per
  §4.1.2.1, no open-redirect helper). Three regression tests added:
  DCR rejects missing `redirect_uris`, DCR rejects non-http schemes,
  authorize rejects `https://evil.example/cb` for a client registered
  for `https://claude.ai/cb` — and `Location` header is asserted
  absent in the rejection.

- security: `DELETE /admin/oauth/clients/<id>` now cascade-revokes
  every refresh + access token issued to the deleted client. Previously
  only the client record was removed; the access tokens that client
  had already been issued kept passing `/mcp` until the per-token TTL
  expired (~1h), which made the admin "revoke" a UX-only operation
  for the duration of any active access window. The cascade helper
  `TokenStore::revoke_by_client` already existed (was marked
  `#[allow(dead_code)] // wired in 7d / future client-cascade work`)
  — wiring it into the admin handler closes the window. Response now
  also includes `revoked_tokens: <usize>` so operators can see how
  many records were dropped; idempotent re-call returns
  `removed: false, revoked_tokens: 0` without churning the token
  store. Regression test parallels
  `admin_tokens_revoke_kicks_remote_caller_immediately` but deletes
  the client and asserts the previously-valid bearer immediately
  returns 401 on `/mcp`.

- bugfix: `/.well-known/oauth-authorization-server` issuer URL now
  honours `X-Forwarded-Host` (preferred over `Host`, with `Host`
  fallback) in addition to `X-Forwarded-Proto` so a same-host
  reverse-proxy front (Caddy / cloudflared / Tailscale Funnel) that
  rewrites `Host` to its internal upstream value (e.g.
  `internal:8767`) still publishes the public hostname to Claude.ai.
  Previously the daemon would advertise endpoints like
  `https://internal:8767/oauth/token` to the public client, and
  Claude.ai would fail at DNS lookup — operator had to manually
  configure the proxy to preserve `Host`. The 7d router-level test
  comment claimed this contract but only asserted `https://`-prefix;
  it now asserts the full URL `https://agent.example.com/oauth/token`
  end-to-end through the router. Two new unit tests pin
  `X-Forwarded-Host` precedence and the empty-header fallback
  edge case. Honouring `X-Forwarded-Host` does not weaken security:
  the header only shapes the *advertised* metadata document; PKCE,
  PIN, and loopback gates on the token-issuance path remain unchanged.

- new `GOSH_AGENT_TEST_MODE_KEYCHAIN_DIR` env var on `gosh-agent`: when set,
  the daemon reads agent secrets (`principal_token` / `join_token`
  / `secret_key`) from `<dir>/agent_<name>.json` instead of the OS
  keychain. Mirrors the file layout the CLI's `FileKeychain`
  (used by `gosh --test-mode`) writes into, so the two sides
  agree on the on-disk schema. Solves the barebones-Linux-container
  case (DinD test harnesses, CI runners) where `keyring` silently
  fails because there's no Secret-Service / dbus / libsecret
  backend installed. Production posture (env unset → OS keychain)
  unchanged.

- `gosh-agent --version` / `-V` now print the binary version sourced
  from `CARGO_PKG_VERSION` (clap `version` derive). Previously the
  top-level CLI rejected `--version` as an unknown argument, which
  broke install-path smoke checks (`gosh-agent --version` in CI
  Dockerfiles, operator-side `gosh-agent --version` after
  `gosh setup --component agent`) and meant the binary's version
  could only be inferred from filename / `cargo --version` of the
  build host.

- harden `/mcp` Bearer middleware + `/admin/*` middleware against
  same-host reverse-proxy bypass. Tracks Commit 7d of
  `<gosh.cli>/specs/agent_mcp_unification.md` (non-localhost
  daemon binding for remote MCP). The previous bypass rule was
  "loopback peer-IP → allow"; that broke the moment a TLS
  terminator (Caddy / cloudflared / Tailscale Funnel) ran on the
  same host as the daemon, because it forwards from `127.0.0.1`
  with a bag of `X-Forwarded-*` headers — the daemon would have
  treated those as direct loopback callers and bypassed
  Bearer-on-`/mcp`, exposing the entire MCP surface to anyone
  Caddy hands a request to. The new rule, applied in both
  `mcp_auth::is_direct_loopback` (used by `/mcp`) and
  `admin::middleware::require_admin_auth` (used by
  `/admin/*`): bypass only when the peer IP is loopback **and**
  no proxy-forwarding header is present. The signal headers
  checked are `X-Forwarded-For`, `X-Forwarded-Host`,
  `X-Forwarded-Proto`, `Forwarded`, and `X-Real-IP`; we use
  presence-only (never trust their values), since presence is
  enough to know the request crossed a proxy boundary. Stdio
  `mcp-proxy` continues to work unchanged (it never sets these
  headers; direct loopback). Operators wanting to expose `/mcp`
  through Caddy / cloudflared / Tailscale will be naturally
  bounced to the OAuth path the way the design intended. Admin
  paths refuse forwarded requests outright — they are
  loopback-direct only by design, even when the daemon binds to
  `0.0.0.0`. Six new router-level tests pin the matrix:
  loopback + `X-Forwarded-For` requires Bearer, loopback +
  `X-Forwarded-Proto` requires Bearer, loopback + RFC 7239
  `Forwarded` requires Bearer, admin refuses forwarded even
  with correct token, loopback + valid Bearer + forwarded
  headers passes (Caddy → daemon happy path), and the
  `/.well-known/oauth-authorization-server` issuer URL honours
  `X-Forwarded-Proto` end-to-end through the router so
  Claude.ai sees the public `https://` URL not the internal
  `http://`. Total tests: 310/310.

- daemon startup banner now prints a prominent warning when
  binding to a non-loopback address, with a pointer to the
  operator runbook in `<gosh.cli>/docs/cli.md` for the TLS
  frontend recipes (Caddy / cloudflared / Tailscale Funnel) and
  the pre-launch security checklist. The daemon does NOT
  terminate TLS itself by design — keeps the binary small and
  leaves cert handling to battle-tested fronts. Tracks Commit 7d
  of `<gosh.cli>/specs/agent_mcp_unification.md`.

- new `src/oauth/tokens.rs` + `/oauth/token` + `/oauth/revoke` +
  Bearer middleware on `/mcp`. Closes Commit 7c of
  `<gosh.cli>/specs/agent_mcp_unification.md` — remote MCP
  callers (Claude.ai, future scripted clients) can now actually
  authenticate and use the daemon. Endpoints:

  - `POST /oauth/token` — RFC 6749 §4.1.3 + §6 token endpoint.
    Two grant types: `authorization_code` (consumes the code minted
    by `/oauth/authorize`, verifies PKCE S256 against the stored
    `code_challenge`, byte-matches `redirect_uri` against the
    `/authorize` value, marks the session `Consumed` so the same
    code can't be exchanged twice) and `refresh_token` (rotates
    per OAuth 2.1 BCP — the presented refresh is invalidated and a
    fresh access+refresh pair is issued under a new `token_id`).
    Client auth accepts both `client_secret_basic` and
    `client_secret_post` per RFC 6749 §2.3.1; HTTP Basic wins when
    both are present. All errors map to RFC 6749 §5.2 envelopes
    `{"error","error_description"}`; bad client creds get 401 +
    `WWW-Authenticate: Basic realm=`.
  - `POST /oauth/revoke` — RFC 7009 token revocation. Always
    returns 200 for any presented token shape (known or unknown,
    well-formed or not) so probing attackers can't enumerate
    valid tokens by status code; the only 401 path is "no client
    auth at all". `token_type_hint` selects the lookup order;
    refresh-token revocation cascades to every active access
    token minted from it.
  - **Bearer middleware on `/mcp`** — loopback callers (the stdio
    `mcp-proxy` spawned by Claude Code / Codex / Gemini) bypass
    Bearer unconditionally; remote callers MUST present
    `Authorization: Bearer <access_token>` matching a non-expired
    record in `TokenStore`. Missing → 401 with
    `WWW-Authenticate: Bearer realm=`; invalid → 401 with
    `error="invalid_token"` per RFC 6750 §3 (which is exactly
    what triggers Claude.ai's connector to invoke `/oauth/token`
    with its saved refresh token to mint a fresh access).
  - `GET /admin/oauth/tokens` / `DELETE /admin/oauth/tokens/<id>`
    — operator control plane. List view exposes `token_id`,
    `client_id`, `created_at`, `last_used_at`, scope, and an
    `active_access_tokens` count (so an operator can see "is
    something still connected?" without seeing the access tokens
    themselves); never `token_hash`, never plaintext. Revoke is
    by `token_id` (the operator-visible handle, surfaced in
    `gosh agent oauth tokens list`); cascades the same way the
    public revoke endpoint does — which is the operationally
    useful "boot the connected client" lever (their next `/mcp`
    request hits 401 invalid_token immediately, before the access
    TTL would have rolled them off).

  Token shapes:

  - `at_<base64>` — access token (32 random bytes, URL-safe-b64
    no pad), Bearer on `/mcp`, in-memory only, 1-hour TTL.
  - `rt_<base64>` — refresh token, persisted at
    `~/.gosh/agent/state/<name>/oauth/tokens.toml` (mode 0600,
    atomic rename) as a salted-free `sha256(plain).hex()` —
    high-entropy plaintext means a salted KDF buys nothing, the
    hash exists only to keep the on-disk file useless after the
    fact.
  - `tok_<8hex>` — operator handle, **not** a secret. Sits next
    to each refresh-token hash in `tokens.toml` and is what
    `oauth tokens list` / `oauth tokens revoke <token_id>` use.
    Operators never see or type the actual `rt_` value.

  Daemon-restart posture: every active access token drops by
  design (in-memory only) — Claude.ai's connector then hits 401
  `invalid_token`, calls `/oauth/token` with its saved refresh,
  gets a fresh access. Refresh tokens persist so the user
  doesn't have to redo the PIN dance after a restart.

  Background sweep at 60s evicts expired access tokens —
  refresh tokens have no TTL (revoked explicitly or not at all).

  Coverage: 17 unit tests on `TokenStore` (mint/verify, refresh
  rotation, cascade-revoke by id / by client / via plain refresh,
  access plain revoke leaves refresh alive per RFC 7009 §2.1,
  sweep, list-view leak guard for `token_hash` / plaintext, file
  permissions 0600, refresh-survives-reload) + 6 unit tests on
  `SessionStore::consume_authorization_code` (PKCE happy path
  with the RFC 7636 §B.2 fixture, PKCE mismatch, redirect_uri
  mismatch, client mismatch, code expiry, single-use after
  consume) + 14 router-level integration tests (auth_code happy
  path, PKCE mismatch, single-use after consume, invalid_client
  with WWW-Authenticate, refresh rotation invalidates old, revoke
  silent-200, revoke without client auth → 401, `/mcp` loopback
  bypass, `/mcp` remote 401 missing-token, `/mcp` remote 401
  invalid-token, `/mcp` remote with valid Bearer passes, admin
  tokens list strips hashes, admin revoke cascades and kicks
  remote caller immediately, admin revoke unknown is idempotent).
  Total tests: +37.

- new `src/oauth/sessions.rs` + `/oauth/authorize` consent flow.
  Tracks Commit 7b of
  `<gosh.cli>/specs/agent_mcp_unification.md`. Endpoints:

  - `GET /oauth/authorize` — RFC 6749 §4.1.1 + RFC 7636 PKCE
    request-validation (rejects `response_type≠code`,
    `code_challenge_method≠S256`, missing `code_challenge`,
    unknown `client_id`). On success allocates an in-memory
    session (10-min TTL) and renders a no-JS embedded HTML
    consent page that displays `session_id` and the exact CLI
    command an operator should run. HTML escapes all
    interpolated strings (regression test pins that an
    attacker-controlled `client_name` from DCR can't inject
    `<script>`).
  - `POST /oauth/authorize` — operator submits PIN + action.
    On `approve` + matching PIN, mints a 32-byte URL-safe-b64
    authorization code (60-second TTL) and redirects to
    `<redirect_uri>?code=…&state=…` (preserving any existing
    query string on `redirect_uri`). On `deny` drops the
    session and shows a closure page. PIN failures render a
    typed error page with retry guidance — within the 5-min
    PIN window the operator can keep typing.
  - `GET /admin/oauth/sessions` / `DELETE
    /admin/oauth/sessions/<id>` /
    `POST /admin/oauth/sessions/<id>/pin` — control plane.
    The list response strips `pin`, `authorization_code`, and
    `code_challenge` defence-in-depth (admin token alone
    must not reveal in-flight credential material).
  - Background sweep task launched in `serve()` evicts
    expired sessions every 60 seconds.

  Session store is in-memory only by design — daemon restart
  drops every pending session, so Claude.ai's flow either
  completes in one daemon-process lifetime or starts over.
  Persisting them would add a failure mode (state on disk +
  matching cleanup) without a real upside.

  PIN model: 6-digit numeric, 5-min TTL, one-time use,
  scoped to a specific session_id (re-issuing for the same
  session invalidates the prior PIN). Operator runs
  `gosh agent oauth sessions pin <session_id>` on the agent
  host; CLI calls `/admin/oauth/sessions/<id>/pin` and prints
  the PIN. Whoever holds keychain access to the state dir is
  the only one who can issue PINs.

  Coverage: 14 unit tests on `SessionStore` (CRUD, PIN
  issue/verify/expire/reissue, code mint, status state
  machine, sweep eviction, list-view leak guard) + 8
  router-level integration tests (consent rendering, PKCE
  rejection, unknown-client guard, full DCR→authorize→PIN→
  redirect happy path, wrong-PIN rejection without session
  consume, admin sessions list/drop/pin paths). Total tests:
  +25 (14 store + 8 router + 2 handler unit + 1 misc).

- new `src/oauth/` module + public OAuth 2.1 endpoints. Tracks
  Commit 7a of `<gosh.cli>/specs/agent_mcp_unification.md` —
  daemon now exposes the authorization-server side that Claude.ai's
  remote MCP connector talks to. Endpoints landing in this commit:

  - `GET /.well-known/oauth-authorization-server` — RFC 8414
    discovery metadata. Issuer URL is built from the inbound
    `Host` header (with `X-Forwarded-Proto` honoured for TLS-
    terminating reverse proxies). `code_challenge_methods_supported`
    is strictly `["S256"]` — no `plain` fallback.
  - `POST /oauth/register` — RFC 7591 Dynamic Client Registration.
    Returns `client_id` + `client_secret`; stores only a salted
    SHA-256 hash on disk. Disabled via `--no-oauth-dcr` on
    `gosh-agent setup`; when off, the endpoint returns 405 and
    metadata omits `registration_endpoint` so RFC-conformant
    clients fall back to expecting manually-issued credentials.
  - `GET / POST /admin/oauth/clients` and
    `DELETE /admin/oauth/clients/<id>` — control-plane CRUD for
    the CLI. Gated by a two-factor middleware: peer-addr must be
    loopback (even when daemon binds to `0.0.0.0` for remote
    transport in 7d), and `Authorization: Bearer <token>` must
    match the per-process admin token written to
    `~/.gosh/agent/state/<name>/admin.token` (mode 0600) at
    startup. Daemon restart rotates the token by design.

  New `GlobalConfig.oauth_dcr_enabled` (default `true`) controls
  the `/oauth/register` endpoint. `gosh-agent setup --no-oauth-dcr`
  flips it off; absence of the flag re-asserts `true` (same shape
  as `--no-autostart` — setup declares desired state every run, so
  re-running setup without the flag re-enables DCR).

  Persistent storage at `~/.gosh/agent/state/<name>/oauth/clients.toml`
  (mode 0600, atomic rename on save). 9 router-level integration
  tests pin the metadata shape, DCR happy / disabled paths, admin
  middleware (loopback + bearer combinations), and the
  manual-register → list → revoke round-trip — including a
  defence-in-depth assertion that secrets never appear in admin
  list output.

- **BREAKING:** `gosh-agent setup` is now the single source of truth for
  every per-instance daemon-spawn knob. New flags `--host`, `--port`,
  `--watch` / `--no-watch`, `--watch-key`, `--watch-swarm-id`,
  `--watch-agent-id`, `--watch-context-key`, `--watch-budget`,
  `--poll-interval`, `--no-autostart` patch the same `GlobalConfig`
  TOML the daemon reads at startup; re-running setup with a subset
  updates only those fields (atomic patch). `gosh-agent serve`'s own
  flags become optional overrides that fall back to GlobalConfig — the
  CLI invokes the daemon with just `--name <name>` going forward.
  Tracked in `<gosh.cli>/specs/agent_mcp_unification.md` (commit 5).

- **BREAKING:** new top-level `gosh-agent uninstall --name <name>`
  command — idempotent teardown of an agent instance. Stops/removes
  the autostart artifact (launchd plist on macOS, systemd user unit
  on Linux), strips this agent's hooks + MCP entries from
  claude/codex/gemini at both user and project scopes (project =
  current cwd), and removes `~/.gosh/agent/state/<name>/`. Each step
  is best-effort — re-running on a partially uninstalled agent
  finishes the job rather than erroring. Tracks Commit 5 of
  `<gosh.cli>/specs/agent_mcp_unification.md`.

- new `src/plugin/autostart.rs` module: writes a launchd plist at
  `~/Library/LaunchAgents/com.gosh.agent.<name>.plist` (macOS) or a
  systemd user unit at `~/.config/systemd/user/gosh-agent-<name>.service`
  (Linux) and reload-loads it via `launchctl bootout`/`bootstrap` or
  `systemctl --user enable --now`+`restart`. Setup invokes
  `autostart::install` automatically unless `--no-autostart` is
  passed; install is idempotent so a re-run during a setup that just
  changed GlobalConfig kicks the supervised daemon into picking the
  new values up. The unit always runs `gosh-agent serve --name <name>`
  — config knobs live in GlobalConfig, so changing them never needs
  to rewrite the unit's argv. Tracks Commit 5 of
  `<gosh.cli>/specs/agent_mcp_unification.md`.

- **BREAKING:** `gosh-agent serve` reads its credentials directly from
  the OS keychain. `--name <NAME>` is now required, the
  `--bootstrap-file` arg is gone, and the previously-ephemeral
  bootstrap-file dance (CLI writes a temp 0600 file with
  `join_token` + `secret_key`, daemon reads it and immediately
  deletes) is retired. Single channel for credentials between CLI
  and daemon: the keychain entry the CLI provisioned during
  `gosh agent create` / `gosh agent import`. Daemon's new
  `keychain` module is read-only — the CLI stays the sole writer,
  schema-compatible with `<gosh.cli>/src/keychain/agent.rs`.
  `principal_token` from the keychain is also threaded through as
  the default for `--memory-auth-token` (CLI/env override still
  wins). Migration: cross-cuts with the cli-side change that drops
  bootstrap-file generation; both must land together. Operators
  running gosh-agent v0.7.3+ binaries with an older CLI (or vice
  versa) see a clap-level "unknown argument" error at spawn — the
  intentional fail-loud contract for this kind of plumbing change.
  Tracks Commit 5a of `<gosh.cli>/specs/agent_mcp_unification.md`.

- **BREAKING (internal):** stdio MCP-proxy collapses to a thin
  stdin↔HTTP transport bridge to the agent daemon. Previously the
  proxy was a smart middleware talking to memory directly — handling
  key/swarm injection, the grounded tool whitelist, `tools/list`
  rewriting, and stale-session recovery. All of that now lives in
  the daemon. The proxy reads stdin, POSTs to
  `http://<daemon_host>:<daemon_port>/mcp`, writes the response to
  stdout — and that's it. ~1000 LOC + 18 tests removed from
  `src/plugin/proxy.rs`. New `--daemon-host` (default `127.0.0.1`)
  and `--daemon-port` (default `8767`) CLI args on
  `gosh-agent mcp-proxy` point it at the daemon. The legacy
  `--default-key`, `--default-swarm`, `--full-memory-surface` args
  are kept as hidden no-ops so existing `.mcp.json` files written
  by older `gosh-agent setup` don't fail clap parsing; setup-side
  cleanup follows in a separate cli-repo commit. Tracks Commit 4
  of `<gosh.cli>/specs/agent_mcp_unification.md`.
- daemon MCP gateway: `tools/call` now also gates `memory_*` calls
  through the same `grounded_memory_tool_allowed` allowlist used by
  `tools/list`. Defense-in-depth for any caller that bypasses
  `tools/list` discovery (direct curl, mis-behaving LLM, future
  remote connector). Returns a structured `TOOL_NOT_ALLOWED` error
  for blocked tools rather than forwarding silently. Defence-in-depth
  follow-up to Commit 3 of
  `<gosh.cli>/specs/agent_mcp_unification.md`.
- removed unused `inject_default_key` / `inject_default_swarm`
  helpers from `client::memory_inject`. Their only caller was the
  pre-thinning proxy; the if-absent variants
  (`set_default_key_if_absent`, `set_default_swarm_id_if_absent`)
  used by the daemon's forwarder remain. If overwrite semantics
  ever come back into demand the deleted helpers were ~10 LOC each
  and trivial to restore. Cleanup pass for Commit 4 of
  `<gosh.cli>/specs/agent_mcp_unification.md`.

- fix: source the daemon's MCP-forwarding defaults (`key` / `swarm_id`
  for `memory_*` tool calls forwarded through `/mcp`) from the
  per-instance `GlobalConfig` written by `gosh-agent setup`, not from
  `--watch-key` / `--watch-swarm-id`. The earlier draft conflated the
  two: watch params drive the watcher loop's task-discovery
  subscription, while bound `key` / `swarm_id` describe the agent's
  default scope for memory operations — agents legitimately watch
  one namespace and answer in another, so the two must stay
  independent. Adds an optional `--name <NAME>` arg to
  `gosh-agent serve`; when set the daemon loads
  `~/.gosh/agent/state/<name>/config.toml` and uses
  `GlobalConfig.key` / `GlobalConfig.swarm_id` as the if-absent
  fallback. When omitted (operator-direct invocation), defaults stay
  `None` and forwarded calls must carry explicit scope per request.
  `gosh agent start` (CLI side) is expected to pass `--name <cfg.name>`
  derived from the resolved instance — landing in a separate cli-side
  commit. Tracks Commit 2a of
  `<gosh.cli>/specs/agent_mcp_unification.md`.

- daemon MCP gateway: implement `tools/list` (previously a fall-through
  to empty 200). Returns a merged surface — memory's tools, fetched
  via `MemoryMcpClient::list_tools`, filtered through the same
  grounded whitelist the stdio proxy uses (`memory_recall` /
  `memory_list` / `memory_get` / `memory_query` / `memory_write` /
  `memory_write_status` / `memory_ingest_asserted_facts`) with `key`
  stripped from their input schemas, plus three `agent_*` tools
  (`agent_create_task`, `agent_status`, `agent_task_list`) defined
  with LLM-tuned descriptions emphasising the dispatch-and-poll
  semantics. `agent_start`, `agent_courier_subscribe`,
  `agent_courier_unsubscribe` stay internal — first because it's a
  CLI/curl path, the others because they require stateful SSE
  per-turn LLM tool calls cannot consume. Memory unreachable at
  `tools/list` time degrades gracefully: warn-log, surface only the
  `agent_*` tools so chat-driven dispatch keeps working. Six unit
  tests cover the whitelist, the schema-key stripping, the agent
  tool list shape (including the "DOES NOT wait" hint in the
  `agent_create_task` description), and the merge behaviour both
  with a healthy memory and with an erroring one. Tracks Commit 3
  of `<gosh.cli>/specs/agent_mcp_unification.md`.

- daemon MCP gateway: forward `memory_*` tool calls from the daemon's
  `:8767/mcp` HTTP endpoint to memory itself (`MemoryMcpClient::forward_tool`),
  with **per-call scoping**: an explicit `key` / `swarm_id` from the
  caller wins; missing fields fall back to the daemon's configured
  defaults (sourced from `--watch-key` / `--watch-swarm-id`). New
  `set_default_key_if_absent` / `set_default_swarm_id_if_absent`
  helpers in `client::memory_inject` implement the if-absent semantics
  (different from the existing `inject_default_*` overwrite helpers
  used by the stdio proxy for cross-namespace prevention). Errors from
  memory surface as `{ error, code: "MEMORY_FORWARD_FAILED" }` shaped
  Values, mapped to JSON-RPC `isError: true` by the existing handler
  envelope. Not yet reachable from coding-CLI LLMs — the stdio proxy
  still talks to memory directly until a later step in the
  `daemon_as_mcp_gateway` series — but curl-driven calls to
  `:8767/mcp` work today. Tracks Commit 2 of
  `<gosh.cli>/specs/agent_mcp_unification.md`.

- refactor: extract memory-tool injection helpers
  (`is_memory_tool_name`, `inject_default_key`, `inject_default_swarm`)
  from the stdio MCP-proxy into a new shared module at
  `src/client/memory_inject.rs`. Pure refactor with no behaviour
  change, prep for the daemon-as-MCP-gateway forwarder. Tracks
  Commit 1 of `<gosh.cli>/specs/agent_mcp_unification.md`.

- transport: centralise `<authority>/mcp` URL canonicalisation so the
  fix that proxy got in v0.7.2 now also covers the capture and
  replay-buffer call paths, plus the courier SSE subscription. Before:
  `plugin::proxy` had a private `construct_mcp_url` that handled
  trailing-slash and already-includes-`/mcp` variants, but
  `HttpTransport` (used by `plugin::capture` and the `replay-buffer`
  subcommand) only did a `trim_end_matches('/')` before appending
  `/mcp` itself — so an `authority_url = "http://h:8765/mcp"` would
  produce `/mcp/mcp` on every prompt/response capture write and on
  buffered-replay. `crate::courier`'s SSE subscribe had the same
  shape (`format!("{}/mcp/sse", memory_url)`). New `pub fn
  canonical_mcp_url(authority_url: &str) -> String` lives in
  `client::transport`; `HttpTransport::new` / `with_client` store the
  canonical form in `mcp_endpoint`, the proxy and courier import it
  directly. Regression test in `client::transport`
  (`http_transport_endpoint_is_canonical_for_every_input_shape`)
  pins the constructor-side centralisation without a network round
  trip.

- mcp-proxy: redact `Mcp-Session-Id` in the failed-replay-initialize
  warning too. The previous fix covered the success `info!` and the
  `record_captured_session_id` `debug!` events, but the failure path
  still logged `stale_session = ?stale` (Debug-formatted
  `Option<String>`), emitting the full id at warn-level — exactly
  when operators are most likely to grab logs for a bug report. Now
  passes the stale value through `session_id_fingerprint` first,
  consistent with the rest of the redaction.

- mcp-proxy: stale-session recovery now also replays
  `notifications/initialized` between the cached `initialize` and the
  retried original request, matching the MCP lifecycle requirement
  (<https://modelcontextprotocol.io/specification/2025-03-26/basic/lifecycle#initialization>).
  Without it, a stricter Streamable-HTTP / FastMCP server keeps the
  freshly-issued session in `initializing` state and rejects
  `tools/list` / `tools/call` until the notification arrives — the
  retry would silently fail on such servers. The notification is
  fire-and-forget; non-2xx is logged as `warn!` so operators can
  diagnose mismatched servers, but recovery still proceeds to the
  retry. New mock test (`forward_with_recovery_replays_initialized_notification_so_strict_server_accepts_retry`)
  pins the lifecycle: the mock server can be flipped into strict-mode
  where non-`initialize`/non-notification methods return JSON-RPC
  `-32002` "session not initialized" until the proxy replays the
  notification, asserting the recovery sequence is exactly
  `tools/list` (404) → `initialize` → `notifications/initialized` →
  `tools/list` (success).

- mcp-proxy observability: stop emitting full `Mcp-Session-Id` values
  in logs. MCP's transport spec
  (<https://modelcontextprotocol.io/specification/2025-03-26/basic/transports#session-management>)
  treats session ids as cryptographically secure session state —
  future deployments may use them as bearer-like routing material —
  so the previous `info!` line on recovery success and the `debug!`
  events on first-capture / mid-session change leaked sensitive
  values into operator-visible output. The recovery-success `info!`
  now omits the session id entirely (just method + the recovery
  message). The capture / change `debug!` lines log an 8-hex-char
  SHA-256 fingerprint via the new `session_id_fingerprint` helper —
  stable across log lines so correlation still works, but reveals
  nothing about the underlying value.

- mcp-proxy: detect a stale `Mcp-Session-Id` after memory restart and
  recover automatically. The stdio proxy used to capture the session
  id from the very first `initialize` response and reuse it for the
  full proxy lifetime; when memory was restarted under it (upgrade,
  container recycle, manual restart) every subsequent request got a
  silent `404 Not Found` from FastMCP because the in-memory session
  table had been cleared, and the operator had to kill the
  coding-CLI session to recover. The proxy now caches the most
  recent `initialize` payload that passed through and, on a 404 with
  a session id held, replays that exact payload to obtain a fresh
  session id, then retries the original request once. Detection is
  intentionally narrow — only HTTP 404, only when a session id was
  set at request time, only for non-`initialize` and
  non-`notifications/*` methods, and only when a cached `initialize`
  exists — so genuine 404s (wrong path, missing route) are still
  surfaced verbatim. If the replayed `initialize` itself fails, the
  proxy returns a structured error naming the suspected cause
  (memory restarted; re-init failed) instead of bubbling a bare
  HTTP status. Coverage in three new tests against an ad-hoc axum
  mock authority: end-to-end recovery sequence, no-loop guard on
  `initialize` method, and verbatim error pass-through when no
  cached `initialize` is available.

- mcp-proxy: normalize `authority_url` so a trailing `/mcp` (or `/`) in
  the configured value doesn't double up into `/mcp/mcp` when the proxy
  appends its own path. FastMCP answers `/mcp/mcp` with a bare 404,
  which historically masqueraded as a "stale URL" production incident
  in the followup backlog. The new `construct_mcp_url` helper trims a
  trailing slash, then a trailing `/mcp`, then any remaining slash
  before appending `/mcp`, so configurations that already include the
  path (legitimate `--public-url` overrides, some remote-bundle
  imports) and bare-host configurations all converge on the same
  canonical POST target. Subpath mounts (e.g. reverse-proxy at
  `/memory`) are preserved.
- mcp-proxy observability: emit a single `info!` line at startup with
  the resolved `authority_url` (userinfo redacted), the agent name,
  and `full_memory_surface`. Without this, operators had no way to
  tell which authority a running stdio proxy was actually targeting
  short of grepping `~/.gosh/agent/state/<name>/config.toml`. The
  `redact_url` helper strips a `user:pass@` segment from the URL
  authority for safe logging — defensive only, since auth flows
  through dedicated headers in the normal path. Additionally, log a
  `debug!` event when the proxy first captures an `Mcp-Session-Id`
  from a memory response (and another if that id ever changes
  mid-session — a memory-side anomaly worth surfacing). Steady-state
  same-id reuse stays silent.

## [0.7.0] - 2026-04-26

- **BREAKING:** `gosh-agent setup` now installs hooks AND MCP config at
  **project scope by default** — under `<cwd>/.<platform>/...`
  (`<cwd>/.claude/settings.json`, `<cwd>/.codex/hooks.json`,
  `<cwd>/.gemini/settings.json`, plus `<cwd>/.mcp.json` for Claude),
  not user-globally. Hooks now fire only when the coding CLI is
  launched from this directory; the previous user-level default
  caused `gosh-agent capture` to fire across **all** the user's
  projects, leaking prompts from projects where they hadn't asked
  for capture into the agent's memory namespace. Migration: each
  project where capture is wanted must run `gosh agent setup` from
  its own directory; the same install no longer covers everything.
- Flag rename: `--mcp-scope` → `--scope`. The new flag controls both
  hooks AND MCP location (previously only MCP). Default `project`.
  Pass `--scope user` explicitly to opt back into the old behaviour
  (one install captures every coding-CLI session on the host) when
  that's what you actually want — rare. Codex MCP registration is
  always user-global regardless of `--scope` (upstream
  `codex mcp add` has no per-project mode); only Codex hooks honor
  the flag.
- Auto-migration: when `agent setup --scope <X>` runs, this agent's
  hook entries (and Gemini's `mcpServers` entries) are removed from
  the OPPOSITE scope's files so switching project↔user doesn't leave
  shadow installs firing in the previous scope. Other agents'
  entries in the same files are untouched.
- For Claude specifically: `--scope project` now also issues
  `claude mcp remove -s user gosh-memory-{agent}` before writing
  `<cwd>/.mcp.json`, so a prior `--scope user` install doesn't leave
  a global Claude MCP registration alive after the user switches
  back to project scope. Without this, the agent's memory tools would
  remain exposed to every Claude session on the host even after the
  switch — exactly the cross-project tool-exposure path the
  project-default change is meant to close. The reverse direction
  (project→user) was already handled.
- The `cwd=/` guard previously fired only when project-scope Claude
  would write `<cwd>/.mcp.json`. With the project-default change it
  now fires for any selected (or auto-detected) platform at project
  scope, since every platform writes a project-rooted file under
  `<cwd>/.<platform>/...`. Error message updated to point at
  `--scope user` as the explicit opt-out for users who actually want
  user-global install.

## [0.6.1] - 2026-04-25

- Fix `NO_MODEL: memory recall did not provide a model` after upgrading the
  memory server to v0.3.0+. memory v0.3.0 made `memory_recall` evidence-only
  and moved the executable inference plan (model, payload, payload_meta,
  secret_ref) into a new `memory_plan_inference` MCP tool. The agent task
  runner now calls `memory_plan_inference` for model routing in addition to
  the existing recall (kept for the `context` string used in prompt
  rendering); the plan-extraction helper reads `secret_ref` from the
  top-level of the plan response with a fallback to the legacy
  `payload_meta.secret_ref` location, so this build is compatible with both
  memory v0.3.0+ and pre-v0.3.0 servers that still embed the plan in recall

## [0.6.0] - 2026-04-25

- Fix race between Claude / Codex Stop hooks and transcript flush. Both CLIs
  could fire the Stop hook before flushing the latest assistant turn to the
  transcript file, leaving capture's parser to read a stale file → empty
  content → silent skip of `memory_write`. Symptom: short single-token
  replies (and small fast turns generally) never reached memory while
  longer turns usually did. `extract_response` now wraps the per-platform
  transcript parser in `read_with_flush_retry`, which retries once after a
  short delay if the first read came back empty. Gemini is unaffected
  (its hook delivers response text directly via stdin, no file involved).
- Bound agent bootstrap memory control-plane calls with a configurable timeout
  (`bootstrap_memory_timeout`, default 60s). Tasks that hit the timeout fail
  with `BOOTSTRAP_RESOLVE_TIMEOUT` or `BOOTSTRAP_RECALL_TIMEOUT` instead of
  hanging the agent indefinitely on slow or unreachable memory.
- `create_task` MCP handler no longer awaits the slow semantic store of the
  task description. The authoritative task fact is written synchronously; the
  semantic indexing write is best-effort in a detached background task.
  Detached store failures are logged with `task_id` but are not surfaced to the
  caller.
- Bound terminal task result persistence/visibility checks with the same memory
  control-plane timeout. If memory hangs while the agent is writing a failed
  terminal result, the agent returns with `TASK_RESULT_PERSIST_TIMEOUT` instead
  of blocking the watch/courier worker indefinitely.
- Bootstrap/pre-execution failures and post-execution result persistence
  failures now write a bounded local fallback JSON artifact under the agent
  state directory. The artifact includes a result preview when execution already
  produced output, is updated when canonical memory persistence later succeeds,
  and is pruned by `local_failure_artifact_retention` to keep long-running
  agents from accumulating unbounded local failure files. This generalized
  artifact is `schema_version=2`; v1 used the bootstrap-specific
  `source=local_bootstrap_failure_fallback`. Memory remains the canonical
  source of truth whenever persistence succeeds.

## [0.5.1] - 2026-04-25

- Fix Claude Code Stop-hook capture missing assistant responses after Claude Code
  changed transcript entries to top-level `type=assistant` with nested
  `message.role=assistant`. The capture path now recognizes legacy top-level
  `role=assistant`, nested `message.role=assistant`, and top-level
  `type=assistant`, so prompts and responses are both persisted to memory.

## [0.5.0] - 2026-04-24

- Local-CLI subprocess runs in task workspace directory. `workspace_dir` is resolved from task metadata or recall payload (fields: `workspace_dir`, `local_cli_workspace`, `working_directory`, `repo_worktree`, `cwd`). Validated: must exist, must be a directory, canonicalized to prevent symlink escapes
- Terminal deliverable source validation: deliverables written by local CLI must have `metadata.source=external_cli`. Stdout-captured results with `source=agent_result` are rejected with a retry prompt instructing the CLI to write via `memory_ingest_asserted_facts`
- Deliverable contract prompt improved: now includes full `memory_ingest_asserted_facts` call shape (key, agent_id, swarm_id, scope, facts array), not just fact metadata fields
- MCP proxy tracing expanded: traces all `memory_*` tool calls (was only `memory_recall`). Trace files include tool name. New counters: `memory_ingest_asserted_facts_attempt_count`, `memory_ingest_asserted_facts_failure_count`
- Landlock sandbox: `GOSH_AGENT_WORKSPACE_DIRS` env var adds workspace paths to RW allowlist. Fails closed at startup if paths are invalid (exit 78)
- Local-CLI backend runs without a wall-clock timeout cap. Long-running Codex,
  Claude, Gemini, or other external CLI tasks can complete instead of being
  killed by the agent's API-model request timeout. Trade-off: a hung local CLI
  subprocess can block that task until operator cancellation.
- `cli_timeout_secs`, `timeout_secs` under `local_cli`, and
  `GOSH_LOCAL_CLI_TIMEOUT_SECS` are no longer honored by local-CLI execution.
  The agent logs a warning when legacy memory config timeout fields are present.

## [0.4.0] - 2026-04-24

- `agent setup --platform claude --mcp-scope user` registers the MCP server via `claude mcp add -s user` instead of writing `<cwd>/.mcp.json`. User-scope registration works from any directory and skips the per-project trust prompt that the project-scope `.mcp.json` requires. Default remains `project` to preserve existing behavior. No effect for codex/gemini — they're already user-global. `agent remove` cleans up both scopes
- `agent setup` cwd=/ guard now only fires when this run will actually write `<cwd>/.mcp.json` — i.e. project-scope claude. `--mcp-scope user` (no cwd write at all) and codex/gemini-only setups (own user-global config paths) now succeed from `/`, where they previously bailed unnecessarily. Helper `writes_project_mcp_in_cwd(mcp_scope, platforms)` covers the gating; auto-detect (empty `--platform`) stays pessimistic so we still fail loudly instead of half-installing
- `agent setup --mcp-scope user` now also strips a stale `gosh-memory-{agent}` entry from `<cwd>/.mcp.json` left by a previous project-scope run. Without this, switching an existing install from project to user scope left both registrations visible to Claude in the project, preserving the per-project trust/stale-args path the user-scope mode is meant to avoid. `remove_claude_mcp` reuses the same project-entry helper
- `mcp-proxy` injects `swarm_id` (and `key`) into **memory** tool calls via new `--default-swarm` flag (mirrors existing `--default-key`). Without this, LLM clients (claude/codex/gemini) calling `memory_recall` / `memory_store` would default to `swarm_id="default"` on the server, missing facts written under a named swarm. `agent setup --swarm <id>` now propagates the swarm into the registered MCP-server args for all three CLIs, so the recall/write swarm context matches the capture-write swarm context end-to-end. Non-memory tools reachable through the same proxy (custom MCP servers exposed alongside `gosh-memory-*`) pass through unchanged — injection is gated on `is_memory_tool_name(...)` so we don't feed memory-specific arguments into unrelated tool schemas
- Fix Codex CLI hooks discovery. Earlier code wrote `~/.codex/hooks-gosh-{agent}.json`, which Codex never reads — Codex 0.117+ discovers hooks at `~/.codex/hooks.json` (and per-repo `<repo>/.codex/hooks.json`). `configure_codex_hooks` now merges into the canonical `~/.codex/hooks.json` (same flow as `configure_claude_hooks`), and `remove_codex_hooks` cleans entries from there plus removes the legacy file from prior installs. Capture for Codex sessions now actually fires
- `agent setup` refuses to run when `cwd` is the filesystem root (`/`). It writes `<cwd>/.mcp.json` for Claude Code's project-local MCP registration, and Claude refuses to load `.mcp.json` from `/` for security — so a setup from `/` produced a silently-broken install. Bail early with a clear suggestion to `cd` into a project directory first
- `agent setup` prints the resolved capture scope at the end (`swarm-shared (swarm: X)` or `agent-private` with a hint about `--swarm`). Earlier the chosen scope was implicit — easy to miss that capture went to `agent-private` until recall came back empty in a later session
- Model pricing sourced from memory profiles: recall returns pricing in `payload_meta`, local `model_pricing.toml` is optional override
- Local pricing file takes priority over recall pricing (operator can override per-model costs)
- Budget controller with preflight estimates, per-model cost tracking
- Prompt isolation: execution/review/review_repair prompts loaded from files, bundled as fallback
- Review loop: structured JSON verdicts, repair pass for malformed output, retry with budget cap
- Task result sanitization: strip `<think>` blocks, reject reasoning-only output
- Conservative token estimation for non-ASCII text (CJK, Cyrillic)
- Execution iteration limit (max 32 turns)
- Proxy forces `--default-key` into memory namespace (prevents model from overriding)
- Setup uses stable git-based namespace resolver instead of cwd basename
- `--platform` filter is now authoritative: omitted platforms have hooks/MCP removed
- Hook cleanup matches agent name as exact token, not substring
- Sandbox: create state/cache dirs before activation, grant RW to prompt cache
- Portable checksum in release workflow (Linux `sha256sum` / macOS `shasum -a 256`)
- Replace `std::sync::Mutex` with `parking_lot` in tests, remove duplicate `cfg(test)`
- Fix `rustls-webpki` security advisory (RUSTSEC-2026-0098, RUSTSEC-2026-0099)
- Zeroize decrypted API keys in MultiProvider
- Move proxy stdin to `spawn_blocking`, add LLM call timeouts
- Deduplicate MockTransport across test modules
- `--swarm` flag in setup: capture uses swarm-shared scope when swarm is configured, agent-private otherwise
- Re-running setup without `--swarm` clears swarm_id (reverts to agent-private scope)
- `--key` passed through setup to agent config (overrides git-based auto-detection)
- Fix: capture uses `agent_name` as agent_id instead of `platform:install_id`
- Fix: capture omits `swarm_id` from payload in agent-private scope (avoids memory rejection)

## 0.3.0

- CI release workflow: cross-platform build matrix, manifest.json generation
- Landlock self-sandboxing (Linux)
- Updated `sha2` to 0.11, `hkdf` to 0.13

## 0.2.1

- Per-instance config.toml (moved from global `~/.gosh/agent/config.toml` to `~/.gosh/agent/state/{name}/config.toml`)
- Hook writers preserve per-agent hooks instead of overwriting
- MCP proxy and server names are per-agent (`gosh-memory-{name}`)
- Removed legacy global config fallback from serve

## 0.2.0

- [fix_multi_agent_state_dir](specs/fix_multi_agent_state_dir.md) — Per-instance state directory isolation

## 0.1.0

- [agent_config_unification](specs/agent_config_unification.md) — Agent config unification
- [gosh_agent_v1](specs/gosh_agent_v1.md) — Gosh Agent v1.0
