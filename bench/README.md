# pick-bench: element-picker benchmark

`tools/pick-bench` measures how fast jumpkey's element picker responds,
and what it costs in CPU and memory, by driving a real `jumpkey.exe`
from outside: it presses the hotkey with `SendInput`, times how long a
jumpkey window takes to appear, presses Esc, and reads process counters.

It links nothing from jumpkey, so the **same benchmark runs before and
after internal refactors**. Record a baseline before a big change, then
compare.

## Quick start

```powershell
# 1. Quit your everyday jumpkey (tray → Quit, or `jumpkey --close`).
#    Only one instance can run per session; pick-bench refuses otherwise.

# 2. Build the jumpkey under test and the harness.
cargo build --release
cargo build --release -p pick-bench

# 3. Run one scenario (opens Vivaldi on a fixture page), 30 presses,
#    each 2 s after switching to the window.
target\release\pick-bench.exe --scenario dense-list

# Baseline to compare refactors against (~30 min): every scenario x
# switch-then-press:300/2000 + repeat-press:300 x active_window.
target\release\pick-bench.exe --preset baseline

# Whole matrix: every scenario x 5 timing patterns x 2 scopes (3-4 h).
target\release\pick-bench.exe --all --label full

# Compare two builds, alternating in blocks of 5 (A/B).
target\release\pick-bench.exe --scenario dense-list --exe-a base.exe --exe-b new.exe
```

Don't touch the keyboard or mouse while it runs (it's sending real
input). Ctrl+C in the console stops it cleanly. Results go to
`bench/results/<run-id>/` (git-ignored):

- `manifest.json`: builds, options, machine description, warnings
- `iterations.jsonl`: one JSON object per line (`kind` = `process_start`,
  `idle`, `iteration`, `process_stop`)

A summary table is printed at the end. Deeper analysis (confidence
intervals for A/B, budgets) comes with `scripts/telemetry_summary.py`.

## What's measured

| Field | Meaning |
|---|---|
| `hotkey_to_visible_us` | Hotkey sent → a jumpkey window became visible. Both timestamps are `QueryPerformanceCounter`, which is system-wide, so the number is accurate across processes. |
| `visible_to_foreground_us` | Visible → jumpkey owns the foreground (today's overlay takes focus; a future no-activate overlay won't, so this becomes empty). |
| `jumpkey_cpu_ms` | jumpkey CPU from hotkey to overlay hidden (`QueryProcessCycleTime`, converted with the measured TSC rate). |
| `target_cpu_ms` | CPU used over the same interval by the target window's process **plus its descendants running the same exe**. Chromium and Electron build the accessibility tree in renderer processes (same exe), so the window's own process isn't enough; other images are left out so the shell `explorer.exe` doesn't pull in every app started from Start, nor VS Code / Terminal their shells. This is the cost jumpkey puts on the app you're using. |
| `between_*` | Since the previous overlay closed: window switching, dwell time, and what jumpkey did in the background meanwhile (cache warming). |
| `after.*` | jumpkey private bytes, working set, handles, GDI/USER objects after each press. |
| `idle` record | jumpkey CPU over `--idle-secs` with nobody pressing anything. |
| `process_start.startup_ms` | Spawn → ready for the first press (splash closed). Slow starts (> 5 s) are also printed. |
| `process_start.first_press_visible_us` | The first press after jumpkey starts: one-time costs (creating the overlay). Kept out of the main stats. |
| `process_start.first_press_attempts` | Presses needed before the first overlay appeared (up to 3, 3 s apart; a just-opened page can make jumpkey find nothing). Above 1, the first-press value is from a later try and isn't cold; the printed summary marks it. |

Esc dismisses every overlay, so the harness never measures `perform`
(acting on an element). The in-app telemetry covers that.

## Timing patterns

Patterns describe what the user does, not how jumpkey works, so they
mean the same thing after a refactor:

- `switch-then-press:<ms>`: focus pick-bench's neutral window, focus the
  target, wait `<ms>`, press. **Default `2000`**: the everyday case
  (switch, read a bit, press). `10000` = read the page first; today
  that's a cold search, but it becomes a cache hit if cache entries live
  longer (the planned freshness redesign), so keep it in comparisons.
  `0` and `300` are stress cases: the press races jumpkey's background
  cache-warming search of the same window.
- `repeat-press:<ms>`: press again `<ms>` after the previous overlay
  closed, without switching ("Esc, retry"). Today this is the only case
  the 500 ms cache helps.

`--all` runs 0, 300, 2000 and 10000 ms plus `repeat-press:300`;
`--preset baseline` runs 300 and 2000 ms plus `repeat-press:300`, in the
active-window scope only (the multi-window scopes depend on whatever
else is open on the desktop, so they don't make a repeatable baseline). These
waits are educated guesses. Once the in-app telemetry records the time
from switching windows to pressing the hotkey in daily use, set them
from that.

## Scenarios and fixtures

`scenarios.toml` lists the target apps and how to open each in a fixed
state. Web scenarios use the local pages in `fixtures/` so the page is
identical on every run and machine:

| Scenario | App | Content |
|---|---|---|
| `dense-list` | Vivaldi | ~500 list rows, each with checkbox + star button |
| `long-feed` | Vivaldi | 300 posts, almost all off-screen |
| `form` | Vivaldi | inputs, selects, checkboxes, radios |
| `nested-menu` | Vivaldi | ARIA menubar with an open nested menu |
| `tabs` | Vivaldi | 30 tabs, each with a close button |
| `edge-cases` | Vivaldi | content a smarter walker could lose: an inner scroller with a sticky header, a fixed bottom bar, a fixed popover whose parent is scrolled far away, a child drawn outside its parent, a `display: contents` wrapper |
| `edge-dense-list` | Edge | `dense-list.html`, for machines without Vivaldi |
| `explorer-folder` | File Explorer | `fixtures/folder/` (200 files, generated on first run) |
| `vscode-repo` | VS Code | this repo with two files open |
| `terminal-split` | Windows Terminal | two split panes |

`fixtures/scroll-areas.html` isn't a scenario. It's the scroll mode
(#54) test page: six scroll containers (the page, a sidebar, a
horizontal code block, a card strip, and a nested pair) and two decoys
that must not count.

Apps that aren't installed are skipped. `--manual` benchmarks whatever
window you focus instead.

Browser scenarios start the browser with a throwaway profile
(`--user-data-dir={profile}`, deleted after the run): its own process
tree, no tabs, extensions or history from your real profile. That keeps
runs repeatable and makes `target_cpu_ms` count only the benchmark
window. Use `--manual` in your real browser for real-profile numbers.

`--monitor <n|primary>` picks the monitor; scenario rects are relative
to its top-left corner. The window's rect and DPI are recorded per
press. (Measured: the monitor's DPI changes how many labels fit, not the
time.)

Multi-window scopes (`all_windows`, `active_monitor`) search every
visible window, so their numbers depend on what else is open. Close other
windows for comparable runs; `visible_app_windows` is recorded per
press.

## Checking the elements found, not just the time

A walker change has to find the **same elements**, not only find them
faster. A telemetry build (`--features telemetry`) run with
`JUMPKEY_DUMP_WALK` set appends one JSON line per UI Automation walk:
the exe basename, `visited` / `collected` counts, the window rect and
each element's role and rect. No names, titles or URLs.

- `JUMPKEY_DUMP_WALK=1` writes `%LOCALAPPDATA%\jumpkey\telemetry\walks.jsonl`.
  pick-bench sandboxes `LOCALAPPDATA` per build, so set it in the shell
  before an A/B and each build gets its own file under
  `bench/results/<run-id>/sandbox/<build>-<scope>/localappdata/`.
  Run one scenario per invocation so the file holds one page.
- Any other value is the folder to write `walks.jsonl` into (for a
  manual run).

Then compare:

```text
python scripts\walk_diff.py <A>\walks.jsonl <B>\walks.jsonl
```

It prints what B is missing and what it added compared with A (rects
relative to the window, so a moved window still matches), and exits 1
if anything is missing. The dump is a diagnostic, not part of the bench
contract below.

## Getting stable numbers

- Plug in and use a High performance / Best performance power plan.
  pick-bench warns about battery and power-saver plans (also recorded in
  the manifest).
- Measure release builds only.
- Close heavy background work (builds, updates, video).
- Run an A/A check once (`--exe-a X --exe-b X`, same file) to see how
  much two identical builds differ on your machine: that's the noise
  floor any real change has to beat.
- Keep baselines worth comparing against under
  `.agents/benchmarks/baseline-<git-sha>/`.

## The bench contract

pick-bench depends on these jumpkey behaviours, and **only** these.
Changing one means updating pick-bench in the same PR:

1. `jumpkey.exe --no-tray` starts it without a tray icon;
   `jumpkey.exe --close` asks a running instance to quit.
2. The config is read from `%APPDATA%\jumpkey\config.toml`
   (pick-bench points `APPDATA` at a sandbox and copies in
   `bench/config/<scope>.toml`). Logs and telemetry go under
   `%LOCALAPPDATA%` (also sandboxed).
3. `hotkeys.pick_element` in the template is the chord pick-bench presses
   (override with `--hotkey`). `scope.mode` selects the scope.
4. **Overlay visible** = any top-level window of the jumpkey process
   raising `EVENT_OBJECT_SHOW`; **closed** = that window raising
   `EVENT_OBJECT_HIDE`. No window class or count is assumed, so
   per-monitor overlay windows work too.
5. Esc closes the overlay.
6. Optional, telemetry builds only: `JUMPKEY_BENCH_RUN_LABEL` tags
   records with the run id; `JUMPKEY_BENCH_DWMFLUSH=1` makes jumpkey wait
   for the compositor after showing the overlay, to time "pixels on
   screen". Other builds ignore both.

Also assumed: jumpkey shows a window (the splash) while it starts and
takes hotkeys once that's gone. pick-bench waits for a window to appear
and then for none to be visible for 400 ms (up to 20 s). If no window
appears within 15 s it assumes a build without a splash and goes ahead.
(Jumpkey sometimes takes 3–5 s before its splash appears; an earlier 3 s
limit pressed the hotkey before it was registered.) A second jumpkey
exits right away when one is already running.
