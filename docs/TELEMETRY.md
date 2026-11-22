# Telemetry (in-app timing)

jumpkey can record where the time goes in every hotkey press, and how
the picker is used in practice (how soon after switching windows you
press, how often you press again). It's a development tool for
measuring performance before and after changes, not something that
ships to users:

- Only in builds with the `telemetry` cargo feature. Off by default and
  never in the released MSI. Without the feature the instrumentation
  compiles to nothing.
- Nothing leaves the machine: jumpkey has no network code. Records are
  JSON lines in local files; analysis happens in separate scripts.
- No window titles, element names, URLs or page text. Records hold ids,
  counts, timings, enum values and exe file names only (see
  [Privacy](#privacy)).

The black-box benchmark (`tools/pick-bench`, see `bench/README.md`)
measures the same presses from outside the process; telemetry explains
its numbers.

## Running it

```powershell
cargo script run-telemetry
```

This quits the running jumpkey, installs a telemetry build over the copy
in `~/.cargo/bin` (`cargo install --features telemetry`, so the
launch-at-startup entry keeps pointing at the same path and telemetry
keeps running after a reboot) and starts it. Use jumpkey as usual.
`cargo script install` switches back to the normal build.

Records go to `%LOCALAPPDATA%\jumpkey\telemetry\telemetry.YYYY-MM-DD.jsonl`
(one file per day, 14 days kept). With the feature on, the normal log
(`%LOCALAPPDATA%\jumpkey\jumpkey.*.log`) also gets one summary line per
press:

```
telemetry: pick element 1062.4 ms (picked): refresh_layout 0.0, capture_ctx 0.0, enumerate 861.1, sort 0.2, labels 2.3, overlay_state 9.0, render 6.8, show_focus 4.4, perform 21.5; shown at 884.7 ms
```

## Records

One JSON object per line. Every record has `record` (its kind),
`schema` (format version, currently 1) and `session` (random id per
jumpkey run). `bench/schema-example.jsonl` has one example of each; a
test checks the real output against it.

### `session`

Written once at startup.

| Field | Meaning |
|---|---|
| `app_version`, `git_sha`, `git_dirty`, `profile` | Which build. `git_sha` is `unknown` outside a git checkout. |
| `pid` | Process id, to match harness runs. |
| `start_unix_nano` | Wall-clock start. |
| `qpc_hz`, `tsc_hz` | Clock rates: `QueryPerformanceCounter` ticks/s and CPU cycle-counter rate (`null` if not calibrated). |
| `machine` | CPU model, cores, RAM, Windows build, VM guess, display adapter, monitors (position, size, DPI), power source and plan. `host_hash` is a hash of the computer name, never the name. Same fields as the harness's run manifest. |
| `config` | Scope mode, hint order, alphabet length, caching on/off, cache TTL, max elements. |

### `pick`

One per hotkey press of the element picker (`attributes.kind =
"element"`) or window picker (`"window"`), written when the press is
fully handled (after the action ran, or after Esc).

| Field | Meaning |
|---|---|
| `trace_id`, `seq` | Ids within the session. |
| `start_unix_nano`, `start_qpc` | When the handler started. `start_qpc` is a raw system-wide QPC value, comparable with the harness's QPC timestamps. |
| `dur_us` | Handler start → end, including the time the overlay waited for your keys. |
| `thread`, `cpu_cycles`, `cpu_us` | Thread id and CPU used by the handler thread while the pick was running (inclusive of child spans on that thread). |
| `attributes` | See below. |
| `events` | Milestones: `{name, t_us, span}`, `t_us` relative to `start_qpc` (negative for `hotkey`, which happened before the handler started). |
| `spans` | Child spans: `{id, parent, name, start_us, dur_us, thread, cpu_cycles, cpu_us, attributes}`. The root is id 0. |
| `dropped_fields` | Fields removed by the privacy filter. Should be 0; anything else is a bug in the instrumentation. |
| `incomplete` | `true` if the pick never finished (written at shutdown, or after 10 minutes). |
| `record_build_us` | Time spent building this record (overhead check). |

`pick` attributes:

| Attribute | Meaning |
|---|---|
| `kind` | `element` or `window`. |
| `scope` | Scope mode at the time. |
| `count` | Hints shown. |
| `outcome` | `picked`, `cancelled`, `empty` (nothing to show) or `error`. |
| `cancel_by` | Only when `outcome` is `cancelled`: how the overlay was closed. `esc`: Esc pressed. `click`: focus moved away within 1 s of a mouse button press (a click outside the badges). `focus_lost`: focus moved away with no recent click (Alt+Tab, the Windows key, another window taking focus). `wheel`: the mouse wheel was turned. `quit`: jumpkey's message loop ended. `unknown`: none of these was recorded. |
| `modifier` | Modifier on the completing keystroke: `none`, `ctrl`, `shift` (element picker). |
| `keystrokes`, `backspaces` | Characters typed and backspaces pressed in the overlay. |
| `hotkey_source` | `message`: `WM_HOTKEY` was seen by the main loop and `queue_ms` / the `hotkey` milestone are exact. `dispatch`: it arrived through another message loop (splash screen, Settings, an open overlay), so the `hotkey` milestone is the handler start. |
| `queue_ms` | How long `WM_HOTKEY` waited in the queue before the main loop got it. |
| `since_foreground_change_ms` | Time since a different window last came to the front. Focus returning to the same window (e.g. when the overlay closes) doesn't count as a change. Absent until the first change after startup. |
| `since_last_pick_ms` | Time since the previous press finished. Absent for the first press. |
| `same_window_as_last_pick` | Whether the foreground window is the same as at the previous press. |
| `process_cpu_cycles` | CPU used by all of jumpkey's threads during the press (includes the parallel walk workers). |
| `private_bytes_before`, `private_bytes_after` | jumpkey's private memory at start and end. |

### Spans

| Span | What it covers | Attributes |
|---|---|---|
| `refresh_layout` | Checking the keyboard layout, rebuilding the alphabet if it changed. | `rebuilt` |
| `capture_ctx` | Snapshot of foreground window, cursor, monitor. | |
| `enumerate` | Finding the elements (all scopes). | `cache` (`miss`, `hit`, `hit_rejected`; active-window scope only), `collected` |
| `list_windows` | Listing and planning top-level windows (multi-window scopes). | `candidates`, `planned` |
| `parallel_walks` | Walking other apps' windows on worker threads. | `groups` |
| `worker` | One worker's share (one app), including its UI Automation setup. | `pid`, `windows` |
| `walk` | One window's UI Automation walk. | `kind` (`browser`/`desktop`), `foreground`, `process.executable.name`, `visited`, `pre_dedup`, `collected`, `max_depth`, `hit_depth_cap`, `hit_element_cap`, `attempts` (1 + retries of a browser page that looked unbuilt, #53), `documents` and `empty_documents` (page documents seen, and on-screen ones with no children, in the last attempt), `tree_cached` (browser: the whole tree was fetched in one call; `false` = node-by-node fallback) |
| `activate`, `root`, `search`, `dedup` | Inside `walk`: waking Chromium's accessibility, fetching the root element, the tree search, removing nested duplicates. Browser walks fetch the root together with the whole tree inside `search` (span `fetch_tree`); `root` then only appears on the fallback path. | |
| `register` | Assigning element ids. | `n` |
| `sort`, `labels` | Ordering elements by distance, generating labels. | `n` |
| `overlay_create` | Creating the overlay window and its buffer (first press only). | |
| `overlay_state` | Laying out and preparing the hints. | `n` |
| `render` | Drawing the first frame and handing it to Windows. | |
| `show_focus` | Showing the overlay and taking keyboard focus. | |
| `perform` | Running the chosen action. | `action`, `invoke_path`, `ok` |

Span names and attributes may change as the code changes (bump `schema`
when one is renamed or removed). The milestones below are the stable
part.

### Milestones (contract)

Any refactor of the pick path must keep emitting these, at the same
meaning, so measurements from before and after can be compared. The
analysis scripts treat a missing milestone as an error.

| Milestone | Element picker | Window picker | Meaning |
|---|---|---|---|
| `hotkey` | ✓ | ✓ | The main loop received `WM_HOTKEY` (see `hotkey_source`). |
| `enum_start` | ✓ | | Element search starts. |
| `elements_ready` | ✓ | | Elements found. |
| `labels_ready` | ✓ | | Labels assigned. |
| `first_ulw` | ✓ | ✓ | First frame handed to Windows (`UpdateLayeredWindow`), not yet visible. |
| `shown` | ✓ | ✓ | Overlay shown and focused. |
| `key_complete` | ✓ | ✓ | Label completed or Esc pressed. |
| `action_done` | ✓ | ✓ | Action finished (only when something was picked). |

`elements_ready`, `labels_ready` are absent when nothing was found
(`outcome = "empty"`).

## Privacy

The telemetry layer keeps number and true/false fields, and string
fields only from a short whitelist (`kind`, `scope`, `outcome`,
`modifier`, `cache`, `action`, `invoke_path`, `hotkey_source`,
`process.executable.name`, `milestone`), cut to 64 characters. Any other
string field (a title, an element name) is dropped and counted in
`dropped_fields`. Window handles are compared in memory
(`same_window_as_last_pick`) but never written.

## Overhead

With the feature off: none (nothing is compiled). With it on: about 20–40
spans per press, each costing well under a microsecond plus two CPU
cycle-counter reads; the record is built after the press on the main
thread (`record_build_us`, typically under 0.1 ms) and written by a
background thread.

## Adding instrumentation

Use the macros from `src/telemetry/mod.rs`; they compile to nothing
without the feature, so call sites need no `cfg`:

```rust
use jumpkey::{tel_event, tel_record, tel_span};

let span = tel_span!("enumerate", collected = tracing::field::Empty).entered();
// ...
tel_record!(span, "collected", elements.len() as u64);
tel_event!("elements_ready");
```

- Declare fields you'll record later as `tracing::field::Empty`.
- Work on another thread needs an explicit parent: capture
  `tel_current!()` before handing off, then
  `tel_span!(parent: &parent, "name")` on the worker.
- New string fields must be added to the whitelist (`STRING_FIELDS` in
  `src/telemetry/layer.rs`), and must never hold user content.
- Update `bench/schema-example.jsonl` when adding record-level or root
  attribute fields.
