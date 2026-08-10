# Security Policy

## Reporting a vulnerability

Please do not report security vulnerabilities in a public GitHub issue.

Use GitHub's private vulnerability reporting for this repository:

<https://github.com/DeanDiasti/clodex/security/advisories/new>

Include the affected version or commit, platform, impact, and a minimal
reproduction when possible. Do not include real credentials, access tokens,
account IDs, or the contents of `~/.codex/auth.json`.

You should receive an initial response within seven days. A fix timeline will
depend on severity and reproducibility. Please allow time for a patch before
public disclosure.

## Scope

Clodex handles a reusable local Codex login and starts a loopback translation
proxy, so reports involving credential exposure, unsafe file permissions,
authentication bypasses, unintended non-loopback access, or command execution
are especially important.

Only the latest release and the current `main` branch receive security fixes.
