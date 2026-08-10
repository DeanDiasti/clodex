# Contributing to Clodex

Thanks for helping make Clodex more reliable and easier to use.

## Before you start

- Search [existing issues](https://github.com/DeanDiasti/clodex/issues) before
  opening a new one.
- Use an issue for significant behavior changes so the approach can be
  discussed before implementation.
- Never include Codex credentials, access tokens, account IDs, or the contents
  of `~/.codex/auth.json` in an issue, test fixture, log, or commit.

## Development setup

Clodex requires Rust 1.85 or newer. Clone the repository and run:

```sh
cargo build --locked
cargo test --all-targets --locked
```

Real Claude, Codex, and proxy credentials are not needed for the automated test
suite. Runtime testing requires the prerequisites listed in the
[README](README.md#requirements).

## Before opening a pull request

Run the same checks as CI:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --all-targets --locked
```

Keep pull requests focused, explain user-visible behavior changes, and add or
update tests when behavior changes. Update the README when a command,
configuration option, prerequisite, or installation step changes.

By participating, you agree to follow the [Code of Conduct](CODE_OF_CONDUCT.md).
