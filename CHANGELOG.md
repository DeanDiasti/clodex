# Changelog

All notable changes to Clodex are documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and the project uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- Ask Claude Code to replay full conversation history when it attempts native
  message threads on Codex routes, preventing missing tool-call errors after
  tools run. Claude routes retain their native message-thread behavior.

## [0.2.0] - 2026-10-20

### Added

- Native Remote Control while retaining OpenAI models: run `/remote-control`
  or `/rc` inside a Clodex session, or launch with `clodex --remote-control`.
  Claude subscription sessions now use an authenticated, session-local HTTPS
  proxy with a temporary process-scoped certificate. Global certificate trust
  remains unchanged. Use `clodex config remote-control off` to restore the
  previous transport. Existing outbound proxy chaining and standalone Remote
  Control server mode are not supported.

### Fixed

- Recover promptly after macOS Wi-Fi, hotspot, VPN, and network configuration
  changes, refreshing connections and safely retrying requests only before
  output has been delivered.
- Isolate concurrent Claude requests using HTTP/1.1 connections.
- Keep Codex streams alive while waiting for initial output.
- Avoid completing tool calls from interrupted streams and improve Claude
  transport failure logging.

## [0.1.1] - 2026-10-09

### Changed

- The Codex CLI is optional. `clodex auth login` signs in to Codex with a
  ChatGPT account (`--device` for machines without a browser), Clodex
  refreshes that sign-in itself, and the model catalog is fetched from Codex
  directly and cached. An existing Codex CLI login is still reused when
  Clodex has no sign-in of its own.
- The built-in Codex backend is the default. `claude-code-proxy` is no longer
  required; `clodex config backend proxy` selects it.
- The minimum supported Rust version is now 1.88.

### Added

- Rolling updates: new sessions use the newly installed binary's deployment,
  while existing sessions keep their original backend and status-line helper.
  Each build has separate runtime files and logs, and its backend, credentials,
  and executable snapshot are cleaned up after its final session ends.
- Background checks for stable Clodex releases, an update-available notice in
  the session status line, and `clodex update` to verify and install a release
  in place without a Rust toolchain.
- A built-in Codex backend (`clodex config backend builtin`), vendored from
  claude-code-proxy v0.1.42's Codex path, that runs inside the supervisor so
  no external proxy is needed. It routes every model in the live Codex
  catalog.
- Claude models on your own Claude subscription with `clodex config route`.
  The bridge forwards Claude-routed requests to Anthropic with Claude Code's
  own login, strips that credential from every Codex-bound request, and
  `clodex doctor` reports the Claude login.
- Every routable Codex model, and with a subscription every current Claude
  model, in the `/model` picker, plus one subagent type per model so subagents
  can run on either provider.
- Configurable Codex transport with `clodex config transport`, while retaining
  HTTP SSE as the concurrency-safe default.
- Configured transport reporting in `clodex doctor` and recovery guidance for
  interrupted Codex response streams.
- Codex server-side compaction, enabled for every launched proxy, so Codex can
  compact upstream instead of rejecting a prompt near the model's limit.
- Opt-in hierarchical compaction (`clodex config hierarchical-compaction`),
  which folds an oversized compaction request into successive rounds that each
  fit the context window, so a conversation past the ceiling can still compact.
- Context capacity reporting in `clodex doctor`, and a launch warning when a
  configured capacity is clamped.

### Fixed

- Transient HTTP connection failures are classified using their underlying
  error causes, so dropped connections can enter the bounded retry policy
  instead of immediately failing as generic network errors. Retries still
  stop after semantic output has been forwarded.
- Retry delays include randomized jitter to spread concurrent agents' retry
  attempts. Explicit upstream `Retry-After` values remain honored.
- Concurrent backend log writes and rotation are serialized so each line
  remains a complete JSON record. HTTP failures and retries now log their
  underlying causes and retry decisions.
- Switching to a Codex model with `/model <name>` no longer waits a second or
  more on the first switch. Clodex lists its models as Claude Code's curated
  `modelPicker` rows, so Claude Code no longer checks each one with a
  one-token request to Codex.
- Restored `clodex --fast` as a session-wide priority policy for supported
  Codex models, including subagents, model switches, and compaction, with
  standard-speed fallback for unsupported models and unchanged Claude routes.
- Restored `clodex statusline-fast` so custom status-line scripts can show
  the current model's fast status without accidentally launching Claude Code.
- `auto` context capacity now follows the catalog's extended
  `max_context_window` and `effective_context_window_percent` instead of the
  smaller standard usage threshold.
- A configured context capacity above what the routed Codex models accept is
  clamped to the routed ceiling. Passing it through left Claude Code
  auto-compacting past the point where every request is rejected, and a
  rejected compaction request cannot recover.

## [0.1.0] - 2026-08-09

### Added

- Claude Code launcher backed by the authenticated Codex model catalog.
- Dynamic Fable, Opus, Sonnet, and Haiku-compatible model routing.
- Shared loopback proxy supervision across concurrent sessions.
- Per-session model and fast-mode routing.
- Configurable context capacity, compaction threshold, and trusted tools.
- Secure reuse and managed refresh of file-backed Codex credentials.
- Diagnostic, configuration, authentication, context, and model commands.
- Repeatable source installer for macOS and Linux.
- Automated tests and GitHub Actions CI on Ubuntu and macOS.

[Unreleased]: https://github.com/DeanDiasti/clodex/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/DeanDiasti/clodex/releases/tag/v0.2.0
[0.1.1]: https://github.com/DeanDiasti/clodex/releases/tag/v0.1.1
[0.1.0]: https://github.com/DeanDiasti/clodex/releases/tag/v0.1.0
