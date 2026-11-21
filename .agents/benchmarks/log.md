# Benchmark log

Newest first. All times are milliseconds; "p50" is the median press.
"hotkey → visible" = hotkey sent until a jumpkey window is shown
(QueryPerformanceCounter on both ends). "jumpkey CPU" / "target CPU" =
CPU used by jumpkey / by the target app's whole process tree from the
hotkey until the overlay closed.

**Machine for all entries below (2026-11-21):** Intel i5-12600K (16
logical cores), 64 GB RAM, RTX 3070, Windows build 26300 (26H2), bare
metal, AC power, **Balanced** power plan. Monitors: 3440×1440 @ 96 dpi
(primary), 1050×1680 @ 96 dpi (portrait, left), 3840×2160 @ 144 dpi
(right). Release builds.

---

## 2026-11-21: smoke test of every scenario (`122329-smoke`)

App commit `7a9c8ce` (main). `pick-bench --all --pattern
switch-then-press:2000 --scope active_window --iterations 2`, throwaway
browser profiles, 1600×1000 on the primary monitor (96 dpi). **Only 2
presses each, so treat these as rough orders of magnitude, not a
baseline.**

| Scenario | App | Labels | hotkey → visible | jumpkey CPU | target CPU | first press after start |
|---|---|---|---|---|---|---|
| long-feed | Vivaldi | 65 | 1357–1404 | 467 | 1788 | 2274 |
| dense-list | Vivaldi | 135 | 733–794 | 266 | 887 | 2033 |
| edge-dense-list | Edge | 124 | 743–753 | 248 | 565 | 867 |
| vscode-repo | VS Code | 96–100 | 115–172 | 60 | 296 | 112 |
| explorer-folder | Explorer | 178 | 145–156 | 43 | 115 | 176 |
| nested-menu | Vivaldi | 58 | 84–90 | 46 | 102 | 2263 |
| tabs | Vivaldi | 66 | 81–88 | 43 | 188 | 1338 |
| form | Vivaldi | 64 | 79–90 | 42 | 96 | 2326 |
| terminal-split | Windows Terminal | 12 | 29–31 | 19 | 17 | 70 |

(Label counts from jumpkey's log, matched to scenarios by jumpkey
restarts, which happen once per scenario.)

**Target CPU method fixed after this run:** the harness summed the
window process's whole tree. For Explorer that's the shell (69
processes: the real Vivaldi, Claude, shells, tray apps); for VS Code and
Terminal it included their shells. It now counts only descendants
running the same exe. A re-check (`144414-fixcheck`, 3 presses each)
counted 1 process for Explorer and Terminal and 10 for VS Code, with
nearly the same numbers (Explorer 118 ms, Terminal 16–19 ms, VS Code
76–366 ms target CPU), so the values above stand; the other processes
were idle.

### What this shows

- **Page size dominates.** Small pages (form, tabs, menus) take
  80–90 ms; the 500-row list ~750 ms; the long, mostly off-screen feed
  ~1.4 s with only 65 labels. Same conclusion as the dense-list runs:
  jumpkey's browser walk visits the whole page (review #41).
- **Desktop apps are fast** (Explorer 150 ms, Terminal 30 ms) because
  they use one server-side UIA search instead of the node-by-node
  browser walk. VS Code (Electron, but on the desktop path, review #39)
  is 115–170 ms.
- **The first press on a freshly opened Vivaldi page costs ~2–2.3 s**
  regardless of page size (Chromium building its accessibility tree);
  Edge's first press wasn't slow, probably because its tree was already
  built during load.
- Only Terminal is within the README's 50 ms goal.

---

## 2026-11-21: first measurements of the element picker (dense-list)

Scenario `dense-list` (Vivaldi, `bench/fixtures/dense-list.html`, 500
rows each with a link, checkbox and star button), active-window scope.

App commits: `7fc27d6` (first run), then `3ada261` (= `7fc27d6` + the
splash-hotkey fix from PR #24, squash-merged to main as `7a9c8ce`). The
fix only affects presses during the splash screen, not these timings.

| Run | Pattern | Window, DPI | Profile | Labels | n | p50 | p95 | min–max | jumpkey CPU | target CPU |
|---|---|---|---|---|---|---|---|---|---|---|
| `114111-spike` | switch-then-press:300 | not placed (likely maximized on the 4K), 144 | real | 182 | 10 | 778 | 1025 | 757–1025 | 251 | 631 |
| `115940-dense-mon-primary` | switch-then-press:300 | 1600×1000 on ultrawide, 96 | real | 134 | 10 | 901 | 928 | 883–928 | 271 | 893 |
| `120006-dense-mon-2` | switch-then-press:300 | 1600×1000 on 4K, 144 | real | 99 | 10 | 902 | 943 | 892–943 | 283 | 871 |
| `121128-aa-dense` build a | switch-then-press:2000 | 1600×1000 on ultrawide, 96 | throwaway | 134 | 20 | 870 | 934 | 763–934 | 274 | 699 |
| `121128-aa-dense` build b (same exe) | switch-then-press:2000 | 1600×1000 on ultrawide, 96 | throwaway | 134 | 20 | 872 | 915 | 783–918 | 289 | 709 |

Other measurements from the same runs:

- **First press on a freshly opened page: ~2.5 s** (2478 ms, first block
  of the A/A run, new browser profile). Later first-presses after a
  jumpkey restart, same page: 806–944 ms. Chromium builds its
  accessibility tree on first request, so the very first pick on a new
  page or tab is about 3x slower.
- **Press during the splash** (`115437-splash-fix`, with the fix): the
  overlay appeared 2.6 s after the press (rest of the 2.5 s splash + one
  search). Without the fix it never appeared until another message
  arrived (20 s in the test).
- **jumpkey memory: ~77 MB private bytes**, constant across presses. The
  overlay buffer covers the whole virtual desktop: (1050 + 3440 + 3840)
  × 2160 × 4 bytes ≈ 72 MB (review #12).
- **jumpkey idle CPU:** 0.04–0.07 % of one core normally; **3.7 %** in
  the 10 s right after a freshly started browser window settled,
  presumably the cache-warming thread reacting to window move events
  (review #10). Worth checking with in-app telemetry.
- Target process tree: 28 processes with the real Vivaldi profile, 8
  with a throwaway profile.

### What this shows

- **Every pick is a cold search, and it's slow:** ~870–900 ms
  hotkey → visible on a 500-row page, against the README's 50 ms goal.
  The 500 ms cache doesn't help normal use: entries expire 500 ms after
  they're stored, and a search takes longer than that.
- **Time follows page size, not label count.** The same window showed 99
  labels at 144 dpi and 134 at 96 dpi, with identical times (901 vs 902).
  jumpkey walks the whole page, off-screen rows included (review #41).
- **Monitor/DPI doesn't affect speed**, only how many labels fit.
- **Noise floor (A/A, n = 20 per build):** identical builds differed by
  +2.4 ms at the median, bootstrap 95 % interval −23 to +62 ms. With 20
  presses per build, only changes larger than ~60 ms (~7 %) are
  reliably detectable. Use 30+ presses for finer comparisons.
- **Vivaldi's own state matters:** with the real profile, target CPU
  moved from 631 to ~880 ms between runs an hour apart, and p50 from 778
  to 901. Runs with the real profile aren't comparable with each other;
  hence throwaway profiles in the harness from the A/A run on.

### Not comparable with later runs

The first four rows use the old default pattern (300 ms wait, racing the
cache-warming search) and/or the real browser profile. Compare future
runs against the A/A rows: `switch-then-press:2000`, throwaway profile,
1600×1000 on the primary monitor. A full-matrix baseline hasn't been
recorded yet.
