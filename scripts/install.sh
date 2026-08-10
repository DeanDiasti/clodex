#!/usr/bin/env bash

set -euo pipefail

readonly MINIMUM_RUST_MAJOR=1
readonly MINIMUM_RUST_MINOR=85

script_directory="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
source_directory="$(cd -- "${script_directory}/.." && pwd)"
install_root="${CLODEX_INSTALL_ROOT:-}"
install_proxy=false
skip_prerequisite_checks=false

usage() {
  cat <<'EOF'
Install or update Clodex from this source checkout.

Usage:
  ./scripts/install.sh [options]

Options:
  --root <directory>           Install under this directory (default: ~/.local).
  --install-proxy              Install claude-code-proxy with Homebrew if absent.
  --skip-prerequisite-checks   Skip checks for Claude, Codex, and the proxy.
  -h, --help                   Show this help.

Environment:
  CLODEX_INSTALL_ROOT          Alternative default for --root.

Re-run this script after updating the checkout to replace an older Clodex
binary. Cargo.lock is honored so installs and updates use the locked dependency
versions.
EOF
}

fail() {
  printf 'clodex installer: %s\n' "$*" >&2
  exit 1
}

command_exists() {
  command -v "$1" >/dev/null 2>&1
}

require_command() {
  command_exists "$1" || fail "$2"
}

check_rust_version() {
  local version major minor
  version="$(rustc --version | awk '{print $2}')"
  major="${version%%.*}"
  version="${version#*.}"
  minor="${version%%.*}"

  if ((major < MINIMUM_RUST_MAJOR)) ||
    ((major == MINIMUM_RUST_MAJOR && minor < MINIMUM_RUST_MINOR)); then
    fail "Rust ${MINIMUM_RUST_MAJOR}.${MINIMUM_RUST_MINOR} or newer is required; found $(rustc --version)"
  fi
}

while (($# > 0)); do
  case "$1" in
    --root)
      (($# >= 2)) || fail "--root requires a directory"
      install_root="$2"
      shift 2
      ;;
    --install-proxy)
      install_proxy=true
      shift
      ;;
    --skip-prerequisite-checks)
      skip_prerequisite_checks=true
      shift
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      fail "unknown option: $1 (run with --help for usage)"
      ;;
  esac
done

case "$(uname -s)" in
  Darwin | Linux) ;;
  *) fail "Clodex currently supports macOS and Linux" ;;
esac

if [[ -z "${install_root}" ]]; then
  [[ -n "${HOME:-}" ]] || fail "HOME is not set; pass an installation prefix with --root"
  install_root="${HOME}/.local"
fi
[[ -n "${install_root}" ]] || fail "the installation root cannot be empty"

[[ -f "${source_directory}/Cargo.toml" ]] ||
  fail "Cargo.toml was not found at ${source_directory}; run this script from a Clodex checkout"
[[ -f "${source_directory}/Cargo.lock" ]] ||
  fail "Cargo.lock was not found; refusing an unlocked install"

require_command cargo "Cargo is required. Install Rust 1.85 or newer, then retry."
require_command rustc "rustc is required. Install Rust 1.85 or newer, then retry."
check_rust_version

if [[ "${install_proxy}" == true ]] && ! command_exists claude-code-proxy; then
  require_command brew "--install-proxy requires Homebrew: https://brew.sh"
  printf 'Installing claude-code-proxy with Homebrew...\n'
  brew install raine/claude-code-proxy/claude-code-proxy
fi

if [[ "${skip_prerequisite_checks}" == false ]]; then
  missing=()
  command_exists codex || missing+=("Codex CLI (https://developers.openai.com/codex/cli)")
  command_exists claude || missing+=("Claude Code (https://code.claude.com/docs/en/setup)")
  command_exists claude-code-proxy ||
    missing+=("claude-code-proxy (rerun with --install-proxy, or install it separately)")

  if ((${#missing[@]} > 0)); then
    printf 'Missing required runtime prerequisites:\n' >&2
    for prerequisite in "${missing[@]}"; do
      printf '  - %s\n' "${prerequisite}" >&2
    done
    exit 1
  fi
fi

mkdir -p -- "${install_root}"
install_root="$(cd -- "${install_root}" && pwd)"

printf 'Installing Clodex from %s into %s...\n' "${source_directory}" "${install_root}"
cargo install \
  --path "${source_directory}" \
  --root "${install_root}" \
  --locked \
  --force

installed_binary="${install_root}/bin/clodex"
[[ -x "${installed_binary}" ]] ||
  fail "Cargo completed but ${installed_binary} is not executable"

printf '\n%s\n' "$("${installed_binary}" --version)"
printf 'Installed successfully at %s\n' "${installed_binary}"

case ":${PATH}:" in
  *":${install_root}/bin:"*) ;;
  *)
    printf '\nAdd this directory to PATH:\n  %s/bin\n' "${install_root}"
    ;;
esac

printf '\nNext checks:\n'
printf '  clodex doctor\n'
printf '  clodex auth status\n'
