# Bug: opencode panes on OpenCode 1.19.3 report `ready` in slopctl forever; no state/transcript events flow

## Resolution

The failure was a stale **session binding**, not renamed SSE events. A read-only
capture from the affected pane's embedded server on OpenCode
`1.19.3.r4380.ga1f604b` showed normal `session.status` and `message.*` events for
`ses_ea0f97d82ffeq7ovytAfws2Gb4` (title `adb into TV`). slopd was filtering and
fetching history for `ses_ea0faa8f7ffez3TjrqMUft5J8d`, an idle initial session
with three messages. The HTTP API agreed with both observations.

`tui.session.select` is an API command **to** the TUI, not a notification of
native TUI navigation. Creating or selecting a session locally can therefore
leave slopd bound to the previous session. Separately, reconnecting SSE reset
its session filter to the original spawn-time ID even after a known selection.

The backend now recovers an idle binding from a single active top-level session
in that pane's embedded server, both on activity events and in the three-second
poll. Session metadata excludes subagents; the shared database's newest session
is never used as evidence. Recovery updates command routing, transcript reads,
the durable tmux session ID, and the SSE filter. Old retry bookkeeping is cleared,
and SSE reconnects consult the live binding. Recoveries, ambiguous bindings, API
failures, and stalled SSE connections produce warnings; status requests and SSE
connection/heartbeat waits are bounded.

There is no API exposing the TUI's current route. Navigation to an idle session
is detected when it produces activity. If the bound session is still active or
multiple top-level sessions are active, automatic recovery does not guess a new
binding. This fix does not add a new `unknown` state to the public state enum.

Validation includes a real OpenCode TUI with a local deterministic model
provider: native new-session navigation, busy/tool/live-transcript events, a
daemon restart with deliberately stale metadata, a real bash tool running for
over 30 seconds, and completion back to ready. All test resources use a private
tmux server and isolated configuration/data directories; a command guard rejects
tmux access outside the test socket. The same real-TUI test fails against the
original commit at the stale-session check and passes with the fix. Run it with:

```sh
OPENCODE_TEST_BINARY=/usr/bin/opencode cargo test -p slopd --all-features \
  --test opencode_live -- --ignored --nocapture
```

Mock integration coverage also forces an SSE reconnect after a session switch,
and HTTP fixture tests cover child sessions, concurrent roots, short turns,
missing metadata, retrying bound sessions, and malformed status responses.

The original incident report follows. Its version-wide workaround and suspected
event rename were hypotheses, superseded by the session-binding diagnosis above.

## Summary

An opencode-backend pane running OpenCode `1.19.3.r4380.ga1f604b` (tmux fork build)
shows `ready|ready` in `slopctl ps` / `slopctl ps --json` **while a run is verifiably
active** in the TUI. `slopctl wait --event StateChange` and `slopctl listen` never
fire for the pane, and `slopctl transcript` stays frozen at the pane's first few
records even after hundreds of KB of conversation. slopd's view of the pane is a
snapshot from around pane creation and never updates.

## Environment

- slopd uptime: ~260k s (long-running daemon), `subscribers: 0`
- Pane: opencode backend, session `ses_ea0faa8f7ffez3TjrqMUft5J8d`, pane `%19`
- Client TUI: `OpenCode 1.19.3.r4380.ga1f604b` (fork build)
- Last verified-good opencode backend parity work was against **1.17.x**

## Observed (single pane, same minutes)

1. `slopctl ps --json` → `"state":"ready","detailed_state":"ready"` while the TUI
   simultaneously showed an in-flight bash tool (spinner), a growing context
   (330.5K → 335.3K), and the busy footer (`esc interrupt`).
2. `timeout 6 slopctl listen --pane-id %19` during the active run → only
   `{"subscribed":true}`, zero events (no StateChange, no transcript records).
3. `slopctl transcript %19` → only the pane-creation records (first user prompt +
   one APIError reply); everything after invisible to slopd.
4. `slopctl send %19 continue` still **works** (prompt reaches the TUI and starts a
   run) but lands with ~30 s lag, and the sent user message never appears in
   `slopctl transcript`.

## Repro

1. `slopctl run -c /tmp` with an opencode backend on OpenCode ≥ 1.19.x.
2. Prompt it with anything that runs a tool for >30 s.
3. While the TUI shows the running tool: `slopctl ps` (says `ready`),
   `slopctl listen --pane-id %N` (silent), `slopctl transcript %N` (stale).

## Impact

- `slopctl wait --event StateChange` and any `ps`-state-based watcher (auto-continue,
  quota-recovery, orchestration) silently never trigger on these panes — worst kind
  of failure: no error, just no events.
- `slopctl ps` misleads humans and scripts about pane liveness.
- Transcript-dependent features (review relay, history) see a tiny frozen prefix.

## Suspected cause

The opencode SSE **bus event names/shapes changed between 1.17.x and 1.19.x**
(e.g. `session.status` / `session.idle` renamed, reshaped, or moved). slopd's
opencode backend subscribes to the 1.17-era event set; on 1.19.3 it receives
nothing it recognizes, so state never transitions and transcript records never
arrive. Prompt-sending goes through a different path (works, lagged), which matches
"control plane alive, event plane dead".

## Suggested fix

1. Re-capture the real SSE bus of a 1.19.x TUI (`GET /event`), re-map the events
   slopd needs: busy/idle transitions + message/tool records.
2. Make the backend fail loud: if a subscribed opencode pane produces zero bus
   events over N minutes while its HTTP session status endpoint says busy, log a
   warning and flag the pane state as unknown instead of `ready`.
3. Add an integration test that drives a real 1.19.x TUI through a tool call and
   asserts slopd observes busy → tool → idle (guards event-map regressions across
   opencode upgrades).

## Workaround (until fixed)

Do not trust `ps`/`wait`/`transcript` for opencode ≥1.19 panes:

```sh
# busy ⇔ footer shows "esc interrupt"
tmux capture-pane -t %N -p | tail -4 | grep -c 'esc interrupt'
```

Verify `slopctl send` success by the footer flipping to busy within ~60 s, not by
`ps` or transcript. (Adjacent gotcha: `tmux has-pane` does not exist; pane-existence
check is `tmux display-message -p -t %N '#{pane_id}'`.)
