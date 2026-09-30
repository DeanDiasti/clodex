# codex-backend

The Anthropic Messages to Codex Responses translator that Clodex runs inside
its supervisor when `clodex config backend builtin` is selected.

## Origin

This crate is vendored from
[claude-code-proxy](https://github.com/raine/claude-code-proxy) at tag
`v0.1.42` (commit `1e30e30`), copyright Claude Code Proxy contributors, under
the MIT License reproduced in [LICENSE](LICENSE).

## Changes from upstream

- Only the Codex provider is kept. The Cursor, Grok, Kimi, and OpenCode
  providers, the terminal UI, and the executable are removed, along with the
  tests that covered them.
- The registry and model table accept every model in the live Codex catalog
  that Clodex installs at startup (`embedded::install_catalog`), and take each
  model's Responses lane from that catalog. The vendored model lists remain
  the fallback.
- `embedded::EmbeddedServer` runs the server on a loopback port inside another
  process.

Configuration is still read from the same `CCP_*` environment variables, which
Clodex sets for its supervisor. Clodex supplies the Codex access token through
the same `auth.json` file it wrote for the external proxy.
