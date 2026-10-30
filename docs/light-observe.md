# Optional read-only observation

`herdr-flow light observe` reads one explicitly registered binding. It never dispatches work, resumes a session, replays a report, retries CI, or changes `tasks.reported_state`. It does not import the old `watch_service` lifecycle or scan terminal contents. Record commands remain usable without Herdr or notification configuration.

## Invocation

After explicitly creating a project, task and binding with a session:

```sh
herdr-flow light --data-root /absolute/private/light observe --once \
  --project 1 --task 1 --binding 1 --service local \
  --socket /absolute/herdr.sock
```

`--service` must equal the binding's reported service label; the socket is supplied explicitly, never discovered by scanning. The returned workspace, pane, agent and full session reference must match the binding. A missing or changed session produces `unknown`; it never adopts another session or rewrites the binding. Service labels are operator-provided endpoint mappings, not authenticated server names. Unix peer credentials must match the current user.

Use `--watch` for a foreground observer. `--samples N` bounds a controlled run; without it the command runs until stopped. There is no installation, daemon, profile or automatic restart. A per-binding advisory lock prevents competing observers in the same data root; lock contention exits 5 (`busy`). Different data-root copies do not share locks or notification budgets.

`--show --project 1 --task 1 --binding 1` queries the last cached observation and up to 20 recent notification attempts without contacting Herdr or reading a Bark key. A missing cache returns an error; queries do not initialize it. Use `task show` separately for authoritative reported state and report history.

Default interval is 1000 ms (allowed 100..60000), each request timeout 500 ms (50..5000), and freshness window 15000 ms (100..3600000). The freshness window must cover the interval plus two request timeouts. Each socket reply is limited to 65536 bytes; each event-drain interval processes at most 32 hints. Requests are sequential; interval overruns do not create overlapping reads. Output is JSON per observation, plus a final stopped record for a bounded watch. Output failure terminates observation.

## Evidence and continuity

The implementation uses one current public JSON API, with `ping`, `agent.get`, and pane-scoped `events.subscribe` only. It does not run a second legacy adapter. Client/server version equality, minimum versions and version allowlists are not runtime requirements. Returned server version and observer package version are optional diagnostics, never compatibility predicates. Unknown fields are ignored; missing fields and unsupported responses remain explicit uncertainty.

Events invalidate the current view: their payloads are never applied as historical state. The observer subscribes before reading, then reads current state at bounded intervals. Duplicate hints do not append report events. An `events_lost` error or disconnected subscription discards continuity, reconnects at the next interval and establishes a new baseline. Missing history is not reconstructed. Subscription failure still permits bounded current-state reads, but cannot establish continuous-event evidence.

The local service-instance fingerprint uses the connected peer's UID/PID, Linux process start time, boot ID and endpoint inode. This is local observation identity, not a remote server-issued identifier. If identity cannot be read reliably, the observation is unknown. A different instance, binding/session, expired baseline, or decreasing sequence starts a new baseline. `completion_transition` only reports an increased available completion sequence in uninterrupted matching context; it is **not** business success, an audit result, or permission to resume work. `done` and `idle` both display `idle`, never task success. Working without output remains working; only the service's current explicit blocked state requests intervention.

Cached age is evaluated again on `--show`. Expired observations and clock rollback become unknown. Normal bounded observer exit stores `observer_stopped` and unknown. A killed process cannot write a final record: offline queries verify the recorded observer PID/start-time against the local process table and display unknown when it has exited or cannot be verified. Otherwise a cached watch record is explicitly a freshness lease, not proof that its event loop is responsive, and becomes unknown on expiry. There is no background crash detector. Once results remain point-in-time observations until expiry.

## Storage and notifications

`observations.sqlite3` is separate from the four authoritative record tables. It stores one latest snapshot per observed binding and at most 100 notification attempts globally; repeated unchanged reads update the snapshot rather than append report events. It uses the same private-root, explicit forbidden-directory overlap, local-filesystem and side-file protections as record storage: directory 0700, files 0600, WAL and full synchronization. Observation leases are private `observe-ID.lock` files whose kernel locks release on exit. Record backup includes records only; this derived cache and its local deduplication history are not copied by record backup. Reusing a different data root starts a separate notification budget.

Notifications are opt-in with `--bark-key-file /absolute/private/key`. The file must be a regular, single-link file owned by the current user with mode 0600, at most 256 bytes, containing an alphanumeric/underscore/hyphen key (surrounding whitespace is trimmed). No key is placed in the database, command arguments, result JSON or logs. The default endpoint is `https://api.day.app/push`; `--bark-endpoint https://HOST/push` permits a self-hosted service. Plain HTTP is restricted to numeric loopback substitutes. Endpoint URLs cannot contain credentials, query strings or a key path.

Only a fresh, context-matching `blocked` observation is eligible. The notification contains numeric task/binding IDs, a fixed blocked reason and an instruction to inspect; no title, session ID, terminal output, code or report summary is sent. It never wakes an Agent. A durable latch keyed by binding and service instance suppresses repeated blocked observations, including after restart, event duplication or sequence changes. Only a subsequently observed reliable working/idle state clears the latch. Unknowns and gaps do not invent an intervening unblocked episode. A database-wide limit of three reserved attempts per rolling hour applies to accepted, failed and unknown outcomes alike; limit/suppression is visible in observation output.

The attempt is durably reserved as `unknown` before sending, so a process crash or uncertain response cannot cause an automatic resend. Notification attempts retain their numeric ID, timestamp and fixed, redacted reason code for offline queries. Deduplication and rate limiting have separate output statuses. `accepted` means HTTP 200 and Bark API code 200; it does not prove delivery to a device. Definite HTTP/API rejection is `failed`. Network errors, timeouts, malformed/oversized responses or ambiguous status are `unknown`. There is no automatic retry. A database failure after sending may leave the durable reservation unknown.

The notification leaf invokes `/usr/bin/curl` once, supplying the key in JSON on stdin. It retains the tool's existing network/proxy configuration, overrides tracing/output destinations to avoid logging response bodies, limits transfer time to three seconds and response size to 4096 bytes, and imposes an outer four-second child deadline. Only that child is terminated on deadline; no Herdr or team process is signalled. Notification configuration is optional; without it observation and offline queries work normally.

## Capability basis and acceptance boundary

On 2026-10-30 the stable documentation index identified Herdr 0.9.5. The installed client reported 0.9.5; the running server reported 0.9.4/protocol22. A single corrected, bounded developer-self probe verified ping, matching `agent.get` and pane-scoped subscription acknowledgement. Ordinary requests use independent connections. The actual response included state-change sequence but no completion sequence or server-issued instance ID. Subscription acceptance did not prove a real transition or lost-event recovery. Those cases require controlled substitutes, not disruption of the live team. The implementation is unified; service upgrade is separately scheduled per the user's clarification.

Sources: [current stable documentation index](https://herdr.dev/llms.txt), [documented socket API revision](https://github.com/herdrdev/herdr/blob/v0.9.5/docs/next/website/src/content/docs/socket-api.mdx), [Bark POST API](https://github.com/Finb/bark-server/blob/master/docs/API_V2.md). These source revisions describe the capability investigation, not runtime version constraints. This batch does not install a service, replace a shared tool, or verify real-device delivery.
