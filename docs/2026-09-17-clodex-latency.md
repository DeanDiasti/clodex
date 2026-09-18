# Clodex latency and empty-response investigation

## Evidence

On September 17, 2026 (Pacific), through approximately 18:08, the proxy recorded
210 Astra failures out of 2,729 completed Astra requests. Every one returned
503 with `Codex completed without producing output`. Failed requests took
197–302 seconds, median 218 seconds. Luna and Sol had no failures in the same
sample. One request for `claude-fable-5-1` failed model routing with 400.

The installed proxy, version 0.1.35 at source commit
`55bf0b5818b461e1860964809726f99d2fd52c10`, allowed ten empty-response retries.
Its ten backoff sleeps total 157.5 seconds, excluding upstream request time.
Those retries were not logged. Claude Code has its own retry layer for 503s.

Clodex also awaited a quota fetch before forwarding a ready model response.
That endpoint has a ten-second timeout; concurrent cache misses could start
multiple fetches. Historical supervisor logs contain failures of that fetch.
This is an additional possible delay, not a measured explanation for every
slow request.

## Changes

- Clodex returns cached quota information immediately and refreshes it in one
  background task. Failed lookups are cached; cancellation releases the task
  marker.
- The proxy permits at most two empty-response retries. Replay work has a
  deadline 60 seconds after request start. The initial generation is not
  interrupted solely because it takes longer than 60 seconds. Once output is
  forwarded, the normal stream continues without this replay deadline.
- Exhausted empty responses carry `x-should-retry: false`, preventing the client
  from multiplying the already exhausted retry loop.
- Retry attempts and exhaustion are warnings; failed HTTP responses are errors.
  Empty streaming completions log terminal shape and usage, not prompt text.
- `claude-fable-5-1` routes to Astra.

The proxy changes are published on the
[`fix/clodex-empty-response-retries` branch](https://github.com/diastidean/claude-code-proxy/tree/fix/clodex-empty-response-retries)
and also live in the sibling `../claude-code-proxy` checkout. The patched version
is `0.1.35-clodex.1`; it is not an upstream release. Updating that binary from
another source can replace this patch.

## Validation and limits

- Clodex full test suite: 96 tests passed, one helper-process test ignored.
- Proxy unit suite: 864 passed; integration suite: 47 passed. Targeted retry
  tests were rerun after the final retry-header and deadline changes.
- Isolated live Astra and Sol requests succeeded over both HTTP and WebSocket
  in approximately 1.6–2.7 seconds. This does not establish that changing
  transport fixes failures under the original workload.
- Patched release binaries passed four additional live transport/model probes
  in 1.3–3.2 seconds. A fresh isolated Clodex session completed end-to-end
  with Astra in 4.91 seconds.
- Installed Claude Code 2.1.275, against a local simulated exhausted-response
  server, made exactly one request and displayed an explicit API error in
  0.66 seconds with `x-should-retry: false`.

The original failed upstream event payloads were not captured. Therefore it is
not yet established whether the empty output originated upstream or resulted
from translation. The added metadata distinguishes a terminal event with zero
output items from one whose output was not translated. These fixes remove
confirmed local delay/retry amplification; they do not prove upstream empty
responses can no longer occur.

## Activation

The installed executables are replaced atomically, with the previous binaries
saved under `~/.local/bin/clodex-backups/2026-09-17/`. Existing processes retain
the old executable. Close all Clodex sessions, then reopen/resume them so the
shared supervisor and proxy load the replacements. Active sessions are not
terminated by the installer.
