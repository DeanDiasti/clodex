# Clodex

[![CI](https://github.com/DeanDiasti/clodex/actions/workflows/ci.yml/badge.svg)](https://github.com/DeanDiasti/clodex/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust 1.85+](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](https://www.rust-lang.org/tools/install)
[![macOS and Linux](https://img.shields.io/badge/platform-macOS%20%7C%20Linux-lightgrey.svg)](#requirements)

**Use Claude Code's agentic coding interface with subscription-backed OpenAI
Codex models.**

Clodex is a local, open-source launcher that runs Claude Code as the interactive
coding harness while routing model requests through your existing Codex CLI
login. It keeps Claude Code's UI, agents, tools, permissions, and workflows;
only the model transport and model aliases change for the launched process.

```text
Claude Code → loopback translation proxy → authenticated Codex session
```

Ordinary `claude` sessions are unaffected. Clodex does not require a separately
configured OpenAI API key: it reuses the existing file-backed ChatGPT login
owned by the Codex CLI.

> [!IMPORTANT]
> Clodex is an independent community project. It is not affiliated with,
> endorsed by, or supported by Anthropic or OpenAI.

## Why Clodex?

- Keep Claude Code's terminal experience, subagents, tool use, and permission
  controls.
- Use the models visible to an authenticated Codex CLI session without
  hard-coding model names.
- Run everything locally through a loopback-only translation proxy.
- Leave normal Claude Code sessions and global model settings untouched.
- Share one supervised proxy safely across concurrent Clodex sessions.

## Quick start

### 1. Requirements

- macOS or Linux
- [Codex CLI](https://developers.openai.com/codex/cli), logged in with ChatGPT
- [Claude Code](https://code.claude.com/docs/en/setup)
- [`claude-code-proxy`](https://github.com/raine/claude-code-proxy)

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

To build from source instead, install Rust 1.85 or newer and run:

```sh
git clone https://github.com/DeanDiasti/clodex.git
cd clodex
./scripts/install.sh --install-proxy
```

`--install-proxy` uses Homebrew only when the translation proxy is missing.
If every prerequisite is already present, use:

```sh
./scripts/install.sh
```

The source installer uses `~/.local` by default, producing
`~/.local/bin/clodex`.

### 3. Verify and run

```sh
clodex doctor
clodex auth status
clodex
```

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

The installer is repeatable. Update the checkout and run it again:

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

`CLODEX_INSTALL_ROOT` supplies the default for `--root`. To uninstall, remove
`<install-root>/bin/clodex`; remove `~/.clodex` as well only if the saved
configuration and logs are no longer wanted.

## Commands

| Command | Purpose |
| --- | --- |
| `clodex` | Start Claude Code through Clodex |
| `clodex auth [status]` | Validate secure reuse of the Codex login |
| `clodex models [list]` | Show visible, API-supported Codex models |
| `clodex models map` | Show Claude-role-to-Codex routing |
| `clodex models … --json` | Emit machine-readable model data |
| `clodex config [show]` | Show persistent defaults |
| `clodex config context <auto\|tokens>` | Set context capacity |
| `clodex config compact-at <1..95>` | Set the auto-compaction percentage |
| `clodex config allow-tool <exact-name>` | Trust one tool for sessions and subagents |
| `clodex config forget-tool <exact-name>` | Remove a trusted tool |
| `clodex config path` | Print the configuration path |
| `clodex context` | Show the effective capacity and compaction trigger |
| `clodex doctor` | Check installed tools, login, and local paths |

Use `clodex --help` or `clodex <command> --help` for generated command help.

## Model routing

On every launch, Clodex reads the authenticated catalog from
`codex debug models`, removes hidden or API-unsupported entries, and maps
catalog priority to Claude Code roles:

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

No model names are hard-coded. This allows the mapping to follow the live
Codex catalog, while a preflight check ensures the installed translation proxy
also understands every selected model.

## Fast mode

Inside a Clodex session, `/fast on` enables the Codex priority service tier for
the model that is already selected. It does not switch the route to Fable,
Opus, Sonnet, or another model. `/fast off` returns that same model to the
standard service tier.

Clodex implements this with a loopback-only bridge owned by the shared
supervisor. Requests are tracked independently by Claude session and subagent,
so concurrent sessions can use different models and fast-mode settings. The
bridge marker and Claude fast-mode override are injected only into the child
process launched by `clodex`; normal `claude` sessions and global Claude
settings are not changed. This requires `claude-code-proxy` 0.1.32 or newer.

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

The default `auto` setting uses the smallest standard context window reported
by the three routed catalog models:

```sh
clodex config context auto
```

An explicit value opts into a larger model window and is passed through
unchanged:

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

This explicit override matters because Claude Code otherwise applies a
conservative window to an unfamiliar model behind a custom Anthropic base URL.
The Codex catalog may report a smaller standard or usage threshold even when a
model supports an extended window. For example, a configured `600k` capacity
with compaction at `90` produces a 540,000-token trigger. The upstream model
remains authoritative and will reject a value beyond its real capability.

Settings apply when a new Clodex process starts. Restart existing sessions
after changing them. Subagents inherit the launch environment and therefore
receive the same capacity and percentage, but each agent has its own context
window.

## Reasoning effort

Model routing and reasoning effort are independent. Use Claude Code's
`/effort` control or its `--effort` option. Claude sends the selected effort
through the translation proxy as the Codex reasoning level; Clodex does not
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
concurrently.

## Credentials and local files

Clodex reuses `~/.codex/auth.json` (or `$CODEX_HOME/auth.json`) when Codex is
authenticated in `chatgpt` mode. It:

- requires the credential to be a regular file owned by the current user;
- rejects symlinks and Unix permissions broader than `0600`;
- reads only the access token and optional account ID;
- never reads, copies, prints, or logs the Codex refresh token;
- never writes the original Codex credential file itself.

While the proxy runs, an access-token-only adapter file is written with mode
`0600` under the Clodex runtime directory. It is removed when the supervisor
stops. Clodex requests managed refreshes through Codex App Server's
`account/read` API, so Codex remains the only process that reads, rotates, and
persists the refresh token. Long-lived supervisors also watch for native Codex
credential changes and replace the access-token adapter automatically.

Persistent and runtime files default to:

```text
~/.clodex/
├── config.json
├── logs/
│   ├── proxy.log
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

- **“Run `/login`” or `403 WebSocket upgrade was rejected`:** update Clodex
  and restart all Clodex sessions. Current launches force the proxy's HTTP SSE
  transport. Confirm the proxy version with `clodex doctor`.
- **The prompt bar still shows a small window:** context changes affect new
  processes only. Close and restart the session, then run `clodex context`.
- **A trusted tool still prompts:** confirm the exact Claude tool identifier in
  `clodex config show`, then start a new session. The rule is injected at
  launch.
- **Codex credentials are unavailable:** run `codex login`, ensure file-backed
  credential storage is enabled, and check that the auth file is owned by you
  with mode `0600`.
- **“No refresh token stored” after a 401:** run `clodex auth sync`. This asks
  Codex to refresh its managed login, then replaces the active proxy's stale
  access-token adapter without copying the refresh token.
- **A newly released model is unsupported:** update
  `claude-code-proxy`; Clodex refuses to start with a translator that cannot
  route the live mapping.
- **A proxy appears to remain after all sessions close:** wait for the
  one-second grace period, then inspect `~/.clodex/logs/supervisor.log` and
  `proxy.log`. A new session can safely remove a stale control socket while
  holding the supervisor lock.

## Development and tests

Run the same checks as CI:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --all-targets --locked
```

The test suite includes:

- unit tests for CLI dispatch, parsing, validation, rendering, model mapping,
  launch environment, credential safety, supervisor protocol, health checks,
  cleanup, and proxy compatibility matching;
- CLI contract tests for help, version output, and installer syntax/help;
- a process-level lifecycle test that races eight supervisors, verifies only
  one proxy starts, holds multiple leases, closes the original lease first,
  checks final-session shutdown, and verifies SIGTERM cleanup.

CI is defined in `.github/workflows/ci.yml` and runs formatting, Clippy, and all
tests on both current Ubuntu and macOS runners for every push and pull request.
The lifecycle test uses a local fake proxy, so CI does not need real Claude,
Codex, credentials, or network access.

## Community and support

- Read [CONTRIBUTING.md](CONTRIBUTING.md) before proposing a change.
- Use [GitHub Issues](https://github.com/DeanDiasti/clodex/issues) for bugs and
  feature requests.
- Report vulnerabilities privately as described in
  [SECURITY.md](SECURITY.md).
- See [CHANGELOG.md](CHANGELOG.md) for release notes.
