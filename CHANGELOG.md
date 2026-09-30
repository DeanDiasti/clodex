# Changelog

All notable changes to Clodex are documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and the project uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- Switching to a Codex model with `/model <name>` no longer waits a second or
  more on the first switch. Clodex lists its models as Claude Code's curated
  `modelPicker` rows, so Claude Code no longer checks each one with a
  one-token request to Codex.

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

- Restored `clodex --fast` as a session-wide priority policy for supported
  Codex models, including subagents, model switches, and compaction, with
  standard-speed fallback for unsupported models and unchanged Claude routes.
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

[Unreleased]: https://github.com/DeanDiasti/clodex/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/DeanDiasti/clodex/releases/tag/v0.1.0
