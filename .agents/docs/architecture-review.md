# Architecture review

_Date: 2026-11-20 · Commit reviewed: `410773f` (v0.6.0, unreleased)_
_Refreshed 2026-11-21 against `596b321` (after pick-bench, telemetry and
the baseline): every item has a status line, line references are
updated, measurements are folded in, and items #52–#54 (including scroll
mode) are new. The current plan is the roadmap below; the older priority
lists further down are kept for history._
_Roadmap revised 2026-11-22 after steps 1–2: owner decisions on the
library, publishing and portability (round 7), a portability guard step,
and the freshness work moved last._

Overall: the design is good. The pure logic is cleanly separated from the
Windows code, and the speed work is careful and well documented. The weak
spots are a platform abstraction that doesn't really do its job yet, and too
much pick logic living in `main.rs`. One likely correctness bug is in the
element-ID / cache interaction (see weakness 6).

## Current roadmap (2026-11-22)

_Replaces "What to do first", both "Revised priorities" lists and the
"Correctness + speed roadmap" table below._

Measured (`.agents/benchmarks/log.md`): before step 2, browser pages took
0.7–1.3 s per press, almost all of it the tree walk (#41). After step 2
a cold browser search is 45–245 ms on the fixtures; desktop apps take
37–155 ms. The cache still never helps a press after a window switch
(#20), but a miss is now cheap.

| Step | Item | Why now | Depends on | Size |
|---|---|---|---|---|
| 1 | **Done (2026-11-21).** Small correctness fixes: reload safety (#8), warming-hook "invalidate on move" (#10), the **quick fix** for the ID/cache bug (issue #22, weakness 6) | Silent config resets, wasted searches, wrong clicks | — | S each |
| 2 | **Done (2026-11-22).** Faster browser walk (#41) and the empty first press (#53). A spike compared pruning off-viewport subtrees with a one-call subtree cache; the cache won (no labels lost) and shipped: dense-list 721 → 165 ms, long-feed 1,296 → 243 ms. The prune variant stays on branch `spike/browser-walk` | The slowest part of the existing features | — | M |
| 3 | **Portability guard** (weakness 2, round 7): `cargo check` and `cargo test --lib` for `x86_64-unknown-linux-gnu` in CI and in the local checks, and fix whatever doesn't build (e.g. `tracing-appender`, Windows-only deps not behind `cfg`) | Makes "portable core" a check rather than an intention, before steps 5–6 add a lot of core code | — | S |
| 4 | **Hook spike** (#34, widened for #54): does a no-activate overlay + `WH_KEYBOARD_LL` keep menus open and swallow keys reliably? Does Chromium expose ScrollPattern on scroll containers, and how fast is the search for them (the one-call tree fetch from step 2 should make this cheap)? Record what the hook delivers as **platform-neutral key events** (physical key, modifiers, down/up), the seam a macOS event tap would fill | One throwaway prototype answers the riskiest question for menus *and* scroll mode | — | S |
| 5 | **Scroll mode v1** (#54) on the hook from step 4, as a self-contained mode the state machine absorbs later | User-visible feature; proves the hook, the off-thread worker pattern and a no-activate indicator window in daily use | 4 | M |
| 6 | **State machine + no-activate pick overlay** (Appendix A, #26), with the pick flow moved out of `main.rs` (#3) into the **platform-free core**: it consumes commands and key events and talks to `ElementSource` / overlay / input traits in neutral types, tested with fakes. The ID/cache thorough fix (`Snapshot`, weakness 6) lands here | Removes queued hotkeys (#15, #47) and the re-search after every pick (#48), gives menu mode a home, and is where the portability seams get designed from real callers (weakness 1) | 3, 5 | L |
| 7 | Per-monitor overlays (#12, #32, #37), then menu mode (#33) | Mixed-DPI correctness, per-keystroke cost, the menu goal | 6 | L |
| 8 | Shared UIA cache + event-driven freshness + serve-stale (#20–22), **re-scoped after the real-site check**: may shrink to "warm on foreground change" | After step 2 a miss costs 45–245 ms on the fixtures, so this mainly matters for very large real pages | 6 | M–L? |
| — | **Step 2 follow-ups:** disabled Vivaldi buttons labelled after the one-call fetch (#41 status); the #53 retry budget is too short on a cold Vivaldi start; the **real-site check** of both walk variants (owner opens 2–3 daily sites) | Small, measured fixes; the real-site check decides prune vs cache for good and re-scopes step 8 | — | S each |
| — | Typed config (#16) | The config file format is user-facing: cheapest to change before the first release | — | M |
| — | Startup stall before the splash (#52) | Investigate when convenient; telemetry can time startup | — | S? |

**Why the walk comes right after the correctness fixes** (owner, round
6): it is the slowest part of the existing features, and architecturally
the most self-contained change on the list. It stays inside the browser
walker (`walk_recurse` / `walk_window`) behind existing interfaces, needs
none of the hook, state machine or cache work, and doesn't constrain
their design. It may also shrink step 6, whose main reason to exist is
slow cold searches, and it mostly removes the cap-before-sort problem
(#41). Keep it a walker change: no backend restructuring before the
`Snapshot` / `ElementSource` refactor (#3, weakness 6). For the same
reason the ID/cache bug gets its quick fix in step 1, not the thorough
`Snapshot` fix, which would rework the same backend code.

### Owner decisions, round 7 (2026-11-22)

| Topic | Decision |
|---|---|
| crates.io | **Not published.** jumpkey ships as MSI / GitHub releases / Store; `publish = false`. (Only upstream `keyhop` 0.5.0 is on crates.io, by its author.) |
| Library API (#18) | **Internal.** The lib + bin split stays as code structure (benches, examples and tests reach the code; the pick flow can move out of `main.rs`), with **no stability promise and no outside consumers**. Visibility is whatever is convenient; nothing needs a version bump. |
| Platforms | **Windows first, written to be portable** to Linux or macOS later. No second backend is built now. |
| Freshness (old step 6) | Moved last (step 8), re-scoped after the real-site check. |

**Portability: what it buys and what it costs.** The owner's view is
that designing for a port forces better decisions. Mostly agreed, with
one condition: the seams must be designed from the code that calls them,
and proven by a **test fake** as the second implementation, not guessed
from one Windows implementation.

- **Buys:** a platform-free core (pick flow, state machine, hint engine,
  layout, keyboard model, config, `Snapshot`) that is testable without a
  desktop; Win32 types (`HWND`, UIA elements) can't leak into app logic;
  the Linux check catches it when they try.
- **Costs:** a trait shaped by one implementation is usually the wrong
  shape (weakness 1 is exactly that: `Backend` was written up front and
  nothing uses it). More indirection at the seams. Keeping the Linux
  build green (Windows-only deps behind `cfg`). And the pull toward a
  lowest common denominator.
- **Rules that keep the cost down:**
  1. Seams where the app actually varies: element source, overlay, key
     input, hotkeys, actions. Designed in step 6 from real callers, each
     with a fake for tests.
  2. Traits speak neutral types (`Bounds`, physical keys, `Snapshot` /
     `ElementId`), never `HWND` or UIA.
  3. Platform-specific power is allowed as an optional capability (e.g.
     UIA menu discovery for menu mode) rather than banned because another
     platform lacks it.
  4. No second backend until one is actually being built; the Linux
     `cargo check` (step 3) is the enforcement in the meantime.
- **Feasibility, for later:** macOS is the realistic second target (AX
  API plus a `CGEventTap`, both behind the Accessibility permission; the
  apps jumpkey is modelled on are macOS apps). Linux on X11 is fine;
  **Wayland** restricts exactly what jumpkey needs (global hotkeys,
  keyboard hooks, overlay placement, synthetic input), which needs
  portals or compositor-specific protocols. The step 4 hook spike should
  keep that in mind: the state machine must not depend on being able to
  swallow keys globally.

## What's done well

1. **The pure logic is separated from the OS code, and it's well tested.**
   Label generation (`hint.rs`), the keyboard model (`keyboard.rs`), spatial
   ordering (`layout_order.rs`), visibility geometry (`model.rs`), config and
   cache are plain data and functions with no Windows calls. That's where
   the tests are: 37 in `hint.rs`, 24 in `alphabet_presets.rs`, 18 in
   `keyboard.rs`, 12 in `cache.rs`.

2. **The overlay doesn't know what it's labelling.**
   `pick_hint(Vec<Hint>, HintStyle) -> PickResult { index, modifier }` takes
   rectangles and labels and returns an index. That's why the element picker
   and the window picker can share one renderer.

3. **Speed work aimed at the actual bottleneck.** The cost is cross-process
   COM calls, not CPU, and the fixes target that: a UIA prefetch request so
   each node's properties come back in one call, one server-side search for
   desktop apps instead of a node-by-node walk, parallel searches grouped by
   process, and a background cache-warming thread with a queue of one so
   only the newest window gets searched. Each fix sits behind the existing
   interfaces.

4. **Fails soft.** If the tray, config watcher, cache-warming thread, IPC
   window or splash screen fails, it logs a warning and the hotkeys keep
   working. Hotkey conflicts are reported without stopping startup.

5. **Comments explain why.** Constants and design choices come with a
   reason.

## Weaknesses

### 1. The `Backend` trait isn't doing its job

> **Status (2026-11-22):** open; **keep and redesign** (round 7: write for portability). Replaced in roadmap step 6 by seams designed from the pick flow's real callers (`ElementSource` and friends), each with a test fake.

The trait has two methods (`enumerate_foreground`, `perform`) and nothing
uses it generically. `main.rs` calls `enumerate_by_scope`, `hint_anchor` and
`PickContext` directly on the concrete `WindowsBackend`. Scope handling and
the window picker aren't in the trait at all. A Linux backend would mean
rewriting the pick flow, not just implementing the trait. Either widen the
trait to cover what the app actually needs, or drop it.

### 2. CI never checks that the core builds on other systems

> **Status (2026-11-22):** open; now **roadmap step 3** (portability guard). CI still runs only on `windows-latest`. Known blocker from step 1: `cargo check --lib` for a non-Windows target fails in `tracing-appender` (seen on wasm; the Linux target wasn't installed).

The README and `Cargo.toml` stress that non-Windows targets compile without
the `windows` module, but `.github/workflows/ci.yml` only runs on
`windows-latest`. A `cargo check` / `cargo test --lib` job on Linux would
keep that promise honest.

### 3. `main.rs` holds the app logic

> **Status (2026-11-21):** open. `handle_pick_element` is now `src/main.rs:738` and also carries the telemetry spans.

`handle_pick_element` (`src/main.rs:738`) runs the whole pick in the binary,
where it can't be tested. The main loop also has the "open settings → apply
config → handle error" block written out twice (once for the hotkey, once for
the tray). See the detailed section below.

### 4. Some files are too big

> **Status (2026-11-21):** open. `overlay.rs` 3,805 lines, `windows/mod.rs` 2,306, `settings_window.rs` 2,110 (57 `unsafe`).

- `overlay.rs` (~3.8k lines) mixes layout and collision handling, callout
  shape geometry, pixel drawing, and the Win32 window, message loop and
  keyboard input. Suggested split: `overlay/{layout,sprite,input,window}.rs`.
- `windows/mod.rs` (~2.2k lines) holds the backend, element filtering, the
  action cascade, browser detection and window planning.
- `settings_window.rs` (~2.1k lines) is hand-written Win32 with 57 uses of
  `unsafe`.

### 5. Picking is synchronous and blocks the main loop

> **Status (2026-11-21):** open. Scroll mode (#54) is designed not to add another nested loop.

`pick_hint` runs its own message loop inside the main one. While the overlay
is open, config reloads and tray commands wait. On a cold cache the UIA
search also runs on the main thread. Fine for now; it only becomes a problem
for features that need to react during a pick.

### 6. Element IDs are tied to backend state, and the cache check is unreliable

> **Status (2026-11-21):** **quick fix done** (issue #22 on `gdoerksen/jumpkey`). Reproduced live first: pick in B, pick in A (the hotkey backend resets its table), back to B → `served from cache` with B's IDs now naming A's elements. The fix tags each cache entry with a process-unique ID generation (`cache::Generation`), renewed on every reset, and only accepts hits from the current one. Found along the way: the warming thread's own check almost always accepted entries (its table never reset, so every ID number "existed"), so its walks after a foreground change or move were mostly no-ops, and its table pinned every UIA element it ever saw. It now skips windows with any fresh entry (`warm_window`) and starts a fresh table per walk. The thorough fix (`Snapshot`, option 3) is still open.

`ElementId` is only meaningful inside the `WindowsBackend` instance that
created it. That instance's map from ID to UIA element changes on every
search, but nothing in the types ties an `Element` to the search that
produced it. The cache tries to make up for this at runtime, and the check it
uses can pass when it shouldn't (likely bug). See the detailed section below.

### 7. Smaller points

> **Status (2026-11-21):** open. `Action::Scroll` is still never triggered; scroll mode (#54) scrolls with wheel input and ScrollPattern jumps and doesn't go through it.

- `Action::Type` and `Action::Scroll` are never triggered by the binary.
- Code is gated with `cfg(windows)` throughout `main.rs`. A Windows-only
  `app` module would be tidier.
- Config is applied by hand in seven ordered steps (`apply_config`). Each new
  setting has to be wired in there manually.

## What to do first

_Superseded by the current roadmap at the top of this document._

1. Fix or confirm the element-ID / cache issue (weakness 6).
2. Add a Linux `cargo check` / `cargo test --lib` CI job.
3. Move the pick flow out of `main.rs` into a testable library type, and
   merge hotkey and tray input into one command path.
4. Split `overlay.rs` along its existing layout / drawing / window lines.
5. Decide what the `Backend` trait is for.

---

## Detail: weakness 3, `main.rs` holds the app logic

### What's there now

`handle_pick_element` is the product's core use case, and it lives in the
binary:

```rust
let ctx = PickContext::capture();
let mut elements = backend.enumerate_by_scope(runtime.scope_mode, &ctx)?;
if let Some(anchor) = hint_anchor(runtime.hint_order, runtime.scope_mode, &ctx, &elements) {
    sort_by_proximity(&mut elements, anchor);
}
let labels = runtime.hint_engine.generate(elements.len());
let hints = /* zip elements + labels */;
match pick_hint(hints, runtime.element_style)? {
    Some(result) => {
        let action = match result.modifier { Ctrl => DoubleClick, Shift => RightClick, None => Invoke };
        backend.perform(elements[result.index].id, action)
    }
    None => ...
}
```

Each step is tested on its own, but the code that joins them isn't tested at
all. These rules have no tests:

- Sort the elements *before* generating labels, so `elements[result.index]`
  still points at the element that was labelled.
- Modifier precedence: Ctrl → double-click, Shift → right-click.
- Empty enumeration → nothing happens (no overlay).
- Cancel → no action.

They can't be tested because the function depends on concrete Win32 types
(`WindowsBackend`, `pick_hint`, `PickContext::capture()`).

The main loop has a related problem. Hotkey actions and tray commands arrive
on two separate paths, and both contain the same open-settings / apply /
report-error block.

### Suggested shape

Move the flow into the library, behind two small traits:

```rust
// src/app.rs (cross-platform)
pub trait ElementSource {
    type Ctx;
    fn capture(&self) -> Self::Ctx;
    fn enumerate(&mut self, scope: ScopeMode, ctx: &Self::Ctx) -> Result<Snapshot>;
    fn anchor(&self, order: HintOrder, scope: ScopeMode, ctx: &Self::Ctx, els: &[Element]) -> Option<(i32, i32)>;
    fn perform(&mut self, snap: &Snapshot, idx: usize, action: Action) -> Result<()>;
}

pub trait HintPicker {
    fn pick(&mut self, hints: Vec<Hint>, style: Style) -> Result<Option<PickResult>>;
}

pub struct ElementPicker { engine: HintEngine, order: HintOrder, scope: ScopeMode, style: Style }

impl ElementPicker {
    pub fn run(&self, src: &mut impl ElementSource, ui: &mut impl HintPicker) -> Result<Outcome> { ... }
}
```

and turn every input into one command type:

```rust
enum Command { PickElement, PickWindow, OpenSettings, ViewLog, Quit, ReloadConfig }
```

so the main loop becomes "collect commands from hotkeys, tray and the
watcher, then `app.handle(cmd)`".

Results:

- Tests with a fake `ElementSource` and a scripted `HintPicker` can check
  "Shift on the last key → right-click on element N" and "sorting changes
  which element gets `f`" without a desktop.
- `main.rs` shrinks to wiring: create the Win32 pieces, run the loop.
- The `ElementSource` trait is in effect what `Backend` should have been
  (weakness 1), designed from the code that actually calls it.

---

## Detail: weakness 6, element IDs and the cache

### How IDs work today

- `WindowsBackend` keeps `elements: HashMap<ElementId, UIElement>` and a
  counter, `next_id`.
- Each search turns UIA elements into public `Element { id, bounds, ... }`
  values. `perform(id)` looks the ID up in **this backend's** map.
- `enumerate_foreground` (active-window scope) **clears the map and resets
  `next_id` to 0** on every real search (`src/windows/mod.rs:1123`).
- `enumerate_window` (multi-window scope **and the cache-warming thread**)
  never resets the counter, so its IDs keep counting up
  (`src/windows/mod.rs:627`).
- The cache-warming thread owns a **separate** `WindowsBackend` with its own
  map and counter, and shares only the cache, which stores `Element`s
  (including their IDs), not UIA elements.

On a cache hit the hotkey path checks whether it can use the cached entry:

```rust
let all_known = cached.iter().all(|e| self.elements.contains_key(&e.id));
if all_known { return Ok(cached.to_vec()); }
```

This asks whether the ID numbers exist in this backend's map. It doesn't ask
whether they refer to the same UIA elements.

### Failure scenario (likely; not yet reproduced on Windows)

1. Active-window scope. The user picks in window **A**, which has 300
   elements. The hotkey backend's map now holds IDs `0..299` → A's UIA
   elements.
2. The user switches to window **B**. The cache-warming thread searches B
   with its own counter (for example IDs `0..39` early in a session) and
   stores B's elements under B's window handle.
3. The user presses the hotkey in B within the cache lifetime (500 ms by
   default, and refreshed every 250 ms while the window moves or scrolls).
4. Cache hit. `all_known` passes because `0..39` all exist in the map from A.
5. The overlay draws labels at **B's** positions (bounds come from the cached
   `Element`), but `perform(id)` looks the ID up in the map built from **A**.
   The action goes to a control in window A, or to a stale UIA element.

The same thing can happen without the cache-warming thread: after a
foreground search resets the counter to 0, older cache entries for other
windows can have IDs that pass the check but point at the wrong UIA
elements. As the cache-warming thread's counter grows during a session,
false positives get less likely, so this would show up as an intermittent
"clicked the wrong thing", mostly soon after startup.

### Suggested fixes, from smallest to most thorough

1. **Quick fix: tag IDs with who created them.** Give each backend a unique
   number (from a global atomic) and either put it in the top bits of every
   `ElementId` or store it with the cache entry. Accept a hit only when it
   matches the current backend *and* its map hasn't been reset since. The
   cache-warming thread's entries would then always trigger a fresh search
   on the hotkey path, as the comment already intends.

2. **Better: cache the UIA elements, not just IDs.** If the cache held
   `(Element, UIElement)` pairs, a hit would be usable directly and the
   cache-warming thread would actually save time. Right now, every time the
   hotkey path correctly rejects the thread's entry, it walks the tree again
   anyway. The obstacle is that `UIElement` is a COM pointer tied to a
   threading model (the code already has an `unsafe impl Send` wrapper for
   moving search results between threads). Sharing one MTA UIA client
   between both threads would make this safe.

3. **Most thorough: make each search's result own its elements.** Have
   enumeration return a `Snapshot` that owns its UIA elements, and have
   `perform` take `(&Snapshot, index)` instead of a bare `ElementId`. Then:
   - Using an ID from one search with another search's elements becomes a
     compile error instead of a runtime check.
   - The backend no longer needs mutable ID state, so `perform` can take
     `&self`.
   - The overlay already returns an index, so this matches what the app
     does anyway.

   This fits the `ElementSource` trait sketched under weakness 3.

Whichever fix is chosen, add a test. The cache already takes an injectable
`Clock`, so a cross-backend cache-hit test is feasible with a fake element
source.

---

# Second pass: threading, config lifecycle, overlay

_Areas the first pass only skimmed. Ranked by impact._

## 8. Hot-reload can apply defaults over a working config (likely bug)

> **Status (2026-11-21):** **done.** Reload uses `try_load`: a missing file still means defaults, but a file that doesn't parse keeps the current settings and shows a warning naming the line. At startup a broken file runs on defaults with the same warning (shown before any hotkey-conflict box, since it explains why the chords reverted) and is left untouched. `save` writes `config.toml.tmp` and renames it over the file, falling back to an in-place write if the rename fails. Known limit: saving from Settings while the file is broken overwrites the hand edits with the in-memory settings; the startup and reload warnings make that state visible.

- `config_watcher` posts a reload 150 ms after a file change.
- The main loop calls `Config::load_or_default()`, which returns **defaults**
  when the file fails to parse (`src/config.rs:329`).
- Defaults differ from the current config, so `apply_config` runs. It
  re-registers hotkeys, resets colors and scope, rewrites the startup
  registry entry, and shows "Settings applied".
- `Config::save` uses a plain `std::fs::write` (`src/config.rs:370`), which
  isn't atomic. Editors also often truncate before writing.

So a typo while hand-editing `config.toml`, or a reload that reads a
half-written file, silently resets every live setting to the defaults, with
a toast saying it succeeded. The next good write fixes it, but a typo stays
in effect until the user notices.

**Fix:** in the reload path, use `try_load()`. On `Err`, keep the current
config and show "config.toml has an error at line N; keeping previous
settings". Make `save` atomic (write a temp file in the same directory,
then rename).

## 9. UIA calls run on the UI thread with no timeout set

> **Status (2026-11-21):** open. No connection or transaction timeout is set anywhere.

On a cold cache, `handle_pick_element` searches the UIA tree on the main
message-loop thread. `WM_GETOBJECT` uses `SendMessageTimeoutW` with a short
timeout, but no UIA connection or transaction timeout is set on the client,
so the library defaults apply. If the target app is hung, the search blocks
until UIA gives up. Meanwhile the tray, hotkeys and config reload are all
frozen. This is the practical cost of weakness 5.

**Fix:** set short timeouts on the hotkey client (`IUIAutomation2`
connection/transaction timeouts). Longer term, run cold searches on a
worker thread and show the overlay (or a "searching…" state) when results
arrive.

## 10. The cache-warming hook reacts to every window, not just the foreground

> **Status (2026-11-21):** **done, differently** (owner, round 6). After the #22 fix the hotkey path can't use a walk by the warming thread at all, so a location change now just **invalidates** that window's cache entry instead of queueing a walk: no debounce, and any top-level window (invalidating is cheap, and multi-window scopes cache background windows too). The walk on a foreground change is unchanged. Re-walking on move comes back with the shared cache (step 6). Note: the hook only sees the top-level window's own location changes (moves and resizes); scrolling inside a page most likely doesn't raise it, which is #21's problem and scroll mode's "scrolled" signal.

`win_event_proc` (`src/windows/prewarm.rs`) forwards
`EVENT_OBJECT_LOCATIONCHANGE` for **any** top-level window in any process.
The comment says "on the foreground window", but there's no
foreground check. The 250 ms debounce is also one global timestamp, not per
window.

- Background windows that move or animate (video players, progress
  animations, apps repositioning themselves) cause repeated full UIA
  searches of windows the user isn't using. Each search costs that app's
  UI-thread time and fills the cache with entries that active-window scope
  never reads.
- A chatty background window can use up the global debounce slot, so the
  foreground window's re-search after a scroll gets skipped. The cache then
  serves stale bounds for up to the cache lifetime.

**Fix:** drop location-change events unless `hwnd == GetForegroundWindow()`
(or, in multi-window scope, the set of windows that scope would search), and
debounce per window.

## 11. Multi-window searches bypass the cache and rebuild UIA clients on every press

> **Status (2026-11-21):** open.

In `active_monitor` / `all_windows` scope, only the foreground process's
windows go through `enumerate_window` (cache-aware). Every other process
group runs on a rayon thread that, **on every hotkey press**:

- creates a new `UIAutomation` client (`CoCreateInstance`), a prefetch
  request and a search condition
- calls `walk_window` directly, skipping the cache both ways (no reads, no
  writes)

So in multi-window scope the cache and the warming thread barely help, and
each press pays for client setup per process group. Keeping one UIA client
per rayon thread (thread-local) and routing these searches through the
cache would make multi-window mode about as fast as single-window mode.
This also depends on fixing weakness 6, since cached IDs would then come
from yet more sources.

## 12. The overlay redraws the whole virtual desktop on every keystroke

> **Status (2026-11-21):** open, and visible in the measurements: the owner's three monitors make a buffer of at least 8330 × 2160 × 4 bytes ≈ 72 MB, which matches jumpkey's 75–85 MB private bytes in the baseline. Drawing a dense-list overlay takes ~19 ms (telemetry).

`PersistentOverlay` keeps one 32-bit image the size of the **whole virtual
desktop** for the life of the process. Every render
(`render_to_dib`, `src/windows/overlay.rs:797`) clears the entire buffer and
hands the entire surface to DWM with `UpdateLayeredWindow`. That happens on
every keystroke, not just on open.

- Memory: two 4K monitors ≈ 66 MB held all the time; three ≈ 100 MB.
- Time: clearing and uploading scales with desktop size, not with the
  number of hints.
- `benches/overlay_render.rs` times `draw_all` and deliberately **excludes**
  the clear and DWM steps (see the comment on `draw_all`), so the benchmark
  doesn't cover the part that grows with display size.

**Options:** one layered window per monitor (or only for monitors with
hints); clear only the rectangles that changed since the last frame; use
`UPDATELAYEREDWINDOWINFO` with `prcDirty`; or free the image after a period
of inactivity. Add a benchmark case that includes the clear and present for
a 2×4K desktop.

## 13. Global state for Win32 callbacks

> **Status (2026-11-21):** open.

Win32 callbacks don't take a context argument, so per-module state lives in
statics: `PERSISTENT_OVERLAY` / `SCROLL_HOOK_TARGET` (thread-local),
settings `STATE` and `SETTINGS_WINDOW`, and the warming thread's
`SENDER` / `HOOKS` / `LAST_LOCATIONCHANGE`. This is a reasonable pragmatic
choice, but there are two sharp edges:

- `pick_hint` holds `PERSISTENT_OVERLAY.borrow_mut()` while `run_pick`
  runs a nested message loop. Anything that ever re-enters `pick_hint` from
  inside that loop (for example, a future change that dispatches hotkeys
  from a window procedure) would panic on the `RefCell`. Today hotkeys are
  polled from a channel in the outer loop, so it's safe, but only by
  convention.
- `SENDER` is a `OnceLock`, so `Prewarmer` can only be started once per
  process. That can't be relaxed later, for example if a setting is added
  to turn warming off and on again.

## 14. Smaller second-pass notes

> **Status (2026-11-21):** open.

- `apply_config` drops the old hotkeys before registering the new ones. If
  registration fails, the user has **no** hotkeys (they're told). Keeping
  the old set and rolling back on failure would be friendlier.
- The warming thread's `max_elements_global` is fixed at startup and isn't
  updated by `apply_config`.

## Revised priorities

_Superseded by the current roadmap at the top of this document._

1. Confirm and fix the element-ID / cache bug (#22, weakness 6).
2. Hot-reload safety (8): small change, prevents silent resets.
3. Foreground filter and per-window debounce in the warming hook (10):
   small change, saves time for other apps.
4. UIA timeouts on the hotkey client (9).
5. Linux CI job (2).
6. Move the pick flow into the library (3), then the overlay split (4) and
   overlay redraw cost (12).

---

# Third pass: UI model, config typing, public API

## 15. Three nested modal loops block the app

> **Status (2026-11-21):** open. Since PR #24, hotkeys pressed during the splash are no longer stuck until the next message; they fire right after it (#49).

The main thread runs the main loop, and three things start their own nested
message loop on top of it:

- the overlay (`run_pick`)
- the Settings dialog (`settings_window::show`, its own `GetMessageW` loop
  at `settings_window.rs:424`)
- **every notification** (`notification::show` → `MessageBoxW`, modal, with
  no owner window)

While any of them is open, the main loop doesn't run, so hotkey and tray
commands queue up and fire all at once when it closes. Message boxes are the
worst case:

- They're used for **successes** too ("Settings applied" after every save or
  file reload), not just errors.
- A message box raised from the config-watcher path (you saved `config.toml`
  in an editor) has no recent user input behind it. Windows' foreground lock
  can stop it from coming to the front, so it may sit behind the editor or
  flash in the taskbar. Until someone finds and dismisses it, jumpkey's
  hotkeys do nothing. _(Expected from Win32 foreground rules; worth checking
  on Windows alongside #22.)_

**Direction:** treat modal states as app state on one event loop rather than
nested loops. Short term, the cheap fixes are:
- Use tray balloon notifications (`Shell_NotifyIconW` with `NIF_INFO`) for
  info and warnings, or simply log successes.
- Keep `MessageBoxW` for real errors only, and give it an owner (or
  `MB_SETFOREGROUND` / `MB_TOPMOST`) so it can't hide.

## 16. Config is mostly strings, parsed late and inconsistently

> **Status (2026-11-21):** open.

Hotkeys, colors and badge shape are stored as `String` and parsed wherever
they're used:

| Value | Parsed in | On a bad value |
|---|---|---|
| hotkeys | `hotkey::register_from_config` | reported as a conflict |
| colors | `overlay::apply_color_override` | log warning, default kept |
| shape | `overlay::apply_shape_override` | log warning, default kept |
| Settings Save | `settings_window::validate` | checks hotkeys and the two `badge_bg` colors only; `badge_fg`, `border` and `leader_color` aren't checked |

Effects:
- A typo in a hand-edited file shows up as "the color just didn't change",
  with the reason only in the log.
- The rules for a valid config are spread across four modules.
- `settings_window` imports `overlay::parse_hex_color` and
  `overlay::BadgeShape` to validate config, so the dialog depends on the
  renderer to understand the config schema.

**Fix ("parse, don't validate"):** add types such as `HexColor`, `Chord`
and `BadgeShape` with `Deserialize` impls that reject bad input. Then
`toml` reports the exact line on load, the Settings dialog reuses the same
types, and the overlay and hotkey code receive values that are already
valid. Together with #8, a bad file becomes "error at line 12, keeping
previous settings".

Related: `hints.alphabet` stores a value derived from
`preset + flags + layout`, and the comments in `main.rs` admit it's
recomputed every launch as "defence-in-depth". Storing derived data
invites the two to disagree. Either drop the field when a preset is in use,
or make it `custom_alphabet` and only read it when `preset = "custom"`.

## 17. Adding a setting means editing many places

> **Status (2026-11-21):** open. Scroll mode's hotkey (#54) will be the next example: eight places in `settings_window.rs` alone.

One new config field currently needs edits in:

1. `config.rs` (struct, `Default`, serde attrs)
2. `settings_window::build_controls` (create the control)
3. `settings_window::collect_config` (~170 lines of field-by-field reads)
4. `settings_window::on_reset` / initial-value setup
5. `settings_window::validate` (if it can be invalid)
6. `main::apply_config` (push it into the live subsystem)
7. sometimes `tray.rs`

Nothing checks that you did all of them. Forgetting step 6 gives a setting
that only applies after a restart (the warming thread's `max_elements` in
#14 is an example). Options, from light to heavy:
- A declarative field table (id, label, kind, getter/setter on `Config`)
  that drives building, collecting and resetting the controls.
- Rebuild affected subsystems from the new `Config` instead of patching
  them field by field (`apply_config` becomes "construct new X, swap in";
  hotkeys and the UIA client are the only parts expensive enough to diff).

## 18. The library API is wider than intended

> **Status (2026-11-22):** **decided** (round 7): the library is internal, not published (`publish = false`), and has no stability promise; the README no longer advertises crates.io, docs.rs or a library API. Nothing to narrow. The portable-core rules in the roadmap replace "keep a small public API".

The crate is published to crates.io as a library, and `pub mod windows`
exposes everything: `settings_window::show`, `tray::Tray`,
`prewarm::Prewarmer`, `overlay::pick_hint`, `window_picker::*`,
`hotkey::*`, notification helpers, splash screen, single-instance guard,
and so on (roughly 190 public items). `backend.rs` describes the trait as
"intentionally small so it can be kept stable", but the real public
surface is the whole app.

Each refactor proposed in this document (the `Snapshot` type, an
`ElementSource` trait, splitting the overlay, typed config) is technically a
breaking change to that surface. At 0.x that's allowed, but it forces
version bumps for changes no outside user cares about, and docs.rs presents
internals as API.

**Decide on purpose:**
- If there are no real outside users: make `windows::*` `pub(crate)` except
  what the examples and benchmarks need (or `#[doc(hidden)]` plus a
  "not stable" note), and treat the library as an implementation detail.
- If it should be a real library: keep the public API to the core types,
  `HintEngine`, the keyboard model and a `Backend`/`ElementSource` trait,
  and hide everything else.

## 19. What's strong in the newer code (for balance)

> **Status (2026-11-21):** still true.

- **The keyboard model** (`keyboard.rs`) keeps physical keys, layout and
  finger geometry as separate concepts and maps between them explicitly.
  That's why layout-aware labels, Dvorak/Colemak support and the
  same-finger rule fit together without special cases.
- **`HintEngine`** takes its constraints from outside (key rule, finger
  table, minimum singles) and loosens the rule in a fixed order rather
  than running out of labels. It's a small, well-tested piece with a
  focused API.
- **`hotkey::parse_hotkey`** has thorough table-style tests, including
  edge cases (`Ctrl++Space`, `\` vs `Backslash`, whitespace).

These are examples of what the rest of the codebase could converge on:
pure logic, typed inputs, failures reported at the edge.

## Revised priorities (after third pass)

_Superseded by the current roadmap at the top of this document._

1. #22 element-ID / cache bug.
2. Config reload safety (8) together with typed config (16). Together these
   turn bad config files from silent resets into clear errors.
3. Warming-hook foreground filter (10).
4. Replace success message boxes with non-blocking notifications (15).
5. UIA timeouts (9).
6. Decide the library's public API (18) **before** the larger refactors,
   since it decides how much those refactors break.
7. Linux CI (2), pick flow into the library (3), overlay split (4) and
   redraw cost (12), a declarative Settings dialog (17).

---

# Focus: correctness and labelling speed

_The owner's top priorities are correctness and labelling speed: picking
should feel very snappy and intuitive. Config and the Settings UI come
second. This section re-reads the findings through that lens and adds the
hot-path issues found while doing so._

## 20. The cache-warming thread almost never speeds up a real pick

> **Status (2026-11-21):** **confirmed by measurement.** Pressing 300 ms or 2 s after a switch costs the same (dense-list 742 vs 748 ms p50): the warming search never finishes in time. Repeat presses alternate between a hit (~25 ms) and a full search, because a hit doesn't refresh the entry's 500 ms lifetime.

`CacheManager::get` treats an entry as fresh for `cache_ttl_ms` from when it
was **inserted** (default **500 ms**, `config.rs:309`). The warming thread
inserts once, when a window comes to the foreground (and again when it
moves). It doesn't refresh entries while you sit in a window.

The usual flow is: switch to a window, look at it for a second or more,
press the hotkey. By then the warmed entry is older than 500 ms, so
`get` returns `None` and the hotkey path runs a full synchronous UIA search
on the main thread. The warmed entry only helps if you press the hotkey
within half a second of switching, or when retrying with Esc.

Then there are three more problems on top of that:
- When the entry *is* fresh, #22's check was a coin flip: either it
  rejected the warmed entry (re-search anyway) or wrongly accepted it
  (wrong element). _(2026-11-21: fixed; the warmed entry is now always
  rejected, so it can only help once the cache shares UIA elements,
  roadmap step 6.)_
- #10 has the thread re-searching background windows the user isn't
  using.
- #11 multi-window scopes skip the cache for everything except the
  foreground process.

**Net: the warming thread costs CPU and other apps' UI-thread time, yet
most picks run cold.** This is probably the single biggest gap between
the intended design and how fast it feels in practice.

## 21. The short TTL is the only protection against stale labels

> **Status (2026-11-21):** open. Scroll mode (#54) adds the first event-driven freshness signal: it knows when it scrolled a window.

The 500 ms TTL is the only thing preventing stale labels. Nothing
invalidates an entry when:
- page content changes (SPA navigation, lazy loading, expanding panels)
- the window scrolls internally (the hook only watches top-level window
  moves, `OBJID_WINDOW`)
- a menu or dialog opens inside the same top-level window

So today's design picks correctness by giving up caching (TTL so short the
cache rarely hits). Making the TTL longer would make picks fast but put
labels in the wrong places. Fixing this needs a different freshness model,
not a different number.

## 22. Nothing checks the chosen element before acting on it

> **Status (2026-11-21):** open.

`perform` acts on the stored `UIElement` without checking that it still
exists or is where its label was drawn. With any caching, and even without
it on a page that changes while the overlay is open, the stored element
could be gone, moved or hidden. The cascade then either fails (logged
as a warning only, so the user sees nothing happen) or acts on something
that has moved away from the label.

A cheap guard: re-read the chosen element's `BoundingRectangle` (one
cross-process call, roughly a millisecond) and compare it with the
bounds the label was drawn from. If it's gone, or has moved more than a
few pixels, don't act. Re-search and re-show the overlay, or flash a "page
changed, try again" hint. This one check makes caching much safer and also
catches the #22 failure mode (the right ID pointing at an element in another
window would fail the bounds check).

## 23. No measurements of hotkey → overlay latency

> **Status (2026-11-21):** **done.** Outside: `tools/pick-bench` (PR #25, #27). Inside: the `telemetry` cargo feature (PR #26) records a span tree per pick, one JSON line per pick, plus a summary line in the log; see `docs/TELEMETRY.md`. Builds without the feature still have no timing log.

The pick path has no timing logs. The README's goal is "sub-50 ms hint
overlay", but nothing measures it, so it's impossible to tell whether a
slow pick was the UIA search, sorting and labelling, rendering, or the
first frame reaching the screen. The benchmark covers only drawing badges
(and leaves out the full-desktop clear and present, #12).

**Add (before optimising anything else):** one `info` line per pick with:
- `t_hotkey` → elements ready (+ cache hit/miss and who made the entry)
- → labels generated
- → first frame presented (`UpdateLayeredWindow` returned)
- → key that completed the label → `perform` returned (+ which invoke path
  was used)
- element count, browser/desktop, scope

That gives a real distribution from daily use and shows which of the
items below is worth doing first.

## 24. The first pick of a session pays for setting up the overlay

> **Status (2026-11-21):** **measured: small.** The first press after start costs ~20–50 ms more than a normal press on desktop apps (Explorer 175 vs 155 ms, VS Code ~105 vs 70, Terminal ~67 vs 37). On a freshly opened browser page the first press is dominated by Chromium building its tree (~1.1–1.2 s on small pages), not by the overlay. Still worth doing, low priority.

`PersistentOverlay` is created lazily on the **first** hotkey press:
registering the window class, creating the window, allocating and zeroing
a desktop-sized buffer (#12), setting up DirectWrite and fonts. The splash
screen already holds the user for 2.5 s at startup. Building the overlay
(hidden) during that time removes the first-press penalty for free.

## Proposed freshness design (fixes 20–22 together)

Goal: always show labels from memory instantly, and never act on a stale
element.

1. **One owner for UIA elements** (fix option 2 from #22). Both the warming
   thread and the hotkey path go through one shared MTA UIA client. The
   cache stores `(Element, UIElement)` per window, along with the window's
   rect and a generation number. Accepting a warmed entry is then always
   correct, and the warming thread actually pays off.
2. **Freshness by events, not just age.**
   - Invalidate on foreground change, foreground window move or resize, and
     mouse wheel or scroll keys in the foreground window (a low-level
     hook, or a scroll-pattern property event).
   - Optionally subscribe to UIA structure-changed events on the
     **foreground window only** for content changes. Measure the cost
     first, since browsers produce a lot of these events.
   - Keep a longer safety maximum age (for example 5–10 s) instead of
     500 ms.
3. **Serve stale entries, refresh in the background.** On hotkey: show
   labels from the cache immediately if there's any entry for this window
   (and its rect still matches). At the same time, start a background
   re-search for next time. **Don't relabel while the overlay is open.**
   Moving labels under the user's fingers is worse than a slightly stale
   set.
4. **Check before acting** (#22 guard above). This is the correctness
   backstop that makes steps 2–3 safe. If the check fails, re-search and
   re-show, keeping the typed prefix if the same element is still there.
5. **Cold searches off the UI thread** (#9). With a cache miss, show the
   overlay window immediately (optionally with a subtle "searching"
   state), search on a worker, and draw labels when results arrive. The
   main loop stays responsive, which also fixes the "fire all at once"
   behaviour (#15) for the overlay case.

Also worth considering for "intuitive": **stable labels**. With
center-out ordering, labels can shift when one element appears or
disappears. Caching per window plus preferring the previous label for the
same element (matched by `AutomationId`/name plus bounds) would make
repeat picks build muscle memory. Do it later, but design the cache so it's
possible.

## Correctness + speed roadmap

_Superseded by the current roadmap at the top of this document._

| # | Item | Why it matters for picks | Size |
|---|---|---|---|
| 1 | Pick timing logs (23) | Find out where the milliseconds go | S |
| 2 | Verify on Windows, then fix #22 (issue #22) | Wrong-element clicks | S–M |
| 3 | Check before acting (22) | Never act on a stale or moved element | S |
| 4 | Warming hook: foreground-only, per-window rate limit (10) | Stop wasted re-searches; refresh the window you'll actually pick in | S |
| 5 | Build the overlay during the splash screen (24) | Remove the first-press penalty | S |
| 6 | UIA timeouts on the hotkey client (9) | A hung app can't freeze jumpkey | S |
| 7 | Shared UIA element cache + event-based freshness + serve-stale (20, 21) | Make most picks warm without stale labels | L |
| 8 | Cold searches off the UI thread (9, 15) | Responsive even on a miss; no queued hotkeys | M–L |
| 9 | Partial redraw / per-monitor overlay (12) | Faster typing on large desktops | M |
| 10 | Multi-window scopes through the cache, one UIA client per thread (11) | Fast picks in monitor/all scopes | M |

Items 1–6 are each a small patch; 1–3 should come first. Item 7 is the
real architectural change, and doing 1 and 3 first makes it measurable and
safe.

---

# Fourth pass: what gets labelled, focus, platform edges

_Exploration only. Several items are hypotheses from reading the code;
each comes with a way to check it on Windows._

## 25. Nested-element dedup is role-blind and keeps the innermost element

> **Status (2026-11-21):** open; confirmed in practice by the owner (round 1). `dedup_nested_records` is now `src/windows/mod.rs:1704`.

`dedup_nested_records` (`src/windows/mod.rs:1704`) runs on **every** search,
browser and desktop. For any clickable element that contains another:

- Rule 1: if the outer element's area is more than 1.5× the inner one's,
  drop the outer.
- Rule 2: if it contains exactly one other record, drop the outer.

Neither rule looks at **role**. The innermost clickable always wins, which
is often not the element the user is aiming for:

| Container (dropped) | Inner (kept) | What the user probably wanted |
|---|---|---|
| Browser tab (`TabItem`) | its close `×` button | switch to the tab |
| File / mail row (`ListItem`/`DataItem`) | row checkbox or star | open the item |
| Search box (`Edit`) | clear / search-icon button | type in the box |
| Card with one inner link | the link | often the same, fine |
| `ComboBox` | its inner "Open" button | equivalent, fine |

The results also depend on order: elements already dropped don't count
towards later checks (`!keep[j]`), so the same tree in a different
enumeration order can produce a different set of labels.

**Hypothesis to check:** in Chrome or Edge with several tabs, press the
element hotkey on the tab strip. Are the tabs themselves labelled, or only
their close buttons? Same for Explorer in Details view with item checkboxes
on, and for Gmail rows.

**Design questions:**
- Should some roles always survive dedup (tabs, list rows, text inputs)?
- When a container and its child are both real targets, should both get
  labels (the child's offset), or should the container get the label and
  a modifier reach the child?
- Should dedup work on the UIA **parent/child relationship** (which
  is known during the walk) rather than geometry alone?

## 26. The overlay takes keyboard focus, which may close transient UI

> **Status (2026-11-21):** open. The owner chose the no-activate + keyboard-hook model for scroll mode (#54), which makes scroll mode the first production user of this design.

`run_pick` shows the overlay with `SW_SHOWNOACTIVATE`, then immediately
calls `SetForegroundWindow` + `SetFocus` on it so keystrokes reach it. When
the target app loses activation:

- **Popup menus, dropdowns, autocomplete lists, flyouts and tooltips
  usually close.** The search already captured their items, so the user
  may see labels over a menu that has disappeared, or the labels' targets
  are gone by the time `perform` runs.
- Some apps clear the selection or caret state when deactivated.
- When the overlay hides, focus isn't explicitly returned to the
  original window. Windows activates whatever is next in z-order, which is
  usually, but not always, the app the user was in.

**Hypothesis to check:** open a context menu (right-click on the desktop,
or Shift+label on a file in Explorer), then press the element hotkey. Can
you pick a menu item? Repeat with an open `<select>` dropdown and a
browser address-bar suggestion list.

**Alternative design:** never activate the overlay (`WS_EX_NOACTIVATE`),
and read keystrokes with a low-level keyboard hook (`WH_KEYBOARD_LL`) that
swallows them while the overlay is open. The target app keeps focus, so
menus stay open and "pick inside a menu" works. Trade-offs to weigh:
- Low-level hooks have to return quickly. If the hooking thread stalls
  (for example a UIA call on the main thread, #9), Windows times out the
  hook and can silently remove it, so this pushes harder towards a
  responsive main thread.
- Swallowing keys globally needs careful handling of modifiers, IME and
  stuck keys.

This choice decides whether jumpkey can drive chains of transient UI
(menu → submenu → item) entirely from the keyboard. It's worth deciding
early.

## 27. "Invoke" is the same for every role

> **Status (2026-11-21):** open. Telemetry now records `invoke_path` per pick, so a week of daily data will show how often each path is used.

`invoke_smart` tries the same order for every element (Invoke → Toggle →
SelectionItem → ExpandCollapse → legacy default action → click), whatever
its role:

- **Text inputs:** `Edit` usually has none of the first four patterns. The
  legacy default action may "succeed" without moving keyboard focus into
  the field, so the user picks a search box and their typing goes
  elsewhere. The intuitive action for a text input is *focus and place the
  caret*, which is what a click does.
- **Combo boxes:** ExpandCollapse only expands (documented). Picking an
  already-open combo box does nothing.
- **Elements with several patterns:** for example a tab that exposes both
  Invoke and SelectionItem, or a tree item where Invoke opens the item but
  the user only wanted to select it. The cascade order decides, not the
  role.

**Design question:** should the default action come from a **role →
action table** (TextInput → focus/click, Tab/ListItem → select, Button/Link →
invoke, ComboBox → toggle open/closed, Checkbox → toggle)? That's easy to
test as pure logic, and #23's timing log could record which path each pick
took, so you can see when the fallback click is used.

## 28. DPI awareness is set after windows already exist (hypothesis)

> **Status (2026-11-21):** open, unverified. The owner's 144-dpi 4K monitor makes it checkable.

The process asks for Per-Monitor-V2 DPI awareness at runtime in
`WindowsBackend::with_shared_cache` (`mod.rs:279`) and again in the overlay,
and **ignores the result** (`let _ =`). But `run_windows` creates the
**splash window** and the **IPC window** *before* the backend
(`main.rs:197, 208, 225`). There's no application manifest declaring DPI
awareness.

Windows documentation recommends setting DPI awareness in the manifest or
before any window exists, and the call fails if the awareness is already
fixed. If creating those earlier windows fixes the process as DPI-unaware or
system-aware, UIA's physical-pixel rectangles and the overlay's
coordinates will disagree on scaled displays (125 %, 150 %). Labels would be
offset from their targets, and worse further right and down on the
screen.

**Check:** on a 150 % display, log the return value of
`SetProcessDpiAwarenessContext` and of `GetThreadDpiAwarenessContext()`
after startup. Are labels exactly aligned at the far bottom-right of the
screen?

Related, mixed DPI: `run_pick` scales the **whole** style from the DPI of
the monitor holding the **first** hint (`dpi_scale_for_bounds(first)`). In
the monitor or all-windows scope on a laptop (150 %) plus an external
display (100 %), every label uses one monitor's size.

## 29. Elevated windows and system UI are probably out of reach

> **Status (2026-11-21):** open. Also limits scroll mode: UIPI blocks synthesized wheel input to elevated windows.

There's no manifest at all, so no `uiAccess`. Without it, a standard-user
jumpkey process:
- can read but generally can't **act on** windows running as admin (Task
  Manager, an admin terminal, installers). UIPI blocks the UIA patterns
  and synthesized input.
- can't draw above some system surfaces (Start menu, some shell flyouts,
  the lock screen and UAC prompts, which is correct).

`uiAccess="true"` needs a **signed** binary installed in a protected
location (Program Files). The MSI already installs there, but code signing
is deferred (`docs/CODE_SIGNING.md`). This is a product decision more than
a code one: "works on every window, including admin ones" depends on
signing.

**Questions:** Is labelling elevated windows a goal? Should jumpkey at least
**detect** elevated targets and say "this window is running as
administrator" instead of failing silently?

## 30. Filtering rules are tuned by hand with no test corpus

> **Status (2026-11-21):** open. pick-bench's fixture pages are a start for timing, not for label correctness.

What gets labelled is decided by hand-tuned constants and rules:
`MIN_HINT_PX = 12`, `MAX_HINT_PX = 1500`, web 10 / 16 px floors, the 1.5×
dedup ratio, the structural-type rule, the browser depth of 32, the
per-window cap of 80, and the global caps of 500 / 800. Each was clearly
tuned against real apps (the comments mention specific ones), but nothing
records **which** apps and **what** the right answer was. A change that
fixes Gmail can quietly break Explorer.

**Idea: a recorded UIA fixture corpus.**
- Extend `examples/enumerate_foreground.rs` to save a snapshot of a
  window's UIA tree (control type, bounds, names, patterns, focusable,
  offscreen, parent/child) to JSON.
- Collect snapshots from the apps that matter (browser with Gmail and
  GitHub, VS Code, Explorer, Settings, Office, Teams/Slack/Electron).
- Run the filtering, dedup, occlusion, ordering and labelling over those
  snapshots in normal unit tests, with golden expected outputs ("these
  elements are labelled, with these labels").

Payoffs:
- Questions like #25 can be answered by looking at the data.
- The tests run **on macOS and Linux CI** too (they're pure logic), which
  also helps #2.
- It needs the filter code to work on plain data rather than live
  `UIElement`s, which pulls in the same direction as the `Snapshot`
  idea from #22.

## Open questions for the owner

These would shape the architecture more than any single fix:

1. **Which apps matter most?** A top-10 list would define the fixture
   corpus (#30) and how to tune #25 and #27.
2. **Pick inside transient UI:** should menus, dropdowns and autocomplete
   lists be pickable? If yes, the focus model (#26) has to change.
3. **When a clickable is inside a clickable** (tab + ×, row + checkbox),
   what should the user see?
4. **What should picking a text field do:** focus only, focus + select
   all, or click at the caret?
5. **Label stability:** should the same element keep the same label across
   picks of the same window (muscle memory), even if that costs a few
   extra keystrokes sometimes?
6. **Elevated windows:** a goal, a non-goal, or "detect and explain"?
7. **Latency budget:** is 50 ms from hotkey to overlay the right target
   for both warm and cold picks, and what's acceptable for a cold
   browser page?
8. **Wrong-pick feedback:** when the pre-action check fails or an action
   fails, how should jumpkey tell the user (flash, re-show, sound, nothing)?
9. **Scope defaults:** do you actually use the monitor or all-windows
   scopes day to day? Their cost (#11) depends on that.

---

# Owner decisions (2026-11-20)

- **#25 confirmed in practice:** the owner has seen tabs and rows lose
  their labels without knowing why. Follow up later.
- **Menus and submenus must be pickable** (#26). This is a product goal,
  not a nice-to-have.
- **Mixed-DPI multi-monitor is on the roadmap.** The owner runs a primary
  monitor at normal scaling and a secondary 4K monitor.

---

# Fifth pass: what menu support and mixed-DPI support require

## 31. Menu picking is blocked in three separate places

> **Status (2026-11-21):** open (design).

Taking focus (#26) is only one of three blockers. Even with a
no-activate overlay, menus still wouldn't get labels.

### 31a. Discovery: popup windows are never searched

Menus, dropdowns, autocomplete lists and flyouts are almost always
**separate top-level windows**:
- Win32 context and menu-bar menus: class `#32768`, unowned, usually
  `WS_EX_TOOLWINDOW` + topmost, no title.
- Chromium and Electron menus and `<select>` popups: owned
  `Chrome_WidgetWin_*` popup windows.
- WinUI / XAML flyouts: often a separate popup host window.

Jumpkey never looks at these:
- `active_window` scope searches only `GetForegroundWindow()`. While a
  menu is open, the foreground window is still the owner app, not the
  menu.
- The monitor and all-windows scopes build their list from
  `window_picker::enumerate_visible`, which removes exactly these
  windows: owned windows (`GA_ROOTOWNER != hwnd`), `WS_EX_TOOLWINDOW`,
  and windows with no title. Those filters are right for the **window
  picker**, but the element picker reuses them for a different purpose.

There's no menu awareness anywhere in the code: no `GetGUIThreadInfo`,
no menu WinEvents, no UIA `Menu` lookup at the desktop root.

### 31b. Occlusion: popups don't hide what's under them

Because popups aren't in the window list, they also aren't treated as
**occluders** (#19's hidden-element pass). If a dropdown is open over part
of the page, page elements under it are still labelled, drawn on top of
the dropdown.

### 31c. Freshness: nothing notices a menu opening or closing

Opening a submenu changes the screen without moving the foreground window,
so the warming thread doesn't react. Worse, if the user picks "View" and
then immediately presses the hotkey again for the submenu, the cache entry
(< 500 ms old) is from **before** the submenu opened, so its labels are
stale.

### Signals Windows offers

- `GetGUIThreadInfo(target_thread)`: flags `GUI_INMENUMODE` /
  `GUI_POPUPMENUMODE`, plus `hwndMenuOwner`. One cheap call that says "a
  menu is open in this app".
- WinEvents `EVENT_SYSTEM_MENUSTART/END` and
  `EVENT_SYSTEM_MENUPOPUPSTART/END`, raised by Win32 menus and many
  frameworks. The warming hook (#10) could subscribe to these to search
  a popup the moment it appears.
- UIA: menus show up as `ControlType::Menu` children of the desktop root.
  A children-only search filtered by the foreground window's process id is
  cheap.
- Plain `EnumWindows`: visible top-level windows of the **foreground
  process** that are above its main window in z-order. This covers owned
  popups, menus and flyouts with no framework-specific code.

### What this suggests

The **unit of searching** should be the **foreground app's window
stack** (main window + its visible popups and dialogs, in z-order), not
"the foreground HWND". Popups get searched first, act as occluders for
the windows below them, get labels first (they're almost always what the
user is about to pick), and key their own cache entries. This is a change
to what "active window" means, and it fits naturally with the freshness
redesign (§ "Proposed freshness design").

### Questions for the owner

1. **Chaining:** after picking an item that opens a submenu, should the
   overlay **automatically re-appear** for the submenu (a "menu mode"
   that lasts until a leaf item is picked or Esc), or should the user press
   the hotkey again each time? Auto-chaining makes the menu → submenu →
   item flow feel like one gesture.
2. When a menu is open, should **only** the menu get labels (the
   rest of the app is "behind" it), or the menu plus the visible parts of
   the app?
3. Should the hotkey **open** menus too? For example, a label on a
   menu-bar item opens it and immediately enters menu mode.

## 32. Mixed-DPI: what has to change

> **Status (2026-11-21):** open (design). Owner decision: per-monitor DIPs, per-monitor overlays.

The owner's setup (primary at 100 %, secondary 4K at likely 150–200 %) hits
every single-scale assumption in the overlay.

### 32a. One scale for the whole pick

`run_pick` computes the scale from the **first hint's** monitor and applies
it to the whole style (#28). The first hint is the one nearest the anchor,
so which monitor "wins" depends on where focus or the cursor is. Labels on
the other monitor are too small (4K side) or too big (primary side).

### 32b. Layout assumes one badge height

`lay_out` uses a single `row_h = font_height + 2 * padding_y` for
collision stacking, and `TextMetrics` is measured once per pick. Per-
monitor sizes need per-hint metrics and a collision pass that handles
mixed heights. **Good foundation:** `LabelText` already caches fonts per
pixel height and states that it "serves any mix of badge sizes", so the
text layer is ready; the layout layer isn't.

### 32c. Font size is stored as physical pixels in config

- `BadgeColors.font_size` is physical pixels. Setting it **turns off DPI
  scaling completely** (`auto_scale_dpi = false`,
  `overlay.rs:347-349`).
- The Settings dialog suggests a size computed from the **primary**
  monitor's DPI (`suggested_element_font_size`).

So any user who touches the size slider gets a size that's right on one
monitor and wrong on the other. On mixed DPI, there's no single correct
number to store. **Config should store size in DIPs (logical points)**,
and the overlay should always scale per monitor. That's a config-format
change worth making before mixed-DPI work, and it ties in with typed config
(#16).

### 32d. The window picker uses primary-monitor sizes

`suggested_window_font_size` and the window picker's style are based on
`primary_monitor_dpi_scale()`. Window badges on the 4K monitor are sized
for the primary.

### 32e. One overlay window across the whole virtual desktop

The single layered window spans every monitor's bounding box. With a 1080p
primary next to a 4K secondary, that's about 5760 × 2160 × 4 bytes ≈ 50 MB,
including the empty area below the shorter monitor, cleared and presented
on every keystroke (#12).

**Per-monitor overlay windows** would solve several problems at once:
- Each window lives on one monitor, so it has exactly one DPI: per-monitor
  scaling becomes "use this window's DPI".
- Memory and per-keystroke cost cover only the actual monitor areas, and
  monitors with no hints need no window at all.
- Hotplug or rearranging monitors becomes "rebuild the window for that
  monitor" instead of reallocating one big buffer.

Cost: a hint and its callout tail must stay on one monitor (already true;
`rect_inside_monitor` keeps badges inside a monitor), and input handling
needs one owner (with a no-activate overlay and a keyboard hook (#26), no
overlay window needs focus at all, which makes this easier).

### 32f. Coordinates for apps that aren't DPI-aware

UIA normally returns physical coordinates to a per-monitor-aware client,
but older **DPI-unaware and system-aware apps on the non-primary monitor**
are a known source of wrong rectangles. On the owner's 4K secondary this
is the most likely place for "labels offset from targets" reports, and it
can't be told apart from #28 without testing.

### Checks on the owner's own setup

| Check | Expected if healthy |
|---|---|
| Log `SetProcessDpiAwarenessContext` result + thread awareness at startup | success / PMv2 |
| Element pick on the 4K monitor, focus on the primary (all-windows scope) | 4K labels sized for 4K |
| Labels at the far bottom-right of the 4K monitor | exactly on target |
| An old DPI-unaware app (e.g. a legacy Win32 tool) on the 4K monitor | aligned, or document as known issue |
| Drag a window across monitors, pick immediately | labels match new position (tests freshness too) |
| Window picker with windows on both monitors | badges readable on both |

### Questions for the owner

1. Should labels look the **same physical size** on both monitors, or the
   same **relative** size (scaled with each monitor's Windows scaling)?
   Per-monitor DIP scaling gives the second.
2. Is per-monitor overlay windows acceptable as the direction? It's the
   biggest overlay restructure in this document, but it addresses #12,
   #28 and #32 together.

## How the big pieces connect

Most of the large changes in this document turn out to be the same few
decisions, approached from different sides:

```
            no-activate overlay + keyboard hook (#26)
               │ needs a responsive main thread
               ▼
   searches off the UI thread (#9, #15) ◀── freshness redesign (#20–22)
               │                                 │
               ▼                                 ▼
   foreground app *window stack* (#31) ──▶ shared UIA element cache (#22)
               │                                 │
               ▼                                 ▼
   per-monitor overlay windows (#12, #32) ◀── Snapshot / ElementSource seam (#3)
```

_Added 2026-11-21:_ **scroll mode (#54)** sits on the left column: it
needs the no-activate window and keyboard hook (#26), forces UIA work off
the main thread (#9), and its indicator is a small per-monitor window
(#37). Building it before the state machine tests those pieces early;
its "window scrolled" signal later feeds the freshness redesign.

Suggested way to think about sequencing: **settle the focus model (#26)
and the unit of searching (#31) first**, because they set the constraints
for threading, caching and the overlay structure. The small correctness
fixes (timing log, #22, pre-action check, warming-hook filter) can go in
before any of it, independently.

---

# Owner decisions, round 2 (2026-11-20)

| Topic | Decision |
|---|---|
| Submenus | **Auto re-show** the overlay when a pick opens a submenu ("menu mode"). Must be **switchable off** in settings. |
| While a menu is open | **Experiment:** label *only* the menu, and judge whether it's good or annoying. |
| Label size across monitors | Scale with **each monitor's Windows scaling** (per-monitor DIPs). |
| Overlay structure | **Per-monitor overlay windows**: approved direction. |
| Menu bar | Picking a menu-bar item **opens it and enters menu mode**. |

---

# Sixth pass: designing menu mode, experiments, per-monitor overlays

## 33. Menu mode can know *before* acting whether a pick opens a submenu

> **Status (2026-11-21):** open (design).

UIA menu items that own a submenu expose `ExpandCollapsePattern`, and items
without one report `ExpandCollapseState_LeafNode` (or have no such pattern).
Both are available from the cached properties of the same search (add
`ExpandCollapseState` to the prefetch request). So, at pick time:

```
chosen is MenuItem (or menu-bar item)?
 ├─ has a submenu (state = Collapsed / PartiallyExpanded)
 │    → expand it → wait for the popup → search it → re-show overlay (menu mode continues)
 └─ leaf (state = LeafNode / no pattern)
      → invoke it → exit menu mode
```

This is better than "act, then see whether a popup appears", which would
need a timeout to decide "no popup came". Menu-bar items (the owner's #5)
are the same case: a `MenuBar` > `MenuItem` with a submenu.

**Things menu mode needs that don't exist today:**

1. **`perform` has to report what happened.** It returns `Result<()>`
   now. Menu mode needs something like
   `Outcome::{Invoked, OpenedSubmenu(popup hint), Toggled, Focused, FellBackToClick}`.
   That's also what the timing log (#23) and the role → action table (#27)
   want to record. Worth designing once, for all three.
2. **Waiting for the popup.** After expanding, the submenu window appears
   asynchronously, and menus can **animate** (fade, or slide, which
   changes the rectangle while it plays). Signals, best first:
   `EVENT_SYSTEM_MENUPOPUPSTART` for the new popup, then the UIA
   structure under it settling, then a short time cap. Bounds captured
   mid-slide would put labels in the wrong place, so this is a real
   correctness concern. **Question:** how long should jumpkey wait at most
   before giving up and showing whatever it has?
3. **A state machine instead of a one-shot pick.** Today: hotkey → pick →
   act → done. Menu mode is: `Idle → Picking(level 0) → Acting →
   WaitingForPopup → Picking(level 1) → … → Idle`. That state needs to
   live somewhere testable. It's a natural fit for the library-side pick
   flow (#3), and a strong argument for building that seam before menu
   mode rather than after.
4. **Esc semantics, a question:** in native menus, Esc closes **one
   level**. Should Esc in menu mode close the current submenu and re-show
   labels for the parent menu (matching native behaviour), or leave menu
   mode completely? Backspace with an empty prefix could be "go up one
   level".
5. **Menu mode starting by itself:** if the user opens a menu with the
   mouse or Alt key and *then* presses the hotkey, `GetGUIThreadInfo`
   (`GUI_INMENUMODE`) tells jumpkey to start directly in menu mode.

**Frameworks to keep in mind** (each exposes menus a bit differently):
Win32 `#32768` (reliable patterns), Chromium/Electron (custom popup windows,
ARIA roles), WinUI/XAML (`MenuFlyout`), Office (its own command bars and
ribbon; KeyTips are Office's native equivalent). The fixture corpus (#30)
should include an open menu from each.

## 34. The riskiest assumption: does a topmost no-activate overlay keep menus open?

> **Status (2026-11-21):** open. Proposed as roadmap step 2, widened to also answer scroll mode's questions (#54).

The whole menu plan relies on this: **showing a non-activating, topmost,
layered window over an open menu does not close the menu.** Win32
menus run a modal loop in the target app's thread and close on
activation changes, `WM_CANCELMODE`, and some mouse and keyboard events.
A window that never activates *should* leave it alone, but this is the
first thing to prove, with a throwaway prototype on Windows, before
designing further:

1. Open a context menu in Notepad, Explorer, Chrome and a WinUI app.
2. Show a `WS_EX_NOACTIVATE | WS_EX_TOPMOST | WS_EX_LAYERED |
   WS_EX_TRANSPARENT` window over it.
3. Install `WH_KEYBOARD_LL` and swallow a few keys.
4. Does each menu stay open? Do swallowed keys stay away from the menu?
5. Does pressing the registered hotkey while a Win32 menu is open reach
   jumpkey, and leave the menu open?

Also related: **z-order between topmost windows.** Menus are topmost too.
A submenu created *after* the overlay is shown appears above it. That's
fine in practice, because the overlay is hidden while acting and re-shown
afterwards (re-asserting `HWND_TOPMOST`), but it explains a class of
"labels hidden behind a popup" bugs to watch for.

## 35. Experiments need a home in the config and the architecture

> **Status (2026-11-21):** open. The local measurements suggested here now exist (cancel rate, keystrokes, backspaces, outcome per pick, in telemetry).

"Label only the menu, see if it's good or annoying" is the first of
what will probably be many UX experiments (stable labels, label placement,
chaining, scope defaults). Today, every option costs a config field plus
four or five places in the Settings dialog plus `apply_config` (#17).
That cost discourages experimenting.

Ideas:
- A **`[experimental]` table** in `config.toml`, typed (#16), *not* shown
  in the Settings dialog. Hand-edited, reloaded live (once #8 makes reload
  safe), and documented as "may change or disappear". Graduating an
  experiment means moving it to a real section and adding UI.
- Read these where the decision is made (the pick flow / menu mode state
  machine), not threaded through `Runtime` by hand.
- `menu_mode.auto_reshow` (the owner's off switch) is a **real** setting
  from day one. `menu_mode.label_only_menu` starts in `[experimental]`.

**How will "good or annoying" be judged?** By feel alone is fine, but
cheap local measurements would help a lot. Candidates, all already
visible to the pick flow:
- **cancel rate** (Esc without a pick)
- **backspaces per pick** and "wrong letter reset the prefix" count
- **time from overlay shown to the completing key**
- **re-pick within N seconds** of a pick (often means "picked the wrong
  thing")

Kept as counters in the local log (or a small local stats file), never
transmitted, these let the owner compare an experiment on vs off over a
week of real use. **Question:** is that kind of local-only, opt-in
measurement something you'd want, given the privacy policy's "no
tracking" stance? It would need careful wording in `PRIVACY_POLICY.md`
even though nothing leaves the machine.

## 36. The logs already contain content the privacy policy says they don't

> **Status (2026-11-21):** open. The window title is still logged at `info` (`src/main.rs:853`) and at `warn` on search failures (`src/windows/mod.rs:476, 559`). The telemetry records follow the rule by construction (string-field whitelist, `docs/TELEMETRY.md`).

Found while checking the above: `PRIVACY_POLICY.md` says the log has "no
personal or sensitive information", but:
- `main.rs:853` logs the **chosen window title** at `info` level on every
  window pick (titles routinely include email subjects, document names,
  URLs, chat names).
- `mod.rs:476, 559` log window titles at `warn` level when a search
  fails.
- `debug`/`trace` levels log titles and element names more widely (only
  when the user turns on debug logging).

Release builds write `info` and above to `%LOCALAPPDATA%\jumpkey\` by
default. Not transmitted, but it contradicts the written policy, and users
attach logs to bug reports. A simple rule worth adopting before adding
any pick metrics (#35) or the timing log (#23): **`info`/`warn` never
contain titles, names or URLs**, only ids, roles, counts and timings.
Content-bearing fields only at `debug`, off by default.

## 37. Per-monitor overlays: design notes

> **Status (2026-11-21):** open (design). Scroll mode's indicator window (#54) is a small first per-monitor, no-activate layered window.

Approved as the direction. Things to decide when designing it:

- **Hint → monitor assignment:** by the target's center (`MonitorFromRect`
  nearest). Targets that straddle two monitors get their badge on the
  monitor holding most of the target; callout tails stay inside one
  monitor (already enforced by `rect_inside_monitor`).
- **Per-monitor style:** each overlay window gets its monitor's DPI and
  scales the DIP-based style (owner decision: per-monitor DIPs, #32c).
  Text layer already supports mixed sizes; layout and collision run per
  monitor, which also makes them smaller problems.
- **Lifetime:** create windows lazily per monitor, cache them like today's
  single persistent overlay, and rebuild on `WM_DISPLAYCHANGE` /
  `WM_DPICHANGED` (no handling of either exists today; geometry is only
  rechecked at the start of each pick).
- **Input:** with the no-activate + keyboard hook model (#26/#34), no
  overlay window needs focus, so N windows don't make input harder. With
  the *current* focus model, one window would have to own focus while the
  others are passive, which is another reason to settle #26 first.
- **Present cost:** each window clears and presents only its own monitor.
  Monitors without hints are hidden. Menu mode on one monitor only touches
  that monitor's window.
- **Window picker:** shares the same per-monitor windows, and its badges
  pick up the right per-monitor size for free (#32d).

**Question:** should a monitor with no hints get a subtle dim or outline
while the overlay is active (so the user knows jumpkey is "on"), or stay
completely untouched?

---

# Owner decisions, round 3 (2026-11-20)

| Topic | Decision |
|---|---|
| Menu mode navigation | **Backspace with an empty prefix = up one menu level.** (Esc presumably leaves menu mode entirely; confirm.) |
| Max wait for a submenu popup | Undecided. |
| Telemetry | Add temporary telemetry while the owner is the only user. The privacy policy may be pulled for now. **Remove, or make opt-in, before general release.** |
| Dim monitors with no labels | Roadmap, low priority (avoid feature creep). |
| Frameworks | Chromium/Electron definitely. **Firefox eventually.** |

---

# Seventh pass: UI frameworks, browsers, telemetry design

## 38. Background: the UI frameworks jumpkey has to read

> **Status (2026-11-21):** background.

Each framework builds its accessibility tree differently, so "what gets
labelled" (#25, #30) and "how menus work" (#33) differ per framework.

| Framework | What it is | Where you meet it | Accessibility shape |
|---|---|---|---|
| **Win32** (USER32, plus MFC / WinForms on top) | The original Windows UI API: every control is its own window (HWND), drawn by the OS | Classic Control Panel, older apps, dialogs, **classic context menus** (`#32768`, e.g. Win11 "Show more options") | Mature, predictable: real control types, Invoke / Toggle / legacy patterns work |
| **WPF** | .NET UI framework from 2006, one HWND, framework draws everything | Visual Studio, many business apps | Good UIA support, deep trees |
| **WinUI / XAML** (UWP, WinUI 2/3, Windows App SDK) | Microsoft's modern UI framework, one HWND, framework draws everything | Settings, Calculator, new Notepad, Terminal, Photos, **Win11 Explorer's command bar and new context menu** | UIA comes from the framework; flyouts and menus are popup windows; heavier use of SelectionItem / ExpandCollapse |
| **Chromium** (Chrome, Edge, Brave… and **Electron**: VS Code, Slack, Discord, Obsidian, Notion; **WebView2**: new Outlook, new Teams, Widgets) | Web engine, the page is a DOM | Browsers and many desktop apps | Accessibility built **on demand**; DOM depth; ARIA roles mapped to UIA |
| **Gecko** (Firefox, Thunderbird) | Mozilla's web engine | Firefox | Historically IAccessible2 / MSAA, with native UIA arriving more recently; activation differs from Chromium |
| **Office** | Own UI toolkit (command bars, ribbon) | Word, Excel, Outlook classic | Custom; has its own keyboard system (KeyTips) |

So "Win32 vs WinUI": Win32 is the classic, one-window-per-control API whose
menus are the old `#32768` style; WinUI is the modern XAML framework whose
menus are flyouts. On Windows 11 the owner meets both every day. Explorer's
new right-click menu is XAML, and "Show more options" opens the classic Win32
one, which makes Explorer a good single test case for #34.

## 39. Browser detection by exe name doesn't match how apps are built

> **Status (2026-11-21):** open. `is_browser_window` is now `src/windows/mod.rs:1993`. VS Code (desktop path) measured 70 ms p50; label coverage there is still unchecked.

`is_browser_window` (`mod.rs:1993`) decides "browser rules or desktop
rules" for a **whole window** by matching the **exe name** against a fixed
list (chrome, msedge, firefox, brave, opera, vivaldi, iexplore, arc).

Consequences:

- **Electron apps are treated as desktop apps.** VS Code, Slack, Discord,
  Obsidian, Notion are Chromium inside, with the same on-demand
  accessibility and the same `Chrome_RenderWidgetHostHWND` renderer child
  windows. Yet they get the desktop path: no `WM_GETOBJECT` activation
  probe, depth 12 instead of 32, desktop filtering and the server-side
  `FindAll` condition. **Hypothesis to check:** in VS Code or Slack, are
  content elements (file tree rows, message actions, buttons inside the
  web view) labelled, or only the outer frame?
- **WebView2 apps mix both worlds in one window.** New Outlook or new Teams
  have native chrome *and* Chromium content. A per-window yes/no can't
  describe that: one half always gets the wrong rules.
- **Firefox is in the list but still "only surfaces chrome"** (README
  roadmap). So Firefox gets browser rules, but the **activation** step
  (written for Chromium; the comment says the `OBJID_CLIENT` probe is a
  "belt-and-braces" guess for Firefox) evidently doesn't wake up Gecko's
  content tree, or Gecko's tree arrives in a shape the web filter
  doesn't match. Which one is true decides the fix, and the
  `enumerate_foreground` example on Firefox would answer it quickly.
- Every search calls `OpenProcess` + `QueryFullProcessImageNameW`. That's
  cheap, but it's redone per search instead of cached per process id.

**Idea: decide rules per subtree by framework, not per window by exe.**
UIA exposes a **`FrameworkId`** property on every element (`"Win32"`,
`"WPF"`, `"XAML"`, `"WinForm"`, `"DirectUI"`, `"Chrome"`, `"Gecko"`, …).
Prefetched in the cache request, it would let the walker:
- switch to web filtering rules when it enters a `Chrome` or `Gecko`
  subtree (a WebView2 inside a XAML app included)
- run activation for any Chromium engine (window class
  `Chrome_WidgetWin_*` / `Chrome_RenderWidgetHostHWND`), Electron included
- use Gecko-specific activation and depth for Firefox
- record the framework of each element, which feeds the fixture corpus
  (#30) and lets #25 and #27 have per-framework rules where needed

This turns "add Firefox" and "support Electron properly" from special cases
into entries in a per-framework table (activation strategy, depth limit,
filter rules, menu discovery). **Question:** are there Electron or WebView2
apps you use daily where labels have felt sparse? That would confirm or
rule out the first bullet quickly.

## 40. Telemetry: designing it so it's easy to remove

> **Status (2026-11-21):** **done, with a different shape** (PR #26): a `telemetry` cargo feature, off by default, implemented as `tracing` spans plus a custom layer instead of a typed `Event` enum. Content is kept out by a whitelist of string fields. Records go to local JSON lines under `%LOCALAPPDATA%\jumpkey\telemetry\`; nothing is sent anywhere.

The plan (temporary telemetry, removed or opt-in before release) is
reasonable. The architecture can make "remove" a build setting rather
than a cleanup project.

### Suggested shape

- **A compile-time cargo feature, `telemetry`, off by default.** The
  owner's builds turn it on; the release workflow, the MSI and
  `cargo install` don't. Release binaries then **cannot** contain it. No
  runtime switch to get wrong, and the published source can't
  surprise someone who builds it.
- **One module, typed events.** A single `telemetry::record(Event)`
  entry point with an `enum Event` (`PickStarted`, `ElementsReady { ms,
  count, cache: Hit|Miss|Warmed, framework }`, `OverlayShown { ms }`,
  `PickCompleted { ms, keystrokes, backspaces, resets, outcome }`,
  `PickCancelled { ms }`, `MenuLevelEntered { depth }`, …).
  - The type system keeps content out: no `String` fields for titles,
    names or URLs. This fixes #36's problem by construction.
  - With the feature off, `record` compiles to nothing.
- **Local sink first.** JSON-lines files under
  `%LOCALAPPDATA%\jumpkey\telemetry\`, rotated daily. For one user, a
  small script (Python, DuckDB, or a spreadsheet) over those files answers
  "is label-only-menu annoying?" and "what's my p95 hotkey → overlay
  time?" without any server. **Question:** do you intend telemetry to stay
  on your machine, or to be sent somewhere (for example to aggregate
  across machines)? Sending changes the design a lot (endpoint, auth,
  batching, failure handling) and is much harder to "just remove".
- **Shares measurement points with the timing log (#23).** The same spans
  feed both. Telemetry is just a second, structured sink.

### Things to think through

- **The privacy policy while the repo is public.** If public builds never
  include the feature, the current policy stays true for every
  distributed binary, and it may not need to be pulled at all. Pulling it
  matters more if there are Store or crates.io listings that reference it.
  **Question:** is anything currently published (Store listing, crates.io
  crate, GitHub release binaries of jumpkey)? The repo has keyhop-era tags
  (v0.2–v0.5) but no GitHub releases.
- **"Remove logging before release"** is probably the wrong goal:
  logs are what make user bug reports useful. A better end state might
  be: logging stays, release default is `warn`, and `info`/`warn` never
  carry content (#36). Telemetry is what goes away.
- **Experiments + telemetry** (#35): each event should record which
  experiment flags were on, so on/off comparisons are possible later.

---

# Owner decisions, round 4 (2026-11-20)

| Topic | Decision |
|---|---|
| Telemetry destination | **Local only** for now. Forwarding to the owner's server may come later, possibly as a separate job. |
| Published artifacts | **Jumpkey has no releases.** Only keyhop artifacts exist, and they aren't the owner's. |
| Daily apps | **Vivaldi, VS Code, Windows Terminal.** |
| Esc | **Fully exits** jumpkey selection, menu mode included. |

### What "no releases" means for the architecture

There are no users, configs or library consumers to break, so this is the
cheapest moment for every breaking change in this document:
- narrowing the public library API (#18)
- changing the config format: typed values (#16), font size in DIPs
  (#32c), `[experimental]` (#35), renaming or dropping `hints.alphabet`
- changing what `ElementId`/`perform` look like (#22 `Snapshot`, #33
  `Outcome`)

After the first release each of these needs migrations or deprecation. A
reasonable rule: **make the format and API changes before v0.6.0 ships**,
even if the features behind them come later.

### Telemetry forwarding, later

If forwarding is added, keeping it **outside the jumpkey process** (a
separate script or scheduled job that ships the local JSON-lines files)
means the app itself never needs network code. "jumpkey does not connect
to the internet" then stays true for the app forever, and the forwarder
can be removed or replaced independently.

---

# Eighth pass: the owner's daily apps

Vivaldi, VS Code and Windows Terminal happen to cover three different
engines: **a Chromium browser whose own UI is a web page**, **Electron**,
and **WinUI/XAML**. Together they're an ideal first fixture corpus (#30).

## 41. Vivaldi: the browser's own UI is web content

> **Status (2026-11-22):** **walk cost fixed** (roadmap step 2, PR 3). Browser walks now fetch the whole control-view tree in one UIA call (`fetch_browser_tree`, `TreeScope::Subtree` + control-view filter) and walk it in memory (`walk_cached`), falling back to the hop-by-hop walk if the fetch fails. p50 switch-then-press:2000, main → branch: dense-list 721 → 165, long-feed 1,296 → 243, edge-cases 608 → 133, form 90 → 53, nested-menu 86 → 48, tabs 87 → 46 ms; target CPU ~4× lower; jumpkey private bytes +5–10 MB during a big walk. Labels: nothing missing on any fixture. **Known quirk:** in the one-call fetch Vivaldi sometimes reports 4 disabled browser-UI buttons (back/forward on a fresh tab, one panel and one status-bar button) as `IsEnabled`, so they get labels (16/22 presses on small pages, ~4/22 on big ones); the hop walk reads them as disabled. Spike alternative (pruning off-viewport subtrees, branch `spike/browser-walk`): 163 ms on long-feed but lost a fixed popover whose parent was off-screen. Still open: cap-before-sort (no fixture hits the 800 cap now), and the real-site check of both variants. Before: browser pages took 0.7–1.3 s per press and scaled with page size (form 89 ms → dense-list 748 → long-feed 1,272 p50), ~98 % in the tree search. The "sparse first press" guess turned out worse: see #53.

Vivaldi builds its tab bar, panels, address bar and status bar out of web
technology inside Chromium. So the *browser chrome* is a DOM too, and
everything goes through the browser path (`vivaldi.exe` is in the list,
so it gets browser rules, activation probes and depth 32).

Things that follow, all to check on the owner's machine:
- **Tabs vs close buttons (#25)** probably hits Vivaldi especially hard:
  every tab in the tab bar contains a close button, and tab stacks add
  another level of nesting. This may well be the behaviour the owner has
  noticed.
- **Cost grows with the whole DOM, not just what's visible.** The browser
  path walks node by node (`walk_recurse`) and only filters
  `IsOffscreen` when **recording**. It still **visits** every off-screen
  subtree (long feeds, long diffs, panels collapsed off-screen). On
  heavy pages, the time is spent on nodes that can never get a label.
  Skipping subtrees whose bounding rectangle lies entirely outside the
  window (with care for overflow and sticky content) could cut that a lot.
  The timing log (#23) with `visited` vs `collected` counts (already in
  `WalkStats`) would show how much.
- **The element cap applies in DOM order, before center-out sorting.**
  `MAX_ELEMENTS_BROWSER = 800` (and 500 for desktop) stops collecting in
  walk order, which is roughly top-left first. Sorting by distance from
  the anchor happens **after** the cap. On a dense page (mail list, file
  list, spreadsheet), elements near the center or bottom can be cut while
  header and sidebar elements keep their places. "Shortest labels nearest
  the center" quietly breaks exactly when there are the most targets.
- **The first pick on a fresh tab or window may be sparse.** Chromium builds
  its accessibility tree asynchronously after being asked. Jumpkey
  sends the activation probes and **walks immediately**. If the tree isn't
  built yet, the first search sees a partial tree, which is then cached.
  **Question:** have you seen "first press shows few labels, second press
  shows many" in Vivaldi?
- **Menus:** Vivaldi's menus and context menus may be drawn by Vivaldi's
  own UI or as Chromium popup windows, depending on settings. That's a
  good #34 test case either way.

## 42. VS Code: Electron gets desktop rules, and menus can be in-window

> **Status (2026-11-21):** open. 70 ms p50 in the baseline.

- **Not in the browser list**, so it gets the desktop path: one
  server-side `FindAll(Subtree)` with the interactable-type condition, no
  activation probe, desktop filters. The `FindAll` has no depth limit, so
  depth isn't the problem. The risks are **activation** (Chromium content
  not built yet) and **filtering** (web content judged by desktop rules,
  for example `Group`/`Custom` elements with ARIA roles).
- **In-window menus.** With VS Code's custom title bar (the default on
  Windows), the menu bar and its dropdowns are **DOM elements inside the
  main window**, not popup windows. The "window stack" discovery in #31
  wouldn't find them as separate windows. Menu mode needs a second
  discovery path: a `ControlType::Menu` / ARIA `menu` **subtree inside the
  window**. The "label only the menu" experiment then means "label only
  inside the open menu element", whether that's a popup window or an
  in-window subtree. **Both shapes need to be in the design from the
  start.**
- **Virtualized lists.** The explorer tree, search results and the
  problems list only create accessibility nodes for visible rows. That's
  good for speed, but scrolling means new nodes, which ties into
  freshness (#21).

## 43. Windows Terminal: XAML tabs plus a large text surface

> **Status (2026-11-21):** open. 37 ms p50 in the baseline, the only app meeting the 50 ms goal.

- Tabs, the new-tab dropdown and the Settings UI are WinUI/XAML: desktop
  path. Tabs contain close buttons, so **#25 again**. The dropdown is
  a XAML flyout (popup window), so it's a #34 test case too.
- **Terminal panes may be filtered out for being too big.** Pane and
  text surfaces are large elements. `MAX_HINT_PX = 1500` drops anything
  wider or taller than 1500 **physical** pixels. A maximized Terminal on
  the owner's 4K monitor (3840 px wide) has panes over that limit, so
  "jump to this split pane" can't work there, while the same layout on
  the 1080p monitor might.
- **Idea (roadmap, not now):** URLs and file paths inside terminal text
  aren't UIA elements, but the text is readable through UIA's
  `TextPattern`. A "text hints" mode (label URLs and paths inside a text
  surface) is a known pattern in terminal tools. Out of scope for now;
  noted because it would change the `Element` model (targets that are
  text ranges, not controls).

## 44. All pixel thresholds are physical pixels, so filtering differs per monitor

> **Status (2026-11-21):** open.

Found via #43, but more general: every size rule in the filters uses
**physical** pixels: `MIN_HINT_PX = 12`, `MAX_HINT_PX = 1500`,
`MIN_WEB_FOCUSABLE_SIZE = 10`, `MIN_WEB_ELEMENT_SIZE = 16`, and the 10 px
pattern-fallback floor. On the owner's 4K monitor at (say) 200 % scaling:

| Rule | Primary (100 %) | 4K (200 %) in DIPs |
|---|---|---|
| min hint size 12 px | 12 DIP | **6 DIP** (lets more tiny noise through) |
| max hint size 1500 px | 1500 DIP | **750 DIP** (drops large real targets: panes, big buttons) |
| web min 16 px | 16 DIP | **8 DIP** |

So the **same app shows different labels depending on which monitor it's
on.** Thresholds should be defined in DIPs and converted with each
element's monitor DPI. This is another reason the fixture corpus should
record the DPI with each snapshot. It belongs with the per-monitor work
(#32/#37), since it's the same "one scale for everything" assumption in
the filter layer.

## Questions

1. **Vivaldi first press:** have you noticed the first press on a new
   tab or window showing fewer labels than the second?
2. **Vivaldi tabs:** when you press the hotkey over the tab bar, do the
   tabs get labels, or only their close buttons? (Directly tests #25.)
3. **VS Code:** do labels reach inside the editor area (tabs, the explorer
   tree, buttons in panels), or mostly the outer frame?
4. **Terminal:** do you use split panes, and would "jump to pane" be
   useful? On the 4K monitor it's likely blocked by `MAX_HINT_PX`.
5. **Dense pages:** in Vivaldi on a long, dense page, do the labels near
   the center or bottom ever seem missing? (Tests the cap-before-sort
   issue.)

---

# Ninth pass: how picking feels, from typing to clicks

## 45. Labels are placed in reading order, not by importance

> **Status (2026-11-21):** open. Owner decision (round 5): distant badge with a leader line when there's no room.

`lay_out` (`overlay.rs:1330`) places badges **top-to-bottom, left-to-right**,
and each badge takes the first free spot from its candidate list (callout
below or above, then `OutsideTop`, `TopLeft`, `TopRight`, `BottomRight`,
`OutsideBottom`, then stacking downward).

But labels are **assigned** by importance: the shortest labels go to the
elements nearest the anchor (window center or cursor), which are also the
likeliest targets. Placement ignores that. On a crowded screen, top-left
elements take the good spots first, and the element nearest the center,
which holds a one-key label, is the one most likely to be pushed to a
fallback position away from its target.

Fallback behaviour adds to it:
- Phase 2 stacks a badge **downward** up to 256 rows until it finds a
  gap, so in dense toolbars badges can form a column far from their
  targets.
- Phase 3 then **clamps** the badge into its monitor **without
  re-checking collisions**, so a clamped badge can sit on top of another
  badge.

**Questions:**
- Should placement go in **label priority order** (shortest / nearest the
  anchor first), so the likeliest targets always get the best spots? The
  cost is that stacking becomes less visually regular.
- When there's no free spot nearby, which is better: a badge far away
  with a leader line, a **smaller** badge, or allowing overlap with the
  more important label on top?

## 46. Synthesized clicks: center point, and the cursor isn't restored

> **Status (2026-11-21):** open. `click_at` is now `src/windows/mod.rs:1261`. Scroll mode (#54) needs the same teleport-and-restore helper.

`click_at` (`mod.rs:1261`, used for right-click and double-click, and
`el.click()` as the last fallback for invoke) moves the cursor to the
element's **center** with `SetCursorPos` and leaves it there.

- **Center isn't always a safe point.** A row's center may land on an
  inner button. A text field's center puts the caret mid-text. An element
  partly covered by something else may have its center covered. UIA
  offers `GetClickablePoint`, which apps can provide to say "click
  here", and it accounts for some of those cases.
- **The cursor stays where the click happened.** After a double-click
  on a file, the mouse is sitting on that file. That can trigger hover
  effects: tooltips, hover cards, hover-to-open menus, row highlights.
  **This matters for menu mode:** Win32 menus highlight whatever item is
  under the cursor, so a cursor left over a menu can change the
  highlighted item between levels, or reopen a submenu on hover.
- On the plus side, `click_at` reads the element's bounds **live** (not
  from the cache), so it clicks where the element is now, though nothing
  compares that with where the label was (#22 pre-action check).

**Question:** should jumpkey **put the cursor back** after a synthesized
click (except right-click, where the context menu appears at the cursor,
and arguably the cursor belongs with it)?

## 47. Pressing the hotkey while the overlay is open queues another pick

> **Status (2026-11-21):** open.

Very likely one of the "fire all at once" behaviours the owner has seen
(#15):
1. The overlay is open (its own nested message loop).
2. The user presses the hotkey again, expecting it to close the overlay
   (a common toggle habit) or because nothing seemed to happen.
3. `RegisterHotKey` still posts `WM_HOTKEY`. The nested loop dispatches
   it to `global-hotkey`'s hidden window, which **queues** an action on its
   channel.
4. The user presses Esc. The overlay closes, the main loop resumes,
   `poll_actions()` finds the queued action, and **a new overlay opens
   immediately.**

The overlay never sees the hotkey, because `key_down` gets only the
non-modifier key and the hotkey itself is consumed by `RegisterHotKey`.
Nothing treats "hotkey pressed during a pick" as meaningful.

**Design question:** what should the hotkey do while the overlay is open?
- **Toggle:** close the overlay (the most common expectation).
- **Cycle:** switch scope (window → monitor → all windows) or switch to
  the window picker.
- **Refresh:** search again (useful if the labels look stale).

Whatever the choice, the event loop needs to know "a pick is in progress"
and route the hotkey to it instead of queuing a fresh pick. That's the
state-machine point from #33 again.

## 48. Every pick triggers a background re-search of the same window

> **Status (2026-11-21):** open.

When the overlay hides, Windows activates the target app again, which
fires `EVENT_SYSTEM_FOREGROUND`. The warming hook skips **jumpkey's own**
windows (`WINEVENT_SKIPOWNPROCESS`), but this event is for the **target
app**, so it's forwarded. The warming thread then starts a **full UIA
search of the window the user just acted on**:

- It runs **at the same moment** as `perform` acts on the same app:
  invoke, click and the warming search compete for the target's UI
  thread, which can slow down the action the user is waiting for.
- It captures the app **mid-transition** (the button was just pressed,
  the page is navigating, a menu is opening) and caches that state.
- **For menu mode, this is a correctness issue:** after expanding a
  submenu, the warming thread searches the app window (not the new
  popup) and caches the pre-popup state. A quick next hotkey press could
  be served from that.

With the current focus model the overlay activates and then gives back
focus on every pick, so this always happens. With a no-activate overlay
(#26) the target never loses foreground, and this disappears by itself.
Another reason to settle #26 first.

## 49. A 2.5-second splash on every start, with hotkeys queued behind it

> **Status (2026-11-21):** **changed.** Since PR #24 the splash loop dispatches hotkeys and the main loop drains them right after the splash, so presses are no longer lost; they still fire after the splash ends. Separately, startup sometimes stalls 3–5 s before the splash appears (#52).

`run_windows` keeps the splash visible for at least 2.5 s (`MIN_SPLASH_MS`)
**on every start**, including **launch at login**. During that time,
hotkeys are already registered but the main loop hasn't started polling,
so presses during the splash queue up and fire afterwards (#15 again).

**Questions:** Is the splash worth keeping for launch-at-login starts, as
opposed to only when started by hand? Should it be skippable (any key or
click), or replaced by a short tray balloon ("jumpkey is running")?

## 50. Keyboard input edge cases

> **Status (2026-11-21):** open. A keyboard hook (#26, #54) changes most of these.

- The overlay handles only `WM_KEYDOWN`. `WM_SYSKEYDOWN` (Alt held) is
  ignored, which is fine.
- The scancode is masked to 8 bits (`>> 16 & 0xFF`), dropping the
  **extended-key** bit. Keys that differ only by that bit (numpad vs
  navigation cluster) can't be told apart. Rarely matters for alphabet
  keys.
- **IME:** with an East-Asian input method active, the overlay window can
  receive `VK_PROCESSKEY` instead of the real key, so labels may be
  untypeable. Not a current-user concern, but the low-level keyboard hook
  design (#26) reads keys **before** the IME and would fix this by
  construction.
- `key_chars` ignores Shift by translating with an empty key state,
  which is what makes Shift+letter (right-click) work. Good.

## The pattern behind passes 4–9

Most "feels wrong" issues found so far trace back to **two structural
choices**:
1. **The overlay takes focus** (#26). Causes: menus closing (#26),
   re-search after every pick (#48), IME problems (#50), only one window
   able to own input (#37).
2. **Picks are a nested loop, not a state in one event loop** (#5, #15).
   Causes: queued hotkeys firing afterwards (#15, #47, #49), no way for
   the hotkey to mean "toggle/cycle" (#47), menu mode having no natural
   home (#33).

Changing both is a sizeable restructure, but it removes whole classes of
bugs instead of patching them one at a time. That's worth weighing
against doing smaller fixes first.

---

# Owner decisions, round 5 (2026-11-21)

| Topic | Decision |
|---|---|
| Event model | Agrees a **state machine** (picks as states in one event loop) is probably the way. |
| Hotkey while overlay open | Undecided. |
| No room near a target | **Distant badge with a leader line.** |
| Cursor after synthesized clicks | Probably **restore** it. And **teleport, never animate**: no watching the mouse fly to the target. |
| Splash screen | Owner plans to **replace** it. |

## 51. Where the "flying mouse" comes from: the `uiautomation` crate's `click()`

> **Status (2026-11-21):** open. The fallback is now `src/windows/mod.rs:1370`; telemetry's `invoke_path` will show how often it fires.

jumpkey's own `click_at` (right-click, double-click) **teleports** with
`SetCursorPos`. The flying cursor comes from the other click path:
`UIElement::click()` from the `uiautomation` crate, which jumpkey calls
- as the **last fallback in `invoke_smart`** (`InvokePath::MouseFallback`,
  `mod.rs:1370`), whenever no UIA pattern worked, and
- for `Action::Click`.

In `uiautomation` 0.24.4 (read from docs.rs), `UIElement::click()` uses
`Mouse::default()`, whose `move_time` is **500 ms**. `Mouse::move_to`
**moves the cursor in ~20 px steps along a straight line, sleeping
between steps**, so a cross-screen fallback click takes about half a second
of visible gliding (moves under 40 px jump at once). There may be more
fixed waits around the click itself (`interval: 100` ms default; a
`max(200, interval)` sleep in the double-click path); those parts were only
partly readable and should be checked in the crate source.

Consequences:
- **Speed:** every fallback click adds roughly **0.5 s or more** of
  blocking, on the main thread (#9), during which jumpkey can't react.
  This happens in exactly the cases where UIA patterns failed, which are
  usually the custom web and Electron controls the owner uses a lot.
- **Feel:** the visible glide is the behaviour the owner wants gone.
- **Inconsistency:** the crate's click prefers `GetClickablePoint` and
  falls back to the center; `click_at` always uses the center (#46). Two
  click paths, two point rules, two movement styles.

**Direction implied by the owner's decision:** one jumpkey-owned click
routine used by every synthesized click (left, right, double, fallback).
It would teleport, pick the point consistently (clickable point, then
center), optionally restore the cursor afterwards, and report its outcome
(#33's `Outcome`). Not depending on the crate's `Mouse` defaults also
removes a hidden timing dependency that could change when the crate is
upgraded. The timing log (#23) recording `InvokePath` per pick would show
how often the fallback (and its glide) currently happens.

**Question:** when the fallback click fires today, do you also notice a
**pause** before the click lands, not just the glide? That would confirm
the extra fixed waits.

---

# Tenth pass: after the measurements (2026-11-21)

## 52. Startup sometimes stalls 3–5 s before the splash appears

Seen by pick-bench: in 4 of 5 runs at least one jumpkey start took
3–5 s before its splash window appeared, always while a browser had just
been launched. A normal start (spawn → splash closed) takes ~2.9 s, most
of it the splash's 2.5 s minimum. Something that runs **before**
`SplashScreen::show()` (`src/main.rs:197`) waits on other processes.
Candidates to check: the single-instance guard, COM/UIA initialisation,
or anything that touches other windows (a broadcast `SendMessage` blocks
on busy windows).

**Not investigated.** A `startup` span in the telemetry build, or plain
timestamps around each startup phase, would find it quickly. It matters
most for launch at login, when every other app is starting too.

## 53. The first press on a freshly opened Chromium page can show nothing

> **Status (2026-11-21):** **done** (roadmap step 2, PR 2). `walk_window` retries a browser walk that `WalkStats::looks_unbuilt` (nothing collected; an on-screen page `Document` with no children; or no `Document` at all in a window with a `Chrome_RenderWidgetHostHWND` child; browser dialogs have neither and aren't retried) after 150 ms, up to twice. A window still unbuilt after its retries is skipped for 30 s (a static map shared by the hotkey and warming backends). Empty results are never cached. Measured: fresh-page first presses showed nothing in 3/24 runs on main, 0/18 on the branch (3 retries, each finding the full page); hotkey walks on built pages never retried (0/108); no p50 change beyond noise on any Vivaldi scenario. Telemetry: `attempts`, `documents`, `empty_documents` on the `walk` span.

Worse than #41's guess of a "sparse first press": on a just-opened Vivaldi
page jumpkey sometimes finds **no** elements, logs "no interactable
elements" and shows no overlay at all, so the user sees nothing happen.
Reproduced in three pick-bench runs; a press a few seconds later works.
pick-bench now retries the first press and records `first_press_attempts`
so it stays visible in the data.

Likely cause: Chromium builds its accessibility tree only after being
asked, the activation probe waits 50 ms, and the walk runs immediately
on whatever exists. **Fix direction:** when a browser walk finds nothing
(or very little) right after activation, wait briefly and walk once more
before giving up; and never cache an empty result. Small, user-visible,
and testable with pick-bench's first-press data.

## 54. Scroll mode (Homerow-style): design and where it fits

The owner wants keyboard scrolling like Homerow's: a hotkey enters
scroll mode, `h j k l` scroll the active scroll area, Shift scrolls
faster, Tab or a digit switches to another area, Esc exits. (Vimac,
Homerow's predecessor, outlines the active area.) Today jumpkey can't do
this: `Action::Scroll` exists (`src/action.rs:34`, ScrollPattern at
`src/windows/mod.rs:1394`) but nothing triggers it, and scroll
containers are filtered out of every search (both the desktop search
condition and the web filter require a clickable control type or
pattern).

**Status: designed, not implemented.** The owner wants to settle where it
goes in the roadmap first.

### Owner decisions

| Topic | Decision |
|---|---|
| Input | **No-activate indicator + low-level keyboard hook**: the target app keeps focus |
| Areas | **Several areas**, as in Homerow; start on the area under the cursor, else the largest |
| Feel | **Smooth while a key is held** (velocity, stops on key-up) |
| Hotkey | **`Ctrl+Alt+J`** by default (same family as the window picker's `Ctrl+Alt+Space`) |
| Pointer | Moved into the active area while scrolling (wheel input goes to what's under the pointer), **restored on exit** |

### Keys

Bound to **physical key positions** (`src/keyboard.rs`), so they stay on
the home row in Dvorak and Colemak, like Homerow.

| Key | Effect |
|---|---|
| `h` `j` `k` `l` (held) | smooth scroll left / down / up / right; keys held together combine |
| Shift + `hjkl` | fast (× `fast_multiplier`) |
| `d` / `u` | half the area's height down / up (animated) |
| `g g` / Shift+`G` | top / bottom (ScrollPattern `SetScrollPercent`; a long wheel burst as fallback) |
| Tab, ↓, Ctrl+N / Shift+Tab, ↑, Ctrl+P | next / previous area |
| `1`–`9` | jump to area *n* |
| Esc, or the hotkey again | exit |
| any other key, or a pick hotkey | exit; the key passes through (a pick then runs) |

### Design

- **Pure core, `src/scroll.rs` (cross-platform, unit-tested).**
  `ScrollMode` holds the areas, the active index, the held keys and a
  pending `g`; `on_key(KeyEvent) -> Vec<Effect>` and `on_areas(..)`.
  Effects: `SetVelocity { dx, dy }`, `Jump(Top | Bottom | HalfPageUp |
  HalfPageDown)`, `SelectArea(i)`, `Exit`, `PassThrough`. This is the
  Appendix A pattern (pure transition, effects run by a Win32 shell) on a
  small scale.
- **Keyboard hook** (`src/windows/scroll/hook.rs`): `WH_KEYBOARD_LL`,
  installed on the main thread only while the mode is active. It ignores
  injected events, maps the scancode to a physical key, swallows bound
  keys (and their key-ups) and posts a thread message to the main loop.
  Keys with Ctrl or Alt held pass through (except Ctrl+N/P), so
  registered hotkeys still arrive.
- **Area discovery** (`areas.rs`):
  - *Instant first area:* `ElementFromPoint(cursor)` if the cursor is in
    the foreground window, else the focused element; walk up to the
    nearest ancestor with ScrollPattern that can scroll on some axis;
    else the window's client rect.
  - *All areas, on a worker:* `FindAll(Descendants,
    IsScrollPatternAvailable)` with bounds and the scrollable flags
    cached, filtered to visible, non-tiny areas inside the window. Posted
    back to the main loop; Tab and digits work once they arrive.
- **Scroll engine** (`engine.rs`): a worker thread with its own UIA
  client that does every `SendInput`. About 120 ticks a second while the
  velocity is non-zero, `MOUSEEVENTF_WHEEL` / `HWHEEL` with the fractional
  delta carried between ticks; jumps through ScrollPattern. Selecting an
  area teleports the pointer to its centre (`SetCursorPos`, never
  animated, #51); on exit the pointer goes back unless the user moved
  the mouse meanwhile.
- **Indicator** (`indicator.rs`): a layered, topmost window that takes no
  clicks or focus (`WS_EX_LAYERED | TRANSPARENT | NOACTIVATE | TOPMOST |
  TOOLWINDOW`) over the foreground window. It outlines the active area
  and puts small digit badges on the others, sized for that monitor's
  DPI.
- **Exits:** Esc, the hotkey, an unbound key, a foreground change
  (WinEvent hook for the session), either pick hotkey.
- **Main loop:** a `scroll: Option<ScrollSession>` handled in the
  existing loop, **not a nested loop**, so the hotkey toggles the mode
  instead of queueing (#47).
- **Config:** `hotkeys.scroll_mode = "Ctrl+Alt+J"`; `[scroll]` with
  `speed` (wheel units per second, tuned live) and `fast_multiplier = 3`.
  The hotkey gets a Settings field (eight places today, #17); speed is
  config-file only.
- **Telemetry:** a `scroll` root span (areas found, discovery time, where
  the first area came from, keys, duration, outcome).
- **Bench contract:** the bench config templates need an explicit
  `scroll_mode` chord (`Ctrl+Alt+Shift+F9`), since an empty string is
  reported as a hotkey conflict; pick-bench's chord pre-check includes it.

**Out of scope for v1:** scrolling from inside the element picker, custom
bindings, elevated windows (UIPI blocks the wheel input, #29).

### Risks

- **Does Chromium expose ScrollPattern on web scroll containers?** If
  not, v1 treats a Chromium window as one area (the wheel still works,
  only area switching is lost). The spike answers this first.
- **Old Win32 apps ignore wheel deltas under 120.** Either gather the
  delta into whole notches for those frameworks (by `FrameworkId`, #39)
  or accept stepped scrolling there.
- **Hook timeouts.** Windows silently removes a low-level hook whose
  thread doesn't answer in time, so the main thread must do no UIA work
  while the mode is active. All UIA calls go to workers.
- **Stuck keys.** Track key-ups; clear the velocity on every exit.

### How scroll mode fits the architecture

Scroll mode shares infrastructure with almost every large item in this
document, which is why its place in the order matters:

| Shared piece | Also needed by | What scroll mode does for it |
|---|---|---|
| No-activate window + `WH_KEYBOARD_LL` (#26, #34) | menu picking, IME (#50), per-monitor input (#37) | Proves it in daily use on a low-risk feature: a hook bug means "scrolling didn't work", not "a menu closed" |
| A mode as a state in the main loop, not a nested loop (Appendix A, #15, #47) | the state machine | `ScrollMode`'s `on_key → effects` is the pattern the state machine generalises |
| UIA off the main thread (#9) | cold searches | Required here (the hook dies otherwise), so the worker-and-post-back pattern gets built early |
| Click-through indicator per monitor DPI (#32, #37) | per-monitor overlays | A small first per-monitor, no-activate layered window |
| Pointer teleport + restore (#46, #51) | the single jumpkey-owned click routine | Same rule, one shared helper |
| Cache freshness (#21) | serve-stale cache | Scroll mode **knows** it scrolled a window: on exit it can drop that window's cache entry and warm a new search, so "scroll, then pick" gets a warm pick. The first event-driven freshness signal |

**Place in the roadmap:** after the correctness fixes and the faster
browser walk, right after the hook spike and before the state machine
(current roadmap, steps 3–4). Building it after the state machine as just
another state would be cleaner, but would delay both the feature and
real-world testing of the hook the state machine depends on.

---

# Owner decisions, round 6 (2026-11-21)

| Topic | Decision |
|---|---|
| Architecture review | Refresh it in place (this pass) and keep one current roadmap |
| Scroll mode | Wanted, Homerow-style (#54). **Design first; build after reviewing where it fits in the roadmap** |
| Scroll mode input | No-activate indicator + low-level keyboard hook |
| Scroll areas | Several, with Tab and digits to switch |
| Scroll feel | Smooth while held |
| Scroll hotkey | `Ctrl+Alt+J` |
| Pointer during scroll mode | Moved into the active area, restored on exit |
| Roadmap order | **Correctness fixes, then the faster browser walk** (the slowest existing feature), then the hook spike, scroll mode, and the state machine |

---

# Appendix A: State machine design sketch (preliminary)

> **Status: first sketch, not a design.** Written at the end of the
> 2026-11-21 review session to gather the owner's decisions in one place.
> A **separate, deeper exploration** of the state machine design is
> planned with another agent. Treat everything here as input to that
> work, not a decision. Open questions are marked **(open)**.

## Why a state machine

Today a pick is a **nested modal loop** (`pick_hint` runs its own
`GetMessageW` loop inside the main one), and so are Settings and every
notification. That's the root of: queued hotkeys firing afterwards (#15,
#47, #49), no meaning for the hotkey during a pick (#47), no home for menu
mode (#33), and searches that can't move off the UI thread (#9). The owner
agreed a state machine is probably the way (round 5).

**Core idea:** one event loop and one `App` state. Every input (hotkey,
key, timer, WinEvent, search finished) is an **event**. A pure
`transition(state, event) -> (state, effects)` decides what happens.
**Effects** (show overlay, start search, perform action, play nothing) are
carried out by a Win32 shell. The transition function has no Win32 in it,
so it can be unit-tested on any OS (#3, #2).

## States

```
                 ┌───────────────────────────────────────────────┐
                 │                                               │
     hotkey      ▼        results ready            key completes │ leaf action done
 Idle ──────▶ Searching ───────────────▶ Picking ─────────────▶ Acting ──────▶ Idle
  ▲  ▲            │                      │  ▲  │                  │
  │  │      Esc / │ timeout/error         │  │  │ Backspace        │ opens submenu
  │  └────────────┘                Esc    │  │  │ (prefix empty,   ▼
  │                                       │  │  │  depth > 0)  AwaitingPopup ──popup ready──▶ Searching (depth+1)
  └───────────────────────────────────────┘  │  ▼                  │
                                             │ (pop level:          │ timeout (open)
                                             │  re-search parent)   ▼
                                             └──────────────── Picking (parent) / Idle (open)
```

| State | Meaning | Data it carries |
|---|---|---|
| `Idle` | Nothing on screen; waiting for a hotkey. | — |
| `Searching { target, depth }` | Finding elements (cache or live search, **off the UI thread**). Overlay may already be visible with no labels or a subtle "searching" hint (open). | pick context (foreground, cursor, monitors), scope, menu stack |
| `Picking { snapshot, labels, typed, depth }` | Labels shown; user typing. | `Snapshot` (owns elements, #22), label assignment, typed prefix, menu stack |
| `Acting { element, action }` | Performing the chosen action (pre-action check first, #22 guard). | chosen element + intended action |
| `AwaitingPopup { parent, deadline }` | Menu mode: a submenu is expected to appear. | parent menu item, deadline (max wait: **open**) |
| `Settings` | Settings dialog open (no longer a nested loop). | dialog state |
| `Scrolling { areas, active }` | Scroll mode (#54): keyboard hook installed, indicator shown, scroll engine running. | areas, active index, held keys, saved pointer position |

Window picker: either a separate `Picking` flavour (`kind: Element |
Window`) or its own states. **(open)**, leaning to one `Picking` with a
`kind`, since rendering and typing are shared.

## Events

| Event | Source |
|---|---|
| `Hotkey(PickElement | PickWindow | OpenSettings)` | `RegisterHotKey` / global-hotkey |
| `Key(char, modifiers)` | keyboard hook (#26) or overlay window |
| `Backspace`, `Esc` | same |
| `SearchDone(Result<Snapshot>)` | search worker |
| `ActionDone(Outcome)` | action executor (#33 `Outcome`) |
| `PopupAppeared(hwnd)` | `EVENT_SYSTEM_MENUPOPUPSTART`, window-stack watch (#31) |
| `ForegroundChanged(hwnd)`, `WindowMoved(hwnd)` | WinEvent hooks (#10) |
| `Scrolled` | low-level mouse hook (today: dismisses overlay) |
| `Timeout(id)` | timers (popup wait, search cap) |
| `ConfigChanged(Config)` | config watcher / Settings save (#8) |
| `Tray(Command)` | tray menu |
| `Hotkey(ScrollMode)` | `RegisterHotKey` (#54) |
| `ScrollKey(key, down/up)` | keyboard hook while `Scrolling` |
| `AreasFound(Vec<Area>)` | scroll-area worker |

## Transitions (decided by the owner where noted)

| From | Event | To / effect | Source |
|---|---|---|---|
| `Idle` | `Hotkey(PickElement)` | `Searching(depth 0)`; if `GUI_INMENUMODE`, start in menu mode | #33.5 |
| `Searching` | `SearchDone(ok)` | `Picking`; render labels (per-monitor overlays, #37) | — |
| `Searching` | `SearchDone(empty / err)` | `Idle`; non-blocking notice (#15) **(open: wording/channel)** | — |
| `Picking` | `Key(c)` completes a label | pre-action check → `Acting` | #22 |
| `Picking` | `Key(c)` extends a prefix | stay; re-render matching labels | — |
| `Picking` | `Key(c)` matches nothing | stay; reset prefix (today's behaviour) **(open: feedback)** | — |
| `Picking` | `Backspace`, prefix non-empty | drop last char | — |
| `Picking` | `Backspace`, prefix empty, depth > 0 | **go up one menu level**: close submenu, `Searching(depth−1)` | owner, round 3 |
| `Picking` | `Backspace`, prefix empty, depth 0 | stay (no-op) **(open)** | — |
| `Picking` | `Esc` | **fully exit** → `Idle` (closes overlay; menus: leave open or close? **open**) | owner, round 4 |
| `Picking` | `Hotkey(same)` | **(open)**: toggle-close / cycle scope / refresh. Must *not* queue a new pick | #47 |
| `Picking` | `Scrolled`, `ForegroundChanged` | `Idle` (today: dismiss) **(open: re-search instead?)** | — |
| `Acting` | `ActionDone(OpenedSubmenu)` and auto re-show **on** | `AwaitingPopup` | owner, round 2 |
| `Acting` | `ActionDone(OpenedSubmenu)` and auto re-show **off** | `Idle` | owner, round 2 (setting) |
| `Acting` | `ActionDone(leaf / other)` | `Idle`; restore cursor if a synthesized click (#46) | owner, round 5 |
| `Acting` | `ActionDone(PreCheckFailed)` | `Searching` (re-search and re-show) **(open: keep typed prefix?)** | #22 |
| `AwaitingPopup` | `PopupAppeared` (+ settled) | `Searching(depth+1)`, scoped to the popup; **only the menu gets labels** (experiment) | owner, round 2 |
| `AwaitingPopup` | `Timeout` | **(open)**: show whatever exists, or `Idle` | owner: max wait undecided |
| any | `ConfigChanged` | apply when back in `Idle` (or immediately if safe) **(open)** | #8 |
| any non-Idle | `Hotkey(OpenSettings)` | **(open)**: ignore, or exit pick then open | — |
| `Idle` | `Hotkey(ScrollMode)` | `Scrolling`; first area from the cursor/focus, full discovery on a worker | #54 |
| `Scrolling` | `ScrollKey` | stay; update velocity, jump or switch area | #54 |
| `Scrolling` | `AreasFound` | stay; update the indicator, keep the active area | #54 |
| `Scrolling` | `Esc`, `Hotkey(ScrollMode)`, unbound key, `ForegroundChanged` | `Idle`; restore the pointer, invalidate the window's cache entry | #54, #21 |
| `Scrolling` | `Hotkey(PickElement)` / `Hotkey(PickWindow)` | `Searching` (exit scroll mode, then pick) | #54 |

Menu-bar items behave like submenu items: picking one opens it and enters
menu mode (owner, round 2, #5).

## Invariants the design should guarantee

1. **No queued picks.** A hotkey during a non-Idle state is handled by
   that state, never deferred to fire later (#15, #47).
2. **The UI thread never blocks on UIA.** Searches and actions run on
   workers; the loop stays responsive (#9).
3. **Never act on a stale target.** Every `Picking → Acting` goes through
   the pre-action check (#22).
4. **Labels never move while the user is typing.** A background refresh
   during `Picking` doesn't relabel (freshness design, #20–22).
5. **Every pick ends in `Idle`** within a bounded time (timeouts on
   search and popup wait), so nothing gets stuck "on".
6. **The target app keeps focus** throughout (no-activate overlay +
   keyboard hook, #26), assuming the #34 prototype confirms it's possible.

## How it fits the other decisions

- **`Snapshot` / `ElementSource` (#3, #22):** `Searching` produces a
  `Snapshot`; `Picking` and `Acting` hold it. IDs from one search can't
  be used with another's elements.
- **`Outcome` from `perform` (#33):** drives `Acting →
  AwaitingPopup / Idle / Searching`.
- **Experiments (#35):** `label_only_menu` is read in the
  `PopupAppeared → Searching` transition; `auto_reshow` (a real setting)
  in `Acting`.
- **Telemetry (#40):** every transition is a natural telemetry event
  (state durations = the latency breakdown from #23).
- **Per-monitor overlays (#37):** rendering is an effect; the state
  machine doesn't know how many windows exist.

## Questions for the deeper exploration

1. Hotkey during `Picking`: toggle, cycle scope, or refresh? (#47)
2. Max wait in `AwaitingPopup`, and what to show on timeout.
3. `Esc` in menu mode: exit jumpkey **and** close the app's menus, or
   leave the menus open?
4. Should `Searching` show the overlay immediately (with a "searching"
   state) or only when labels are ready? How long before showing
   anything (a short delay avoids flicker on fast cache hits)?
5. Re-search on `Scrolled` / content change during `Picking` instead of
   dismissing?
6. Element picker and window picker: one `Picking` with a `kind`, or
   separate states?
7. Where config changes apply: immediately or only at `Idle`?
8. Should Settings be a state of this machine at all, or its own window
   outside the pick flow (it's not modal to picking)?
9. What does the machine look like in **tests**: event scripts with
   expected effect lists (`[Hotkey, SearchDone(fixture), Key('d'),
   ActionDone(OpenedSubmenu), PopupAppeared, …] → [ShowOverlay, …]`)?
   This pairs well with the fixture corpus (#30).
10. Library vs app: does the state machine live in the cross-platform
    library (testable on macOS/Linux CI), with the Win32 shell as a thin
    executor of effects?
