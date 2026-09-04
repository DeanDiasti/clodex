# Changelog

All notable changes to Clodex are documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and the project uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

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

- Haiku background requests now route to the fourth Codex catalog model, making
  the current mapping Astra → Fable, Sol → Opus, Terra → Sonnet, and
  Luna → Haiku.
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
