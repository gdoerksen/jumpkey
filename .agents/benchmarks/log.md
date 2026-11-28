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

## 2026-11-22: baseline before step 6 (`222443-baseline-step6-pr0`)

**The reference for roadmap step 6.** It replaces `154236-baseline`,
which predates the step 2 walk speedup. Raw records:
`baseline-db1e280/` (`iterations.jsonl`, `manifest.json`, and the
`--check` results as `check-main.jsonl`).

- **App:** `db1e280` (main) plus the step 6 PR 0 harness changes. The
  only app change is the telemetry-only labels file, off unless
  `JUMPKEY_BENCH_LABELS` is set (it isn't for the benchmark).
- **Build:** release with `telemetry`.
- **Command:** `pick-bench --preset baseline`: every scenario ×
  `switch-then-press:300`, `switch-then-press:2000`, `repeat-press:300`,
  active-window scope, 30 presses each.
- **Run:** 900 presses, 0 timeouts, no power warnings.

New in this run: the responsiveness probe. It sends `WM_NULL` to
jumpkey's IPC window every 5 ms; the "stall" column is the longest gap
in which jumpkey's main thread handled no message during a press.

| Scenario | p50 2000 ms | p95 2000 ms | p50 300 ms | repeat-press: cache hits | repeat-press: misses p50 | main-thread stall p50 | jumpkey CPU | target CPU |
|---|---|---|---|---|---|---|---|---|
| long-feed | 241 | 248 | 243 | 15/30 at 26 | 233 | 206 | 81 | 235 |
| dense-list | 163 | 168 | 162 | 14/30 at 19 | 161 | 138 | 58 | 171 |
| explorer-folder | 162 | 178 | 165 | 15/30 at 25 | 175 | 84 | 50 | 123 |
| edge-dense-list | 158 | 166 | 154 | 15/30 at 14 | 260 | 137 | 53 | 139 |
| edge-cases | 134 | 139 | 135 | 15/30 at 26 | 131 | 113 | 54 | 152 |
| vscode-repo | 72 | 94 | 85 | 14/30 at 21 | 75 | 62 | 38 | 105 |
| nested-menu | 48 | 53 | 46 | 15/30 at 26 | 44 | 39 | 31 | 72 |
| tabs | 47 | 54 | 46 | 15/30 at 27 | 35 | 39 | 30 | 73 |
| form | 42 | 53 | 48 | 15/30 at 26 | 47 | 34 | 29 | 70 |
| terminal-split | 31 | 36 | 29 | 24/30 at 16 | 31 | 15 | 20 | 18 |

hotkey → visible in ms. CPU is the mean ms per press at 2000 ms. jumpkey
private bytes were 79–99 MB. Idle CPU in the 10 s after start was 0.06 %
of one core.

**Where the time goes** (`scripts/telemetry_summary.py`, dense-list at
2000 ms, p50):

| Step | ms |
|---|---|
| Search | 139 |
| Drawing the first frame | 20 |
| Showing and taking focus | 5 |
| Labels, sort, layout | < 1.5 |

The 20 ms frame is mostly clearing the buffer for the whole virtual
desktop (three monitors, #12, step 7).

**A/A noise floor** (`aa-step6-pr0`: the same exe as A and B, 30 presses
each, blocks of 5):

| Scenario | p50 A | p50 B | Difference |
|---|---|---|---|
| dense-list | 165.2 | 162.3 | 1.8 % |
| explorer-folder | 162.6 | 158.7 | 2.4 % |

The script's bootstrap marked the Explorer difference (−3.9 ms) as
significant even though both builds were identical. The bootstrap
treats presses as independent, but presses within one jumpkey start
(block) aren't. So for step 6, a change counts only if it beats both the
A/A floor (about 2.5 % or 4 ms) and the bootstrap interval.

**Live checks** (`pick-bench --check`, 14 cases × 10):

| Result | Cases |
|---|---|
| Pass 10/10 | `pick-click`, `pick-modifiers`, `prefix-backspace`, `esc-cancel`, `wheel-cancel`, `click-cancel`, `foreground-cancel`, `window-pick`, `lone-alt` |
| Fail 10/10 | `focus-kept`, `toggle`, `toggle-during-search`, `rapid-x3`, `menu-win32` |

The failures match the step 6 plan, with one worse than expected: with a
Win32 popup menu open, the overlay shows but can't take focus. Esc goes
to the menu, so the labels stay up until a wheel turn. In that state
`jumpkey --close` doesn't work either, because the overlay's nested loop
swallows the quit message. Recorded in `bench/check/expected-main.json`.

**Hook cost** (`examples/hook_cost.rs`, 1000 iterations × 2 runs; the
cost of installing the hook per pick):

| Measure | p50 | p99 |
|---|---|---|
| Installing both low-level hooks | 1.5 µs | 5 µs |
| Removing both | 1.8 µs | 3.4 µs |
| A full enter + leave through a thread with two message round trips | 22 µs | 39 µs |

**What this shows:**

- **Faster than the old baseline.** Browser pages run at 4–5× the old
  baseline's speed (step 2), and desktop apps are unchanged.
- **The search blocks the main thread.** The stall column tracks the
  search time on browser pages, which is what PR 2 removes.
- **Explorer's stall is about half its press time (84 of 162 ms).** The
  main thread handled messages partway through that search, which fits
  UI Automation's cross-process calls pumping messages on the calling
  thread. Messages dispatched mid-search are another reason to move
  searches to a worker.
- **The hook costs nothing measurable.** Installing it per pick, as
  decided for step 6, adds tens of microseconds.

---

## 2026-11-22: hook spike (roadmap step 4)

`examples/hook_spike.rs` on branch `spike/hook`, release build, same
machine. Findings and the menu matrix: architecture review #34.

**Hook latency** (`SendInput` → `WH_KEYBOARD_LL` callback, 100 tagged
events, 3 runs): p50 99–108 µs, p95 235–241 µs, max 381–522 µs. A
fourth run, started right after a build, showed p50 66 µs but p95 4.3 ms
and one event at 328 ms. That is delivery delay on a busy machine, not
time in the callback, and no key leaked.

**60 s idle wait** (`menus-wait 60`): all 9 menus still open, the overlay
still above them, and the hook still swallowing keys afterwards.

**Hook stalls** (`LowLevelHooksTimeout` not set):

| Stall in callback | Stalled key leaked | Leak after | Later keys reach hook and are swallowed |
|---|---|---|---|
| 300 ms | no | — | 5/5 |
| 1,000 ms | yes | 319 ms | 5/5 |
| 5,000 ms | yes | 318 ms | 5/5 |
| 3 / 10 / 20 × 500 ms in a row | — | — | yes (hook not removed) |

**Scroll areas** (Vivaldi, throwaway profile, 1600×1000, 5 runs; median
ms, scrollable areas found):

| Page | `FindAll(IsScrollPatternAvailable)` | one-call tree + filter | point + walk up | areas found |
|---|---|---|---|---|
| scroll-areas | 11 | 14 (238 nodes) | 2 | 2 of 6 (page, sidebar) |
| scroll-areas, marked up¹ | 11 | 14 | 2 | 5 of 6 |
| dense-list | 38 | 69 (2,111 nodes) | 3 | 1 (page) |
| long-feed | 58 | 116 (3,706 nodes) | 3 | 1 (page) |

¹ The same page with `tabindex` on the code block, `role="region"` on
the card strip and `role="group"` on the outer nested scroller. The
unmarked inner scroller stays invisible.

---

## 2026-11-22: faster browser walk (roadmap step 2 PR 3) and the spike

**Spike** (`spike-<mode>-<scenario>`, branch `spike/browser-walk` at
`ec753d8`): one build, mode from the exe name, current vs mode, 10 presses
at `switch-then-press:2000`, p50:

| Scenario | current | prune | subtree cache |
|---|---|---|---|
| dense-list | ~790 | 249 | 166 |
| long-feed | ~1,360 | 163 | 251 |
| edge-cases | ~640 | 259 | 138 |
| form | ~90 | 94 | 53 |

Prune visited 3–8× fewer nodes but lost the 3 labels of edge-cases'
fixed popover (its DOM parent is far below the viewport). Subtree cache
lost nothing. Subtree cache was chosen.

**PR 3** (`pr3-<scenario>`): branch `perf/browser-walk` vs main
`1bfa07c`, telemetry release builds, 10 presses per build:

| Scenario | p50 2000 main → PR 3 | p95 2000 | repeat-press miss (p95) | target CPU |
|---|---|---|---|---|
| dense-list | 721 → 165 | 730 → 175 | 742 → 170 | 573 → 167 |
| long-feed | 1,296 → 243 | 1,360 → 249 | 1,350 → 243 | 1,003 → 226 |
| edge-cases | 608 → 133 | 631 → 144 | 616 → 134 | 484 → 131 |
| form | 90 → 53 | 104 → 58 | 95 → 50 | 91 → 62 |
| nested-menu | 86 → 48 | 100 → 55 | 93 → 49 | 85 → 56 |
| tabs | 87 → 46 | 100 → 56 | 89 → 48 | 94 → 57 |

A/A (main vs main, dense-list): 744 vs 731, so ~2 % noise. Desktop
unchanged (explorer-folder 156 vs 154, vscode-repo 80 vs 75, 5 presses).
jumpkey private bytes +5–17 MB after big walks. `walk_diff.py`: no
missing elements on any scenario; PR 3 adds the same 4 disabled
Vivaldi buttons in some presses (see review #41). A probe build logged
those buttons as `IsEnabled=true` in 6/8 one-call fetches.

Also seen: main's first press on dense-list (the batch's first, cold
Vivaldi) still showed nothing after the #53 fix's 3 attempts (~370 ms,
6 nodes visited, no `Document`).

---

## 2026-11-21: empty first press fix (#53, roadmap step 2 PR 2)

Branch `fix/browser-empty-first-press` vs main `6168e3c`, telemetry
release builds.

- **A/B** (`pr53-ab`): the 6 Vivaldi scenarios × `switch-then-press:2000`
  and `repeat-press:300`, 10 presses per build. p50 changes from −16.5 to
  +24.8 ms (−2.7 % to +3.3 %; tabs `repeat-press` +6 ms on a 28 ms cache
  hit), i.e. noise. The branch's 108 hotkey walks all took 1 attempt.
- **Fresh page** (`fp-<build>-<scenario>-<n>`): each build alone, each
  scenario 3 times, so every run's first press hits a just-opened
  window. Main: no overlay on the first press in 2/18 (long-feed,
  nested-menu), plus 1/6 in the A/B run (dense-list). Branch: 0/18; it
  retried 3 first presses (tabs, dense-list, form), each succeeding on
  the second attempt with the full element count, at 0.7–1.3 s for the
  walk.

---

## 2026-11-21: baseline before the refactor (`154236-baseline`)

**The reference for comparing refactors.** Raw records:
`baseline-a4d2f52/`. App commit `a4d2f52` (main). `pick-bench --preset
baseline` (harness from `fix/bench-startup-wait`: waits for the splash,
retries the first press): every scenario × `switch-then-press:300`,
`switch-then-press:2000`, `repeat-press:300`, active-window scope, 30
presses each (810 presses, 0 timeouts). Throwaway browser profiles,
1600×1000 on the primary monitor (96 dpi), Balanced power plan as for
every entry here.

| Scenario | p50 2000 ms | p95 2000 ms | p50 300 ms | repeat-press: cache hits | repeat-press: misses p50 | jumpkey CPU | target CPU |
|---|---|---|---|---|---|---|---|
| long-feed | 1272 | 1324 | 1266 | 14/30 at 26 | 1263 | 421 | 994 |
| dense-list | 748 | 778 | 742 | 15/30 at 26 | 758 | 253 | 597 |
| edge-dense-list | 731 | 966 | 744 | 15/30 at 25 | 990 | 261 | 587 |
| explorer-folder | 155 | 161 | 160 | 15/30 at 25 | 155 | 48 | 111 |
| form | 89 | 96 | 90 | 13/30 at 25 | 83 | 44 | 88 |
| tabs | 86 | 91 | 84 | 15/30 at 25 | 85 | 43 | 94 |
| nested-menu | 85 | 93 | 77 | 14/30 at 26 | 85 | 42 | 90 |
| vscode-repo | 70 | 83 | 74 | 15/30 at 26 | 74 | 37 | 82 |
| terminal-split | 37 | 50 | 33 | 18/30 at 25 | 37 | 22 | 17 |

hotkey → visible in ms; CPU = mean ms per press at 2000 ms. "Cache
hits" = repeat presses under 30 ms (for Terminal a full search is barely
slower, so its split is approximate). jumpkey private bytes 75–85 MB.
Idle CPU in the 10 s after start: 2.5 % of one core.

First press after start (ms, the three jumpkey starts per scenario):

| Scenario | 1st start | 2nd | 3rd |
|---|---|---|---|
| long-feed | 1774 | 1422 | 1489 |
| form | 1224 | 121 | 114 |
| tabs | 1133 | 116 | 115 |
| dense-list | 923 | 764 | 757 |
| edge-dense-list | 852 | 849 | 852 |
| nested-menu | 118 (2nd try) | 109 | 113 |
| explorer-folder | 175 | 181 | 178 |
| vscode-repo | 119 | 99 | 106 |
| terminal-split | 68 | 70 | 63 |

On small Vivaldi pages the first press on a freshly opened page costs
~1.1–1.2 s (Chromium building its accessibility tree), later starts
~0.1 s. On the big pages the first press is within ~1.5× of a normal
press. nested-menu's first try showed nothing (see below); its 118 ms is
the second try, after the page had warmed up, so it isn't a cold value.
Startup (spawn → splash closed) 2.93 s, once 4.7 s.

In-app telemetry from the same day (PR #26, dense-list, 10 presses): 98 %
of a press is the browser walk's tree search (~930 ms, 2,111 nodes
visited for 134 elements); overlay drawing ~19 ms, show + focus ~10 ms,
labels and sort < 0.2 ms.

### What this shows

- **Waiting 300 ms or 2 s after switching makes no difference** (742 vs
  748 on dense-list): the cache-warming walk never finishes in time to
  help, so every press after a switch is a full search.
- **The cache only helps an immediate retry, and only every other
  time:** repeat presses alternate between a hit (~25 ms) and a full
  search. An entry expires 500 ms after it was stored and a hit doesn't
  refresh it, so the press after a hit misses.
- **Browser pages are 10–35× slower than desktop apps** and scale with
  page size (form 89 → dense-list 748 → long-feed 1272), not label
  count. This is the walk visiting the whole page, off-screen included
  (review #41); it's where a refactor can win the most.
- Desktop apps (UIA server-side search) are 37–155 ms; only Terminal
  meets the README's 50 ms goal at p50.
- edge-dense-list p95 (966) and its repeat-press misses (990) are well
  above its p50: Edge occasionally takes ~250 ms longer on the same page.
- **jumpkey sometimes stalls 3–5 s before its splash appears** while a
  freshly launched browser is busy (4 of the last 5 runs had at least
  one). Not investigated; something in startup before the splash is
  shown waits on other processes.
- **Open issue, user-visible: a press right after opening a page can
  show nothing.** nested-menu's first press found no elements on the
  freshly opened Vivaldi page (jumpkey logs "no interactable elements"),
  so no overlay appeared; the same happened in two runs before the
  harness retried. Likely the browser was still busy and the 50 ms
  accessibility activation timed out. pick-bench now retries and records
  `first_press_attempts`, so this stays visible in the data but isn't in
  the main numbers. Both this and the startup stall are worth fixing in
  jumpkey.

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

**Correction (found later the same day):** in the first two scenarios
(dense-list, long-feed) jumpkey took ~4 s before its splash appeared.
pick-bench only waited 3 s for a window, so it pressed the hotkey before
jumpkey had registered it and timed the splash instead. Their "first
press after start" values (2033, 2274) are not real; the other rows are
unaffected (no stall in jumpkey's log), and the per-press columns are
fine. pick-bench now waits up to 15 s for the splash and records
`startup_ms`.

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
1600×1000 on the primary monitor, or better, against the baseline
(`154236-baseline`, above).
