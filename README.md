# Clodex

[![CI](https://github.com/DeanDiasti/clodex/actions/workflows/ci.yml/badge.svg)](https://github.com/DeanDiasti/clodex/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust 1.88+](https://img.shields.io/badge/rust-1.88%2B-orange.svg)](https://www.rust-lang.org/tools/install)
[![macOS and Linux](https://img.shields.io/badge/platform-macOS%20%7C%20Linux-lightgrey.svg)](#1-requirements)

**Use Claude Code's agentic coding interface with OpenAI Codex models on your
ChatGPT account, and Claude models on your Claude subscription, side by side.**

Clodex is a local, open-source launcher that runs Claude Code as the interactive
coding harness while routing model requests to Codex on your ChatGPT account.
It keeps Claude Code's UI, agents, tools, permissions, and workflows; only the
model transport and model aliases change for the launched process. Any role can
also run a real Claude model on your own Claude subscription.

```text
                         ┌→ built-in Codex backend → Codex on your ChatGPT account
Claude Code → bridge ────┤
                         └→ api.anthropic.com (Claude routes, your Claude login)
```

Ordinary `claude` sessions are unaffected. Clodex needs no OpenAI API key, no
Codex CLI, and no external proxy: sign in once with `clodex auth login`. An
existing Codex CLI login is reused when you have one.

> [!IMPORTANT]
> Clodex is an independent community project. It is not affiliated with,
> endorsed by, or supported by Anthropic or OpenAI.

## Why Clodex?

- Keep Claude Code's terminal experience, subagents, tool use, and permission
  controls.
- Use the models your ChatGPT account can reach in Codex without hard-coding
  model names, and mix in Claude models on your Claude subscription.
- Pick any model from `/model`, and give subagents a model from either
  provider.
- Run everything locally in one self-contained binary, behind a loopback-only
  bridge.
- Leave normal Claude Code sessions and global model settings untouched.
- Share one supervised backend safely across concurrent Clodex sessions.

## Quick start

### 1. Requirements

- macOS or Linux
- A ChatGPT account with Codex access
- [Claude Code](https://code.claude.com/docs/en/setup)
- Optionally, [`claude-code-proxy`](https://github.com/raine/claude-code-proxy),
  only if you switch from the [built-in Codex backend](#built-in-codex-backend)
  to the external one

Clodex currently relies on Unix domain sockets and does not support native
Windows.

### 2. Install Clodex

Download the archive for your platform from the
[latest release](https://github.com/DeanDiasti/clodex/releases/latest), then:

```sh
tar -xzf clodex-v*-*.tar.gz
mkdir -p ~/.local/bin
install -m 755 clodex ~/.local/bin/clodex
```

Release archives are available for Linux x86-64/ARM64 and macOS
Intel/Apple Silicon. Ensure `~/.local/bin` is on `PATH`.

To build from source instead, install Rust 1.88 or newer and run:

```sh
git clone https://github.com/DeanDiasti/clodex.git
cd clodex
./scripts/install.sh
```

Add `--install-proxy` to also install `claude-code-proxy` with Homebrew, for
the optional external backend.

The source installer uses `~/.local` by default, producing
`~/.local/bin/clodex`.

### 3. Verify and run

```sh
clodex auth login     # sign in to Codex with your ChatGPT account
clodex doctor
clodex
```

`clodex auth login` opens a browser. Over SSH or on a machine without one, use
`clodex auth login --device` and enter the code it prints on any device. If you
already use the [Codex CLI](https://developers.openai.com/codex/cli), you can
skip signing in: Clodex reuses its login.

Run `clodex` from any project directory. Arguments that are not Clodex
management commands pass directly to Claude Code:

```sh
clodex --resume
clodex -p "summarize this repository"
clodex -- --resume
```

Clodex shows a purple launch banner, changes the terminal title while it runs,
and selects a Clodex-only purple Claude theme, including the welcome logo. The
theme definition is stored at `~/.claude/themes/clodex.json`; ordinary Claude
sessions retain their own theme.

## Update or uninstall

Clodex checks for a new stable GitHub release in the background when you launch
it and once an hour while it runs. When a release is available, the status line
shows `Clodex v… update available · clodex update`, alongside your existing
status-line output. Run this in another terminal:

```sh
clodex update
```

The command downloads the release for your platform, verifies its SHA-256
checksum and version, and atomically replaces the installed executable in
place. No Rust toolchain is needed, and the installation directory must be
writable. Close all active Clodex sessions and relaunch after updating so the
shared backend also restarts with the new version.

Update checks share an hourly cache under `~/.clodex/cache/updates.json` (or
`$CLODEX_HOME/cache/updates.json`). Offline and rate-limited checks are silent;
status-line rendering reads only the cache and never makes network requests.
The wrapper is supplied only to the launched Claude process, preserving the
configured user, project, or `--settings` status-line command and its JSON input.
It refreshes at least every 15 seconds, including while idle. Managed Claude
settings can override the session's status line.

For development builds, the source installer is also repeatable. Update the
checkout and run it again to install the latest source rather than a release:

```sh
git pull
./scripts/install.sh
```

It builds with `Cargo.lock` and atomically replaces the installed Cargo binary.
Useful installer options:

```text
--root <directory>           Choose an installation prefix
--install-proxy              Install a missing proxy with Homebrew
--skip-prerequisite-checks   Build without checking runtime commands
```

`CLODEX_INSTALL_ROOT` supplies the default for `--root`. To uninstall, run
`clodex auth logout`, then remove `<install-root>/bin/clodex`. Remove the
Clodex home as well, `$CLODEX_HOME` when it is set and `~/.clodex` otherwise,
only if the saved configuration, sign-in, cache, and logs are no longer wanted.

## Commands

| Command | Purpose |
| --- | --- |
| `clodex` | Start Claude Code through Clodex |
| `clodex update` | Install the latest stable release in place |
| `clodex auth login [--device]` | Sign in to Codex with your ChatGPT account |
| `clodex auth logout` | Remove Clodex's Codex sign-in |
| `clodex auth [status]` | Show which Codex sign-in is used and validate it |
| `clodex auth sync` | Refresh the sign-in and sync the running backend |
| `clodex models [list]` | Show visible, API-supported Codex models |
| `clodex models map` | Show Claude-role-to-Codex routing |
| `clodex models … --json` | Emit machine-readable model data |
| `clodex config [show]` | Show persistent defaults |
| `clodex config context <auto\|tokens>` | Set context capacity |
| `clodex config compact-at <1..95>` | Set the auto-compaction percentage |
| `clodex config backend <builtin\|proxy>` | Choose the Codex translator |
| `clodex config hierarchical-compaction <on\|off>` | Fold an oversized compaction into rounds |
| `clodex config route <role> <claude-model\|codex>` | Route a role to a Claude model on your subscription |
| `clodex config allow-tool <exact-name>` | Trust one tool for sessions and subagents |
| `clodex config forget-tool <exact-name>` | Remove a trusted tool |
| `clodex config path` | Print the configuration path |
| `clodex context` | Show the effective capacity and compaction trigger |
| `clodex doctor` | Check installed tools, login, and local paths |

Use `clodex --help` or `clodex <command> --help` for generated command help.

## Model routing

On every launch, Clodex reads your account's Codex model catalog, removes hidden
or API-unsupported entries, and maps catalog priority to Claude Code roles. The
catalog is cached under `~/.clodex/cache/` and reused for five minutes; if it
cannot be fetched, the last cached copy is used.

| Claude role | Codex catalog entry |
| --- | --- |
| Fable | First |
| Opus | Second |
| Sonnet | Third |
| Haiku compatibility | Same route as Sonnet |

The launched session defaults to the Opus route. Fable and Sonnet remain
available from Claude Code's model picker. Haiku is hidden from the picker but
Claude Code's background Haiku requests are supported through the Sonnet
route. If fewer than three models are available, Clodex safely reuses the
closest available route.

No model names are hard-coded, so the mapping follows the live Codex catalog.
The built-in backend routes every model in that catalog. With the external
proxy backend, a preflight check also ensures the installed
`claude-code-proxy` understands every selected model.

## Claude models on your Claude subscription

Any role can use a real Claude model instead of its Codex model. Claude-routed
requests reuse Claude Code's own subscription login and count against your
Claude plan's usage limits, exactly as an ordinary `claude` session does:

```sh
clodex config route opus claude-opus-5-5
clodex config route haiku claude-haiku-4-5
clodex config route opus codex      # back to the automatic Codex model
clodex models map
```

Roles are `fable`, `opus`, `sonnet`, and the hidden background `haiku` role.
Unrouted roles keep their automatic Codex model, so a session can mix both
providers and switch between them from the model picker.

Clodex never reads, copies, or stores the Claude credential. When a Claude
route is configured, the launched Claude Code process keeps its own login
instead of the local placeholder token, and the bridge splits traffic by
model:

```text
anthropic/claude-*  → prefix removed, forwarded unchanged → api.anthropic.com
auto mode judge     → Claude classifier model            → api.anthropic.com
everything else     → Claude credential and OAuth beta removed → Codex proxy
```

Auto mode permission checks use Anthropic models even when the conversation
runs on GPT. Clodex leaves Claude Code's server-review default and your
`CLAUDE_CODE_AUTO_MODE_SERVER` override intact. Claude routes preserve the
server-review request fields, headers, and response streams. Where Anthropic
enables server review, those checks run at no charge as part of the
conversation request.

Codex does not implement Anthropic's server-review protocol. When it returns
no review results, Claude Code falls back to separate classifier requests;
that fallback can persist for the session, including after a model switch.
With a Claude subscription login, Clodex keeps Claude Code's Sonnet default
at the canonical `claude-sonnet-5` ID, preserving its native judge selection
and per-model scoring format. Ordinary requests for the Sonnet role resolve
to your configured route in the bridge; classifier requests keep the native
Claude model and go directly to Anthropic before any Codex or fast-mode routing,
preserving the classifier prompt and your Claude login. An unavailable judge
reports an error rather than falling back to GPT. Pro, Max, and Team plans
have no classifier overhead charge; other accounts can be billed for
separate classifier calls. See Anthropic's
[classifier billing documentation](https://code.claude.com/docs/en/auto-mode-classifier-billing).
Run `/status` and check **Auto mode server** for the session's active path.
After upgrading this routing, close every active Clodex session before
relaunching so the shared bridge also upgrades. A new launcher refuses an
older bridge that cannot separate the Sonnet role from the native judge.

Routed models appear to Claude Code as `anthropic/<model>`. Claude Code treats
that exactly like the bare ID, and the prefix keeps a real Claude route
distinct from the `claude-*` names fast mode uses while it shadows a Codex
model. `/fast` on a Claude route uses Anthropic's own fast mode.

`clodex` refuses to start a Claude route unless `claude auth status` reports
a Claude subscription login. Context capacity follows the smallest routed
window: 1M for Fable, Mythos, Opus 5.x and 4.6 to 4.8, and Sonnet 5.x and 4.6;
200K for Haiku, older Opus and Sonnet models, and any ID Clodex does not
recognize. Hierarchical compaction currently applies to Codex routes only; a
Claude-routed compaction is forwarded unchanged.

With a subscription login, the Claude Code features that depend on it keep
working in a Clodex session, on Codex models too: claude.ai connectors,
managed plugins, feature-flagged tools, and artifacts. Without one, Clodex
turns off Claude Code's nonessential traffic, since there is no Claude account
for it to reach.

## Every model in `/model` and in subagents

Each launch lists every routable Codex model in Claude Code's `/model` picker,
after the four roles: every model in the live catalog with the built-in
backend, or those the installed proxy lists with the external one. With a
Claude subscription login, the current Claude models are listed as well, even
when no role is routed to one.

Clodex writes that list into Claude Code's model-discovery cache
(`~/.claude/cache/gateway-models.json`, or under `CLAUDE_CONFIG_DIR`) for the
bridge's address, and enables discovery for the launched process only. Claude
Code reads the cache without a credential, so its own subscription login is
untouched. The bridge declines the discovery fetch itself, because a successful
fetch would replace the list with one filtered to Claude-looking IDs.

The same models are also passed as Claude Code's curated `modelPicker` rows
for the launched process. Claude Code otherwise checks a model it does not
recognise with a one-token request the first time `/model <name>` or `--model`
names it in a session, which costs a full Codex round trip, about a second or
more, before the switch completes. Listed models switch immediately. These rows
take the place of a `modelPicker` in your own user settings for Clodex
sessions.

The Agent tool's `model` parameter only accepts the four role aliases, so
Clodex also passes one `--agents` definition per listed model, such as
`codex-gpt-6-luna` or `claude-sonnet-5-5`. Ask for a subagent on a specific
model and Claude Code can delegate to either provider. Passing your own
`--agents` replaces these definitions.

The context capacity still follows the four roles. A model listed only in the
picker keeps its own limit, so a conversation larger than that model accepts
must compact before switching to it.

## Fast mode

Launch with `clodex --fast` to keep supported Codex models on the priority
service tier for the entire session, including subagents and compaction.
The policy follows model switches and each subagent's own model, without
depending on Claude Code's Opus-only `/fast` toggle. Claude routes run at
their normal speed. Codex models without the catalog's `fast` capability,
or without fast support in the external proxy, run at standard speed.

Place `--fast` before Claude Code arguments:

```sh
clodex --fast
clodex --fast --resume
clodex --fast -p "Review this repository"
```

The launch banner shows `FAST (Codex session)`. The policy lasts for that
launch; Claude Code's `/fast` toggle is disabled in a `--fast` session.

Custom status-line scripts can pipe Claude Code's JSON to
`clodex statusline-fast`. It prints `FAST` for the current supported model,
`FAST unavailable` for an unsupported model, and nothing in a launch without
`--fast`. The helper reads only the launch environment and stdin; it never
starts Claude Code or makes a network request.

Inside a Clodex session, `/fast on` enables the Codex priority service tier for
the model that is already selected. It does not switch the route to Fable,
Opus, Sonnet, or another model. `/fast off` returns that same model to the
standard service tier. This interactive toggle applies to sessions launched
without `--fast`.

Clodex implements this with a loopback-only bridge owned by the shared
supervisor. Requests are tracked independently by Claude session and subagent,
so concurrent sessions can use different models and fast-mode settings. The
bridge marker and Claude fast-mode override are injected only into the child
process launched by `clodex`; normal `claude` sessions and global Claude
settings are not changed. With the external proxy backend, this requires
`claude-code-proxy` 0.1.32 or newer.

After installing or upgrading Clodex, close all older Clodex sessions once so
their old supervisor can exit. The first new `clodex` process will start the
fast-capable bridge; later sessions share it until the final lease closes.

Inspect the current result:

```sh
clodex models
clodex models map
clodex models map --json
```

## Context and compaction

The default `auto` setting resolves to the largest capacity every routed model
will actually accept. Clodex reads the catalog's extended `max_context_window`
and applies its `effective_context_window_percent`, rather than the smaller
standard usage threshold:

```sh
clodex config context auto
```

An explicit value is honoured up to that ceiling and clamped above it:

```sh
clodex config context 600k
clodex config compact-at 90
clodex context
```

Suffixes `k` and `m` are decimal (`600k` is 600,000); plain token counts and
underscores are accepted as well. Clodex supplies the selected value as both
`CLAUDE_CODE_MAX_CONTEXT_TOKENS` and
`CLAUDE_CODE_AUTO_COMPACT_WINDOW`, with the percentage in
`CLAUDE_AUTOCOMPACT_PCT_OVERRIDE`.

This matters because Claude Code otherwise applies a conservative window to an
unfamiliar model behind a custom Anthropic base URL. For example, a configured
`600k` capacity with compaction at `90` produces a 540,000-token trigger.

A capacity above the routed ceiling is clamped rather than passed through, and
`clodex doctor` reports both numbers. This is not a harmless over-request:
Codex rejects an oversized prompt with a 413, and Claude Code's recovery is to
compact — but the compaction request carries the same oversized conversation,
so it is rejected too. The session then cannot compact its way back under the
limit.

Clodex also runs the Codex backend with server-side compaction enabled, which
lets Codex compact upstream rather than reject a prompt that approaches the
model's limit. The extended window is served without it; this is defence in
depth against the rejection path above.

Settings apply when a new Clodex process starts. Restart existing sessions
after changing them. Subagents inherit the launch environment and therefore
receive the same capacity and percentage, but each agent has its own context
window.

## Hierarchical compaction

Opt-in. When a conversation grows past what the routed models accept, the
compaction request carries the same oversized conversation and is rejected too,
so the session cannot compact its way back under the limit. Hierarchical
compaction replaces that single request with a fold whose rounds each fit by
construction:

```text
S₀ = compact(chunk₀)
Sᵢ = compact(Sᵢ₋₁ ++ chunkᵢ)
```

```sh
clodex config hierarchical-compaction on
```

The round count follows the conversation size rather than a fixed number, so a
conversation at twice the ceiling folds in two rounds and one at ten times the
ceiling folds in ten. Each round costs one model call, which is why this is
off by default.

Claude Code's `PreCompact` hook arms the fold, and the bridge confirms the
request that follows carries the summary prompt before folding it. The fold
engages only once a conversation genuinely exceeds the ceiling, so it never
pre-empts a compaction that would have succeeded, and every uncertain
path — an unreadable catalog, a conversation with no safe split point, a
message larger than a whole round — forwards the request unchanged.

Rounds never split a `tool_use` from its `tool_result`, and each round retries
on the interrupted upstream responses that are common on this transport.

## Reasoning effort

Model routing and reasoning effort are independent. Use Claude Code's
`/effort` control or its `--effort` option. Claude sends the selected effort
through the Codex backend as the Codex reasoning level; Clodex does not
replace or silently choose it.

Claude Code may persist its last effort as a user setting, and individual
subagents may override effort in their agent definition. Proxy-generated
compaction summaries intentionally use low effort to keep housekeeping fast;
normal main-agent and subagent requests preserve their selected effort.

## Trusted tools and “don't ask again”

Some Claude Code versions do not reliably persist a “don't ask again” choice
made inside a subagent. Clodex can supply an exact per-launch allow rule to the
main session and every subagent:

```sh
clodex config allow-tool mcp__codebase-memory-mcp__search_code
clodex config forget-tool mcp__codebase-memory-mcp__search_code
```

Only the exact tool name is allowed. Clodex does not trust an entire MCP server
or bypass unrelated permission prompts. Treat these entries like code
execution permissions and allow only tools you understand.

## Built-in Codex backend

Clodex translates Claude Code's requests for Codex itself, with no external
proxy installed. This is the default. The external proxy remains available:

```sh
clodex config backend proxy     # use claude-code-proxy
clodex config backend builtin   # back to the built-in backend (default)
```

The built-in backend is the Codex path of `claude-code-proxy` v0.1.42,
vendored under its MIT license in [`crates/codex-backend`](crates/codex-backend)
with the other providers removed. It runs inside the shared supervisor behind
the same bridge, so fast mode, Claude routes, and hierarchical compaction work
unchanged, and it honours `clodex config transport` and Codex server-side
compaction.

It follows the live Codex catalog rather than a fixed model list: a model
Codex adds is routable immediately, on the Responses lane the catalog names.
The external proxy only routes the models its release lists.

Close every active Clodex session after switching so the supervisor
restarts. The backend writes its log to
`~/.clodex/logs/claude-code-proxy/proxy.log`, like the external proxy, and
keeps one rotated `proxy.log.1` once the log passes 20 MB.

## Shared proxy lifecycle

The first active session starts one supervisor and one loopback-only proxy on
an available ephemeral port. Every launcher obtains a lease over a shared Unix
socket. The supervisor returns its proxy port only after an exact health check
succeeds.

Concurrent startup is serialized with an exclusive file lock. Even if several
Clodex sessions start at almost the same moment, only the lock owner starts the
proxy and all launchers converge on the same control socket.

The session that started the supervisor has no special ownership. If it exits
while another session remains open, the other lease keeps the proxy alive.
Abrupt terminal closure is also handled because the kernel closes that
session's socket. One second after the final lease disappears, the supervisor
stops the proxy and removes its control socket and ephemeral credential.
SIGINT, SIGTERM, startup failure, and a proxy crash follow the same cleanup
path. A supervisor that never receives a lease exits after 15 seconds.

Codex traffic uses streaming HTTP SSE by default. This avoids the 403
WebSocket-upgrade failures that can occur when many Claude subagents start
concurrently. The transport can be changed persistently:

```sh
clodex config transport http
clodex config transport websocket
clodex config transport auto
```

`websocket` can reduce exposure to HTTP response-body interruptions, but it
may be less reliable under heavy parallel-agent load. `auto` starts with
WebSocket and falls back to HTTP only if setup fails before a request is sent;
it cannot replay an interrupted in-flight request. Close every active Clodex
session after changing the transport so the shared supervisor restarts.

## Credentials and local files

Clodex uses its own sign-in from `clodex auth login` when there is one, and
otherwise reuses the Codex CLI's `~/.codex/auth.json` (or
`$CODEX_HOME/auth.json`) when Codex is authenticated in `chatgpt` mode.

Clodex's own sign-in is stored in `~/.clodex/auth/codex.json` with mode `0600`
in a `0700` directory. Clodex refreshes it before it expires, under a file lock
so concurrent sessions never spend the same refresh token twice. Signing in
uses the same OpenAI sign-in and OAuth client as the Codex CLI.

When it reuses the Codex CLI's login, Clodex:

- requires the credential to be a regular file owned by the current user;
- rejects symlinks and Unix permissions broader than `0600`;
- reads only the access token and optional account ID;
- never reads, copies, prints, or logs the Codex refresh token;
- never writes the original Codex credential file itself;
- asks Codex App Server's `account/read` API to refresh it, so Codex remains
  the only process that rotates that refresh token.

Either way, while the backend runs, an access-token-only adapter file is written
with mode `0600` under the Clodex runtime directory and removed when the
supervisor stops. Long-lived supervisors watch for credential changes and
replace it automatically.

Persistent and runtime files default to:

```text
~/.clodex/
├── config.json
├── auth/
│   └── codex.json            # after `clodex auth login`
├── cache/
│   ├── codex-models.json
│   └── updates.json           # hourly release-check cache
├── logs/
│   ├── claude-code-proxy/
│   │   └── proxy.log         # Codex backend log
│   ├── proxy.log             # external proxy output
│   └── supervisor.log
└── run/
    ├── supervisor.lock
    ├── control.sock          # active sessions only
    └── proxy/                # active sessions only
```

Set `CLODEX_HOME` to move this entire directory.

## Troubleshooting

Start with:

```sh
clodex doctor
clodex auth status
clodex auth sync
clodex context
```

- **“Agent terminated early” with “error decoding response body”:** this is an
  interrupted upstream Codex response. The proxy retries failures that are
  still safe to replay, but it cannot safely replay a partially emitted tool
  stream. Retry the failed agent after connectivity recovers. If interruptions
  persist and the workload does not use heavy agent concurrency, try
  `clodex config transport websocket`, close every Clodex session, and start a
  new one. Inspect `~/.clodex/logs/claude-code-proxy/proxy.log` for
  `codex_http_stream_failed` and `buffered_transport_retry_exhausted`.
- **“Run `/login`” or `403 WebSocket upgrade was rejected`:** update Clodex,
  run `clodex config transport http`, and restart all Clodex sessions. Confirm
  the configured transport and proxy version with `clodex doctor`.
- **“Prompt is too long” or `413 request_too_large` mid-session:** the
  conversation passed what Codex accepts. Run `clodex doctor` and compare
  "Context capacity" against the routed ceiling; a capacity above the ceiling
  means an older Clodex passed a configured value through unclamped. Update
  Clodex and start a new session. Note that the upstream 413 message carries no
  token counts, so Claude Code cannot size its compaction retry precisely and
  may fail to recover.
- **The prompt bar still shows a small window:** context changes affect new
  processes only. Close and restart the session, then run `clodex context`.
- **A trusted tool still prompts:** confirm the exact Claude tool identifier in
  `clodex config show`, then start a new session. The rule is injected at
  launch.
- **No Codex sign-in, or it expired:** run `clodex auth login`. When reusing a
  Codex CLI login instead, ensure file-backed credential storage is enabled and
  that the auth file is owned by you with mode `0600`.
- **“No refresh token stored” after a 401:** run `clodex auth sync`. This
  refreshes the sign-in, then replaces the running backend's stale
  access-token adapter.
- **A newly released Codex model is missing from `clodex models`:** Codex lists
  models by client version, and Clodex requests the catalog as the Codex
  version it was built against. Update Clodex to pick up models that need a
  newer client.
- **A newly released model is unsupported:** with the external proxy backend,
  update `claude-code-proxy` or switch back with `clodex config backend builtin`;
  Clodex refuses to start with a translator that cannot route the live mapping.
  The built-in backend routes every model in the live Codex catalog.
- **A proxy appears to remain after all sessions close:** wait for the
  one-second grace period, then inspect `~/.clodex/logs/supervisor.log` and
  `proxy.log`. A new session can safely remove a stale control socket while
  holding the supervisor lock.

## Development and tests

Run the same checks as CI:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
```

The test suite includes:

- unit tests for CLI dispatch, parsing, validation, rendering, model mapping,
  Claude routes and the picker, launch environment, credential safety, the
  Codex sign-in store and catalog cache, supervisor protocol, health checks,
  cleanup, and proxy compatibility matching;
- the vendored Codex backend's own unit and integration tests, under
  `crates/codex-backend`;
- CLI contract tests for help, version output, and installer syntax/help;
- process-level lifecycle tests that race eight supervisors, verify only one
  proxy starts, hold multiple leases, close the original lease first, check
  final-session shutdown and SIGTERM cleanup, and start the built-in backend.

CI is defined in `.github/workflows/ci.yml` and runs formatting, Clippy, and all
tests on both current Ubuntu and macOS runners for every push and pull request.
The lifecycle tests use a fake Codex login and a local fake proxy, so CI does
not need real Claude, Codex, credentials, or network access.

## Community and support

- Read [CONTRIBUTING.md](CONTRIBUTING.md) before proposing a change.
- Use [GitHub Issues](https://github.com/DeanDiasti/clodex/issues) for bugs and
  feature requests.
- Report vulnerabilities privately as described in
  [SECURITY.md](SECURITY.md).
- See [CHANGELOG.md](CHANGELOG.md) for release notes.
