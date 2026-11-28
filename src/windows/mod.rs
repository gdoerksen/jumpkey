//! Windows backend for jumpkey.
//!
//! Implements [`crate::Backend`] against the Windows UI Automation API via
//! the `uiautomation` crate, plus the global hotkey ([`hotkey`]) and
//! transparent overlay ([`overlay`]) building blocks the binary needs.
//!
//! The backend walks the control-view subtree of the foreground window and
//! collects on-screen, interactable elements (buttons, links, inputs, menu
//! items, ...) into the platform-agnostic [`crate::Element`] model.

#[cfg(feature = "telemetry")]
pub(crate) mod bench_labels;
pub mod config_watcher;
pub mod hotkey;
pub mod ipc;
pub mod keyboard_layout;
pub(crate) mod label_text;
pub mod notification;
pub mod overlay;
pub mod prewarm;
pub mod scroll;
pub mod settings_window;
pub mod single_instance;
pub mod splash_screen;
pub mod startup;
pub mod tray;
#[cfg(feature = "telemetry")]
mod walk_dump;
pub mod window_picker;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use parking_lot::Mutex;

use uiautomation::{
    core::UICacheRequest,
    patterns::{
        UIExpandCollapsePattern, UIInvokePattern, UILegacyIAccessiblePattern, UIPatternType,
        UIScrollPattern, UISelectionItemPattern, UITogglePattern,
    },
    types::{ControlType, Handle, ScrollAmount, UIProperty},
    UIAutomation, UIElement, UITreeWalker,
};
use windows::core::PWSTR;
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{MonitorFromPoint, MONITOR_DEFAULTTONEAREST};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEINPUT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumChildWindows, GetClassNameW, GetCursorPos, GetForegroundWindow, GetWindowThreadProcessId,
    SendMessageTimeoutW, SetCursorPos, SMTO_ABORTIFHUNG, WM_GETOBJECT,
};

use crate::cache::{CacheManager, Generation, Lookup, WindowKey};
use crate::config::ScopeMode;
use crate::layout_order::{union_center, HintOrder};
use crate::{tel_current, tel_record, tel_record_current, tel_span};
use crate::{Action, Backend, Bounds, Element, ElementId, Role};

/// `dwExtraInfo` on every input event jumpkey synthesizes (`SendInput`).
/// jumpkey's low-level hooks ignore events carrying it and treat every
/// other event, injected or not, as real: keys remapped by PowerToys or
/// AutoHotkey arrive injected and must still count (spike #34).
pub const JUMPKEY_INPUT_TAG: usize = 0x4A4B_5350; // "JKSP"

/// Maximum tree depth we descend into for *desktop* apps. Native Win32 /
/// WinUI / Electron trees are wide but rarely deep — twelve levels covers
/// every real app surface we've measured (Office ribbons, VS sidebar
/// trees, etc.) without spending time on pathological structural nesting.
const MAX_TREE_DEPTH_DESKTOP: usize = 12;

/// Maximum tree depth we descend into for *browser* foregrounds.
///
/// Browsers map every DOM node to a UIA element, so the path from the
/// browser window to a single `<a>` tag in the rendered page typically
/// looks like:
///
/// ```text
/// Window > NavigationView > Tab > WebView > Document
///   > <body> > <div> > <div> > <header> > <nav> > <ul> > <li> > <a>
/// ```
///
/// Modern web apps (GitHub, Gmail, dashboards, …) routinely nest 15–25
/// elements deep; the desktop limit of 12 only reaches the browser
/// chrome and the page document, never the page content. This is why
/// the picker would highlight the address bar and tabs but nothing on
/// the page itself. The browser path is bounded primarily by
/// [`MAX_ELEMENTS_BROWSER`] so the deeper limit doesn't translate to
/// runaway walks — it just lets us *find* page content before we hit
/// the element cap.
const MAX_TREE_DEPTH_BROWSER: usize = 32;

/// Hard cap on collected elements per enumeration in a regular desktop
/// app. A typical foreground window has well under this many interactable
/// items; the cap is mainly a safety valve.
const MAX_ELEMENTS_DESKTOP: usize = 500;

/// Larger cap for browsers, where the UIA tree mirrors the DOM and a
/// content-rich page (Gmail, GitHub, dashboards) can legitimately expose
/// hundreds of clickable items. The web-specific filter in
/// [`WindowsBackend::try_record_web_element`] is what keeps the
/// signal-to-noise ratio sane at this size.
const MAX_ELEMENTS_BROWSER: usize = 800;

/// Per-window element cap applied to *non-foreground* windows in the
/// multi-window scope modes (`ActiveMonitor`, `AllWindows`).
///
/// Without this, a single chatty background app — Cursor, Slack, a
/// content-rich Outlook window — can return its full
/// [`MAX_ELEMENTS_DESKTOP`]/[`MAX_ELEMENTS_BROWSER`] quota and starve
/// every other visible app of badge slots, leaving the user staring at
/// 300 hints all anchored on one monitor.
///
/// 80 is empirically enough to cover the visible affordances of any
/// reasonable app at a glance, while leaving budget for ~3 other
/// windows on the typical 3-monitor setup. The foreground window is
/// always exempt — the user clearly cares about that one and we want
/// to preserve every clickable on it.
const PER_WINDOW_HINT_CAP_MULTI: usize = 80;

/// Default minimum element size (in physical pixels) before we even
/// consider a web element. Filters out the swarms of 1×1 / 4×4
/// layout/spacer/decoration nodes that the Chromium UIA provider
/// exposes. Elements that explicitly advertise interactivity (an action
/// pattern, a known ARIA role, or `IsKeyboardFocusable=true`) get a
/// looser bar — see [`MIN_WEB_FOCUSABLE_SIZE`].
const MIN_WEB_ELEMENT_SIZE: i32 = 16;

/// Looser size floor for web elements that *prove* they're interactive
/// (focusable / patterned / known role). Many real targets — small
/// "x" close icons in chips, custom icon buttons, dense toolbar items
/// — fall under [`MIN_WEB_ELEMENT_SIZE`] but are absolutely something
/// the user wants to hint. Ten pixels is small enough to catch those
/// without dipping into the spacer-element noise.
const MIN_WEB_FOCUSABLE_SIZE: i32 = 10;

/// MSAA / IAccessible role IDs we treat as "definitely clickable" when
/// the modern UIA control type doesn't make it obvious. These match the
/// `ROLE_SYSTEM_*` constants from `oleacc.h` and are how older / web
/// content surfaces semantic intent through the legacy bridge.
const MSAA_ROLE_MENU_ITEM: i32 = 0x0C;
const MSAA_ROLE_CELL: i32 = 0x1D;
const MSAA_ROLE_LINK: i32 = 0x1E;
const MSAA_ROLE_LIST_ITEM: i32 = 0x22;
const MSAA_ROLE_OUTLINE_ITEM: i32 = 0x24;
const MSAA_ROLE_PAGE_TAB: i32 = 0x25;
const MSAA_ROLE_PUSHBUTTON: i32 = 0x2B;
const MSAA_ROLE_CHECKBUTTON: i32 = 0x2C;
const MSAA_ROLE_RADIOBUTTON: i32 = 0x2D;
const MSAA_ROLE_COMBOBOX: i32 = 0x2E;
const MSAA_ROLE_BUTTON_DROPDOWN: i32 = 0x38;
const MSAA_ROLE_BUTTON_MENU: i32 = 0x39;

/// Bookkeeping for one [`walk_window`] invocation. Surfaced via
/// the debug log at the end of `enumerate_foreground` so we can tell —
/// after the fact — whether the picker stopped because it ran out of
/// elements, hit the depth cap, or simply found everything the tree
/// exposes. Crucial when triaging "jumpkey didn't see X" reports on a
/// specific page or app.
#[derive(Debug, Default)]
struct WalkStats {
    visited: usize,
    max_depth: usize,
    hit_depth_cap: bool,
    hit_element_cap: bool,
    /// Input: the window's rect, for judging whether an empty page
    /// `Document` is on screen. `None` if UIA didn't report one.
    window_rect: Option<Bounds>,
    /// `Document` nodes seen (browser walks). A built Chromium window
    /// has at least one: the page, and in Vivaldi the browser UI too.
    documents: usize,
    /// Page `Document`s with a real on-screen rect but no children: a
    /// tab whose accessibility tree Chromium hasn't built yet (#53).
    empty_documents: usize,
}

impl WalkStats {
    /// Does a browser walk that `collected` this many elements look
    /// like Chromium hadn't built the tree yet (#53)? Nothing found, an
    /// on-screen page `Document` with no children, or, in a window that
    /// hosts web content (a renderer child window), no `Document` at
    /// all. Dialogs host none and never have a `Document`.
    fn looks_unbuilt(&self, collected: usize, hosts_web_content: bool) -> bool {
        collected == 0 || self.empty_documents > 0 || (hosts_web_content && self.documents == 0)
    }
}

/// Wait before each retry of a browser walk that came back empty (#53).
const EMPTY_WALK_RETRY_DELAY: Duration = Duration::from_millis(150);
/// Retries after the first attempt, so at most ~300 ms of waiting.
const EMPTY_WALK_MAX_RETRIES: u32 = 2;
/// How long a window whose retries still found an empty page skips
/// retrying, so a genuinely empty page (a blank tab, a canvas app)
/// pays the wait once, not on every press.
const EMPTY_WALK_SKIP_FOR: Duration = Duration::from_secs(30);

/// Whether a browser walk should be retried, and after how long.
///
/// Chromium builds a tab's accessibility tree only after being asked
/// (`activate_browser_accessibility`), and the walk runs right after
/// asking. On a freshly opened page it can find nothing at all, and
/// the user sees no overlay (#53). So retry while the walk
/// [`WalkStats::looks_unbuilt`], up to [`EMPTY_WALK_MAX_RETRIES`]
/// times, unless this window recently stayed unbuilt through its
/// retries (`recently_empty`).
fn walk_retry_delay(
    retries_done: u32,
    looks_unbuilt: bool,
    recently_empty: bool,
) -> Option<Duration> {
    if looks_unbuilt && !recently_empty && retries_done < EMPTY_WALK_MAX_RETRIES {
        Some(EMPTY_WALK_RETRY_DELAY)
    } else {
        None
    }
}

/// Windows whose retries last found an empty page, and when. Shared by
/// every backend (hotkey and cache warming) because it's about the
/// window, not about whoever walked it.
fn empty_walk_windows() -> &'static Mutex<HashMap<WindowKey, Instant>> {
    static MAP: std::sync::OnceLock<Mutex<HashMap<WindowKey, Instant>>> =
        std::sync::OnceLock::new();
    MAP.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Was `key` empty through its retries less than `skip_for` before
/// `now`? Forgets entries older than that.
fn recently_empty(
    map: &mut HashMap<WindowKey, Instant>,
    key: WindowKey,
    now: Instant,
    skip_for: Duration,
) -> bool {
    map.retain(|_, at| now.saturating_duration_since(*at) < skip_for);
    map.contains_key(&key)
}

/// One element captured by [`walk_window`] before any backend-side
/// bookkeeping runs. Holds everything the public `Element` needs,
/// plus the live `UIElement` so the foreground backend can register
/// it for [`Backend::perform`] later.
///
/// IDs aren't assigned at walk time: parallel pid-group walks (Phase
/// 5) emit records on multiple threads, and we need a consistent
/// monotonic counter across the merged result. The merge step that
/// runs back on the main thread takes care of numbering.
pub(crate) struct WalkRecord {
    pub role: Role,
    pub name: Option<String>,
    pub bounds: Bounds,
    pub element: UIElement,
}

/// Send-shim for [`WalkRecord`] so it can ship across rayon worker
/// boundaries.
///
/// # Safety
///
/// `UIElement` wraps an MTA-only COM interface; the `windows`/`uiautomation`
/// crates conservatively leave it `!Send`. We uphold the COM rule by
/// guaranteeing every thread that ever touches one of these records —
/// the rayon worker that constructs it and the main thread that later
/// invokes patterns on it — has CoInitialized as MTA via
/// `UIAutomation::new()`. Both paths satisfy that: the per-pid worker
/// in `enumerate_windows_filtered` builds its own `UIAutomation` (which
/// registers MTA on the worker thread), and the main thread does the
/// same when [`WindowsBackend::with_shared_cache`] runs at startup.
struct SendWalkRecord(WalkRecord);
// SAFETY: see type-level comment above.
unsafe impl Send for SendWalkRecord {}

/// UI Automation-backed implementation of [`Backend`] for Windows.
pub struct WindowsBackend {
    automation: UIAutomation,
    elements: HashMap<ElementId, UIElement>,
    next_id: u64,
    /// ID generation of `elements`: renewed whenever the table is
    /// cleared and `next_id` reset, and stored with every cache entry
    /// this backend inserts. A cache hit is only usable when its
    /// generation matches, because cached `Element`s carry IDs that
    /// only mean something to the table that minted them (issue #22).
    generation: Generation,
    /// Per-HWND enumeration cache. Backs the "press Esc, retry"
    /// flow — same window, same elements, no UIA round-trip. Disabled
    /// by setting `enable_caching = false` in `config.toml`.
    ///
    /// Phase 4 of the perf plan introduces a pre-warm worker thread
    /// (`crate::windows::prewarm`) that owns its own
    /// [`WindowsBackend`] but writes into the *same* `CacheManager`
    /// as the synchronous hotkey path. The shared `Arc<Mutex<…>>`
    /// makes that hand-off lock-free for readers in the common case
    /// (`parking_lot::Mutex` is uncontended ~99 % of the time) and
    /// keeps the cache's existing API intact — callers just go
    /// through `self.cache.lock()` instead of touching it directly.
    cache: Arc<Mutex<CacheManager>>,
    /// Hard cap on elements per enumeration in non-active-window scope
    /// modes. The single-window walk caps are
    /// `MAX_ELEMENTS_DESKTOP`/`MAX_ELEMENTS_BROWSER`; this kicks in
    /// only when we're aggregating across multiple windows.
    max_elements_global: usize,
    /// Pre-built UIA cache request that prefetches every property and
    /// pattern the walker reads on each node. With this attached to
    /// `element_from_handle_build_cache` and the `*_build_cache`
    /// walker methods, every `get_cached_*` call inside `try_record*`
    /// resolves from in-process memory instead of a cross-process COM
    /// round-trip — the dominant cost of a UIA tree walk on real apps
    /// (each remote get is a few-hundred-microsecond IPC). Built once
    /// in [`Self::with_config`] and reused for every walk.
    cache_request: UICacheRequest,
    /// Pre-built UIA condition that pre-filters the desktop walk on
    /// the server side: "controls of an interactable type or any
    /// element advertising an action pattern, that is not currently
    /// scrolled offscreen". Phase 2 of the perf plan replaces the
    /// recursive control-view walker (one IPC per `get_first_child` /
    /// `get_next_sibling`, N hops total) with a single
    /// `find_all_build_cache(Subtree, …)` call against this condition,
    /// collapsing an O(N)-IPC walk into one IPC for desktop apps.
    /// Browser windows still go through the recursive walker because
    /// their DOM trees over-match every generic control-type predicate.
    desktop_condition: uiautomation::core::UICondition,
}

impl WindowsBackend {
    /// Construct a new backend with caching enabled and the default
    /// 500ms TTL. Use [`Self::with_config`] to override either knob.
    pub fn new() -> anyhow::Result<Self> {
        Self::with_config(true, 500, 300)
    }

    /// Construct a new backend, initializing the UI Automation client.
    /// `enable_caching` and `cache_ttl_ms` configure the cache; the
    /// Settings dialog calls [`Self::reconfigure_cache`] to flip the
    /// switch at runtime without restarting.
    pub fn with_config(
        enable_caching: bool,
        cache_ttl_ms: u64,
        max_elements_global: usize,
    ) -> anyhow::Result<Self> {
        let cache = Arc::new(Mutex::new(CacheManager::new(cache_ttl_ms, enable_caching)));
        Self::with_shared_cache(cache, max_elements_global)
    }

    /// Construct a backend whose `CacheManager` is shared with another
    /// `WindowsBackend` (typically the pre-warm worker — see
    /// [`crate::windows::prewarm`]). Both backends own their own
    /// `UIAutomation` client (UIA is per-thread / per-apartment) but
    /// hits one populates the other's cache for free.
    pub fn with_shared_cache(
        cache: Arc<Mutex<CacheManager>>,
        max_elements_global: usize,
    ) -> anyhow::Result<Self> {
        // PerMonitorV2 ensures `GetBoundingRectangle` returns physical
        // pixels matching the overlay's coordinate space on high-DPI
        // displays. Best-effort: ignore "already set" / older-OS errors.
        unsafe {
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        }

        let automation = UIAutomation::new().context("initializing UI Automation client")?;
        let cache_request = build_cache_request(&automation)
            .context("building UIA cache request for prefetched properties/patterns")?;
        let desktop_condition = build_desktop_condition(&automation)
            .context("building UIA desktop pre-filter condition for FindAll(Subtree, …)")?;
        Ok(Self {
            automation,
            elements: HashMap::new(),
            next_id: 0,
            generation: Generation::next(),
            cache,
            max_elements_global,
            cache_request,
            desktop_condition,
        })
    }

    /// Hand out a clone of the shared cache handle so the pre-warm
    /// worker can write into the same `CacheManager` the hotkey path
    /// reads from. Cheap (atomic refcount bump on the `Arc`).
    pub fn cache_handle(&self) -> Arc<Mutex<CacheManager>> {
        Arc::clone(&self.cache)
    }

    /// Mirror new cache settings from [`crate::Config`] into the live
    /// backend without restarting. Disabling caching also clears the
    /// existing entries.
    pub fn reconfigure_cache(&mut self, enable_caching: bool, cache_ttl_ms: u64) {
        self.cache.lock().reconfigure(cache_ttl_ms, enable_caching);
    }

    /// Replace the global element cap (used in multi-window scope
    /// modes). Single-window walks still cap at the platform-specific
    /// constants — this is purely a safety net for the aggregate.
    pub fn set_max_elements_global(&mut self, max: usize) {
        self.max_elements_global = max.max(1);
    }

    /// Enumerate elements scoped by [`ScopeMode`].
    ///
    /// - [`ScopeMode::ActiveWindow`] is exactly [`Backend::enumerate_foreground`].
    /// - [`ScopeMode::ActiveMonitor`] enumerates every visible top-level
    ///   window whose frame overlaps [`PickContext::active_monitor`]
    ///   (the focused window's monitor; see [`resolve_active_monitor`]).
    /// - [`ScopeMode::AllWindows`] enumerates every visible top-level
    ///   window across all monitors.
    ///
    /// `ctx` is the snapshot of focus / cursor / monitor taken once at
    /// the start of the pick, so enumeration and hint ordering agree
    /// even if focus changes during the (slow) UIA walk.
    ///
    /// Multi-window modes aggregate per-window walks and apply the
    /// [`Self::max_elements_global`] cap so a busy desktop can't render
    /// thousands of badges. The cap is enforced in walk order, so the
    /// foreground window's elements are always present.
    pub fn enumerate_by_scope(
        &mut self,
        mode: ScopeMode,
        ctx: &PickContext,
    ) -> anyhow::Result<Vec<Element>> {
        let span = tel_span!(
            "enumerate",
            cache = tracing::field::Empty,
            collected = tracing::field::Empty
        )
        .entered();
        let result = match mode {
            ScopeMode::ActiveWindow => self.enumerate_foreground(),
            ScopeMode::ActiveMonitor => self.enumerate_active_monitor(ctx),
            ScopeMode::AllWindows => self.enumerate_windows_filtered(ctx.foreground, |_| true),
        };
        tel_record!(
            span,
            "collected",
            result.as_ref().map_or(0, |e| e.len()) as u64
        );
        result
    }

    fn enumerate_active_monitor(&mut self, ctx: &PickContext) -> anyhow::Result<Vec<Element>> {
        // No resolvable monitor → overshoot to every window rather than
        // refuse to enumerate anything.
        let Some(monitor_rect) = ctx.active_monitor else {
            tracing::warn!("active monitor unresolved; falling back to all windows");
            return self.enumerate_windows_filtered(ctx.foreground, |_| true);
        };
        // Phase 5: tightened prune. The previous "any pixel overlaps"
        // check pulled in windows that just barely touched the active
        // monitor (e.g. a sliver of an off-screen Slack window),
        // wasting a UIA walk per spurious match. Requiring 10% area
        // overlap drops those without losing any window the user
        // would actually consider "on this monitor".
        self.enumerate_windows_filtered(ctx.foreground, |bounds| {
            rect_overlap_fraction(&monitor_rect, bounds) >= 0.10
        })
    }

    /// Shared implementation behind the multi-window scope modes.
    ///
    /// Phase 5: parallelize the per-window walks across rayon worker
    /// threads, grouped by process id. Most of the time in this
    /// function is spent waiting on COM IPC against remote app
    /// processes, which is embarrassingly parallel — the only thing
    /// we *can't* do is hammer two HWNDs of the same app
    /// simultaneously (they share one UI thread on the target side),
    /// so the pid grouping serializes per-app work onto a single
    /// worker. The foreground window's pid group is walked locally
    /// to keep its UIElements bound to this backend's apartment for
    /// `perform()` (rayon workers register theirs on their own
    /// thread, which is fine for invocation thanks to MTA marshalling
    /// but risks subtle ordering bugs we don't need to court).
    ///
    /// Errors from a single window's walk are swallowed with a
    /// `tracing` warning — better to return what we did manage to
    /// enumerate than abort the whole pick because one tab's UIA
    /// tree was unhealthy.
    fn enumerate_windows_filtered<F>(
        &mut self,
        foreground: HWND,
        mut accept: F,
    ) -> anyhow::Result<Vec<Element>>
    where
        F: FnMut(&Bounds) -> bool,
    {
        use std::collections::BTreeMap;

        let list_span = tel_span!(
            "list_windows",
            candidates = tracing::field::Empty,
            planned = tracing::field::Empty
        )
        .entered();
        let candidates = crate::windows::window_picker::enumerate_visible()
            .context("listing top-level windows for multi-window scope")?;
        tel_record!(list_span, "candidates", candidates.len() as u64);
        let cap = self.max_elements_global;

        // Decide which windows to walk and which higher-z windows sit on
        // top of each (see `plan_windows`), then group by pid. BTreeMap
        // so iteration order is stable across runs (helpful for test
        // reproducibility and for attributing trace logs to specific
        // apps). The foreground pid gets walked first; everything else
        // fans out onto rayon.
        let mut by_pid: BTreeMap<u32, Vec<PlannedWindow>> = BTreeMap::new();
        let mut foreground_pid: Option<u32> = None;
        for planned in plan_windows(candidates, foreground, &mut accept) {
            let pid = window_pid(planned.win.hwnd);
            if planned.win.hwnd == foreground {
                foreground_pid = Some(pid);
            }
            by_pid.entry(pid).or_default().push(planned);
        }
        tel_record!(
            list_span,
            "planned",
            by_pid.values().map(Vec::len).sum::<usize>() as u64
        );
        drop(list_span);

        // Walk the foreground pid group locally (sequentially through
        // `enumerate_window`, which goes through the cache and binds
        // UIElements into `self.elements`). This guarantees the
        // hotkey-driven backend can invoke any element on the active
        // window via `perform()` even if a parallel worker had
        // pre-walked the same HWND.
        //
        // The foreground HWND is allowed the full per-window quota
        // (it's the user's primary target); other windows in the same
        // pid group share the smaller `PER_WINDOW_HINT_CAP_MULTI`
        // budget so a single chatty app can't crowd everything else
        // out of the overlay.
        let mut out: Vec<Element> = Vec::with_capacity(64);
        if let Some(pid) = foreground_pid {
            if let Some(group) = by_pid.remove(&pid) {
                let mut group = group;
                group.sort_by_key(|p| if p.win.hwnd == foreground { 0 } else { 1 });
                for PlannedWindow { win, occluders } in group {
                    if out.len() >= cap {
                        break;
                    }
                    match self.enumerate_window(win.hwnd) {
                        Ok(elements) => {
                            let remaining = cap - out.len();
                            let per_window = if win.hwnd == foreground {
                                remaining
                            } else {
                                PER_WINDOW_HINT_CAP_MULTI.min(remaining)
                            };
                            // Drop occluded elements *before* the cap so
                            // hidden ones don't consume the window's quota.
                            let visible =
                                retain_unoccluded(elements, &occluders, |e| &e.bounds, &win);
                            out.extend(visible.into_iter().take(per_window));
                        }
                        Err(e) => {
                            tracing::warn!(?win.hwnd, title = %win.title, error = ?e,
                                "foreground-pid window walk failed; skipping");
                        }
                    }
                }
            }
        }

        // Remaining pid groups fan out across rayon workers. Each
        // worker constructs its own `UIAutomation` (the `with_config`
        // CoInitializeEx happens on the worker thread), then walks
        // every HWND in its group. Records come back wrapped in
        // [`SendWalkRecord`] (see safety note on that type).
        if !by_pid.is_empty() && out.len() < cap {
            use rayon::prelude::*;
            let groups: Vec<(u32, Vec<PlannedWindow>)> = by_pid.into_iter().collect();
            let _par = tel_span!("parallel_walks", groups = groups.len() as u64).entered();
            // Rayon workers don't inherit this thread's current span, so
            // their spans get it as an explicit parent (telemetry).
            let parent = tel_current!();
            let walked: Vec<Vec<SendWalkRecord>> = groups
                .into_par_iter()
                .map(|(pid, windows)| {
                    let _worker = tel_span!(
                        parent: &parent,
                        "worker",
                        pid,
                        windows = windows.len() as u64
                    )
                    .entered();
                    let automation = match UIAutomation::new() {
                        Ok(a) => a,
                        Err(e) => {
                            tracing::warn!(pid, error = ?e,
                                "rayon worker: UIAutomation init failed");
                            return Vec::new();
                        }
                    };
                    let cache_request = match build_cache_request(&automation) {
                        Ok(r) => r,
                        Err(e) => {
                            tracing::warn!(pid, error = ?e,
                                "rayon worker: build_cache_request failed");
                            return Vec::new();
                        }
                    };
                    let desktop_condition = match build_desktop_condition(&automation) {
                        Ok(c) => c,
                        Err(e) => {
                            tracing::warn!(pid, error = ?e,
                                "rayon worker: build_desktop_condition failed");
                            return Vec::new();
                        }
                    };
                    let mut local: Vec<SendWalkRecord> = Vec::with_capacity(64);
                    for PlannedWindow { win, occluders } in windows {
                        let is_browser = is_browser_window(win.hwnd);
                        match walk_window(
                            &automation,
                            &cache_request,
                            &desktop_condition,
                            win.hwnd,
                            is_browser,
                        ) {
                            Ok(records) => {
                                // Drop occluded elements before the cap
                                // so hidden ones don't consume quota.
                                let records =
                                    retain_unoccluded(records, &occluders, |r| &r.bounds, &win);
                                // Per-window cap so one busy background
                                // window (e.g. a Cursor / VS Code with
                                // hundreds of UIA matches) can't eat the
                                // global budget alone, leaving the user's
                                // other open apps unbadged on
                                // multi-monitor setups.
                                local.extend(
                                    records
                                        .into_iter()
                                        .take(PER_WINDOW_HINT_CAP_MULTI)
                                        .map(SendWalkRecord),
                                );
                            }
                            Err(e) => {
                                tracing::warn!(?win.hwnd, title = %win.title, error = ?e,
                                    "parallel window walk failed; skipping");
                            }
                        }
                    }
                    local
                })
                .collect();

            // Merge in pid-group order so logging stays
            // deterministic. `register_records` mints the global
            // `ElementId`s and stuffs UIElements into `self.elements`
            // so `perform()` works on every returned element.
            for group in walked {
                if out.len() >= cap {
                    break;
                }
                let remaining = cap - out.len();
                let take: Vec<WalkRecord> =
                    group.into_iter().map(|w| w.0).take(remaining).collect();
                let elements = self.register_records(take);
                out.extend(elements);
            }
        }

        tracing::info!(
            collected = out.len(),
            cap,
            "enumerate_windows_filtered done"
        );
        Ok(out)
    }

    /// Walk a single HWND, going through the cache first. Used by the
    /// multi-window scope modes and by the foreground path.
    pub fn enumerate_window(&mut self, hwnd: HWND) -> anyhow::Result<Vec<Element>> {
        let key = hwnd_to_key(hwnd);
        let lookup = self.cache.lock().lookup(key, self.generation);
        if let Lookup::Hit(cached) = lookup {
            return Ok(cached.as_ref().to_vec());
        }
        // Otherwise (no entry, or IDs from another generation: the
        // pre-warm worker's backend, or before this backend's last
        // reset) walk fresh, so `self.elements` holds UIElement
        // handles for exactly these IDs when `perform()` runs.

        let is_browser = is_browser_window(hwnd);
        let records = walk_window(
            &self.automation,
            &self.cache_request,
            &self.desktop_condition,
            hwnd,
            is_browser,
        )?;
        let elements = self.register_records(records);

        // Never cache an empty result: it's usually a page Chromium
        // hadn't built yet (#53), and caching it would serve "nothing"
        // again on the next press.
        if !elements.is_empty() {
            let shared: Arc<[Element]> = Arc::from(elements.clone().into_boxed_slice());
            self.cache.lock().insert(key, shared, self.generation);
        }
        Ok(elements)
    }

    /// Cache-warming walk of one HWND, for the pre-warm worker's
    /// backend. Returns `Ok(None)` without walking when the window
    /// already has a fresh entry from *any* backend: replacing the
    /// hotkey path's own entry with ours would turn its usable hit
    /// ("Esc, retry") into a miss, because our IDs aren't valid there.
    ///
    /// This backend never performs actions, so each walk starts a new
    /// ID table instead of growing one forever (each entry pins a UIA
    /// element).
    pub fn warm_window(&mut self, hwnd: HWND) -> anyhow::Result<Option<usize>> {
        let key = hwnd_to_key(hwnd);
        if self.cache.lock().has_fresh(key) {
            return Ok(None);
        }
        self.elements.clear();
        self.next_id = 0;
        self.generation = Generation::next();

        let is_browser = is_browser_window(hwnd);
        let records = walk_window(
            &self.automation,
            &self.cache_request,
            &self.desktop_condition,
            hwnd,
            is_browser,
        )?;
        let elements = self.register_records(records);
        let count = elements.len();
        // Never cache an empty result (#53); see `enumerate_window`.
        if count > 0 {
            self.cache
                .lock()
                .insert(key, Arc::from(elements.into_boxed_slice()), self.generation);
        }
        Ok(Some(count))
    }

    /// Drain a batch of [`WalkRecord`]s into `self.elements`,
    /// minting fresh IDs and producing the public [`Element`] list
    /// in the same order. Centralized so the foreground-,
    /// single-window-, and multi-window-merge paths can't drift.
    fn register_records(&mut self, records: Vec<WalkRecord>) -> Vec<Element> {
        let _span = tel_span!("register", n = records.len() as u64).entered();
        let mut out = Vec::with_capacity(records.len());
        for rec in records {
            let id = ElementId(self.next_id);
            self.next_id = self.next_id.wrapping_add(1);
            self.elements.insert(id, rec.element);
            out.push(Element {
                id,
                role: rec.role,
                name: rec.name,
                bounds: rec.bounds,
            });
        }
        out
    }
}

/// Build the per-process UIA cache request once, attaching every property
/// and pattern the walker / `try_record*` paths consult. This is the spine
/// of Phase 1 of the perf plan: by sending the entire shopping list to UIA
/// up front, every subsequent `get_cached_*` call reads from in-process
/// memory instead of round-tripping to the target app's UI thread.
///
/// Anything fetched by the walker must be added here — the cached getter
/// returns an error otherwise. We deliberately *over-cache* a couple of
/// rarely-used properties (LocalizedControlType, AutomationId) so future
/// debug logging can pick them up without forcing a re-roll of the
/// request.
fn build_cache_request(automation: &UIAutomation) -> uiautomation::Result<UICacheRequest> {
    let req = automation.create_cache_request()?;

    // Properties read by `try_record`, `try_record_desktop_element`,
    // `try_record_web_element`, and `create_element`.
    for prop in [
        UIProperty::ControlType,
        UIProperty::BoundingRectangle,
        UIProperty::IsOffscreen,
        UIProperty::IsEnabled,
        UIProperty::IsKeyboardFocusable,
        UIProperty::IsControlElement,
        UIProperty::Name,
        UIProperty::AutomationId,
        UIProperty::LocalizedControlType,
        UIProperty::ClassName,
    ] {
        req.add_property(prop)?;
    }

    // Patterns probed by `has_any_action_pattern` / `looks_clickable_web`.
    // `Scroll` is here so that the future `Action::Scroll` path can stay
    // on cached pattern handles too. `LegacyIAccessible` is needed because
    // `looks_clickable_web` reads its `get_cached_role()`.
    for pat in [
        UIPatternType::Invoke,
        UIPatternType::Toggle,
        UIPatternType::SelectionItem,
        UIPatternType::ExpandCollapse,
        UIPatternType::LegacyIAccessible,
        UIPatternType::Scroll,
    ] {
        req.add_pattern(pat)?;
    }

    Ok(req)
}

/// Build the desktop pre-filter once per backend lifetime. Phase 2 of the
/// perf plan replaces the recursive control-view tree walk with a single
/// `find_all_build_cache(TreeScope::Subtree, …)` call against this
/// condition; UIA evaluates it on the target app's UI thread and ships
/// back only the elements that match (already cached per
/// [`build_cache_request`]).
///
/// Conceptually the condition is:
///
/// ```text
/// And(
///   IsControlElement = true,
///   Or(
///     ControlType in { Button, SplitButton, Hyperlink, Edit, MenuItem,
///                       TabItem, CheckBox, RadioButton, ComboBox,
///                       ListItem, TreeItem, DataItem, Image },
///     IsInvokePatternAvailable = true,
///     IsTogglePatternAvailable = true,
///     IsSelectionItemPatternAvailable = true,
///     IsExpandCollapsePatternAvailable = true,
///   ),
///   Not(IsOffscreen = true),
/// )
/// ```
///
/// The `uiautomation` crate exposes the boolean combinators in binary
/// form, so the implementation reduces lists with a small `fold` helper.
fn build_desktop_condition(
    automation: &UIAutomation,
) -> uiautomation::Result<uiautomation::core::UICondition> {
    use uiautomation::core::UICondition;
    use uiautomation::types::PropertyConditionFlags;
    use uiautomation::variants::Variant;

    fn or_all(
        automation: &UIAutomation,
        mut conds: Vec<UICondition>,
    ) -> uiautomation::Result<UICondition> {
        let mut acc = conds.remove(0);
        for c in conds {
            acc = automation.create_or_condition(acc, c)?;
        }
        Ok(acc)
    }

    let prop = |property: UIProperty, value: Variant| -> uiautomation::Result<UICondition> {
        automation.create_property_condition(property, value, Some(PropertyConditionFlags::None))
    };

    // ControlTypes worth hinting on a desktop app. Mirrors the
    // accept-list inside `map_role` plus `Image` (which the desktop
    // path also accepts via the structural fallback). UIA stores
    // ControlType as the `UIA_*ControlTypeId` integer, so we pass the
    // enum's `i32` discriminant.
    let ct_ids: [ControlType; 13] = [
        ControlType::Button,
        ControlType::SplitButton,
        ControlType::Hyperlink,
        ControlType::Edit,
        ControlType::MenuItem,
        ControlType::TabItem,
        ControlType::CheckBox,
        ControlType::RadioButton,
        ControlType::ComboBox,
        ControlType::ListItem,
        ControlType::TreeItem,
        ControlType::DataItem,
        ControlType::Image,
    ];
    let mut control_type_conds: Vec<UICondition> = Vec::with_capacity(ct_ids.len());
    for ct in ct_ids {
        control_type_conds.push(prop(UIProperty::ControlType, Variant::from(ct as i32))?);
    }
    let by_control_type = or_all(automation, control_type_conds)?;

    // Pattern-availability fallback: catches custom controls whose
    // ControlType is structural (Pane / Group / Custom) but that still
    // expose an action pattern. Stays in lock-step with
    // `has_any_action_pattern`.
    let pattern_conds = vec![
        prop(UIProperty::IsInvokePatternAvailable, Variant::from(true))?,
        prop(UIProperty::IsTogglePatternAvailable, Variant::from(true))?,
        prop(
            UIProperty::IsSelectionItemPatternAvailable,
            Variant::from(true),
        )?,
        prop(
            UIProperty::IsExpandCollapsePatternAvailable,
            Variant::from(true),
        )?,
    ];
    let by_pattern = or_all(automation, pattern_conds)?;

    let interactable = automation.create_or_condition(by_control_type, by_pattern)?;

    let is_control_element = prop(UIProperty::IsControlElement, Variant::from(true))?;

    let offscreen = prop(UIProperty::IsOffscreen, Variant::from(true))?;
    let on_screen = automation.create_not_condition(offscreen)?;

    let with_control_element = automation.create_and_condition(is_control_element, interactable)?;
    automation.create_and_condition(with_control_element, on_screen)
}

/// Read a cached boolean property as a `Result<bool>`. The `uiautomation`
/// crate doesn't expose a direct `get_cached_is_*` for arbitrary boolean
/// properties — only for a handful of named ones — so we go through the
/// generic `get_cached_property_value` path and convert via `Variant`.
/// Non-bool variants are surfaced as `Err`; callers fall back to the
/// safe default themselves (usually "treat as enabled / on-screen" so a
/// quirky control isn't dropped silently).
fn cached_bool(el: &UIElement, prop: UIProperty) -> uiautomation::Result<bool> {
    let v = el.get_cached_property_value(prop)?;
    v.try_into()
}

/// Pack a Win32 HWND into the cache's opaque [`WindowKey`]. Round-trip
/// is one-way (we never reconstruct an HWND from a key); we only need
/// equality + hashing inside the map.
pub(crate) fn hwnd_to_key(hwnd: HWND) -> WindowKey {
    hwnd.0 as usize as u64
}

/// Snapshot of "where the user is" taken once at the start of an
/// element pick. Enumeration ([`WindowsBackend::enumerate_by_scope`])
/// and label ordering ([`hint_anchor`]) both read from it, so they
/// can't disagree if focus or the cursor moves during the UIA walk.
#[derive(Debug, Clone, Copy)]
pub struct PickContext {
    /// Foreground window at capture time. May be null.
    pub foreground: HWND,
    /// Cursor position in physical screen pixels, if it could be read.
    pub cursor: Option<(i32, i32)>,
    /// The monitor that [`ScopeMode::ActiveMonitor`] targets, resolved
    /// by [`resolve_active_monitor`]. `None` when no monitor could be
    /// determined; callers treat that as "no monitor constraint".
    pub active_monitor: Option<RECT>,
}

impl PickContext {
    /// Read the foreground window, cursor and monitors from Win32 and
    /// resolve the active monitor.
    pub fn capture() -> Self {
        use windows::Win32::Graphics::Gdi::{MonitorFromWindow, MONITOR_DEFAULTTONULL};

        // SAFETY: these Win32 queries have no preconditions; every
        // failure is mapped to `None` / a null handle and handled below.
        unsafe {
            let foreground = GetForegroundWindow();
            let mut pt = POINT { x: 0, y: 0 };
            let cursor = GetCursorPos(&mut pt).ok().map(|_| (pt.x, pt.y));

            let has_foreground = !foreground.0.is_null();
            let fg_is_shell = has_foreground && window_picker::is_shell_window(foreground);
            let fg_monitor = if has_foreground && !fg_is_shell {
                monitor_rect(MonitorFromWindow(foreground, MONITOR_DEFAULTTONULL))
            } else {
                None
            };
            let cursor_monitor = cursor.and_then(|(x, y)| {
                monitor_rect(MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST))
            });

            Self {
                foreground,
                cursor,
                active_monitor: resolve_active_monitor(fg_monitor, fg_is_shell, cursor_monitor),
            }
        }
    }
}

/// Pick the monitor [`ScopeMode::ActiveMonitor`] should target.
///
/// The focused window's monitor wins, so picking a window on another
/// screen (window picker, Alt+Tab, a click) moves the scope with it
/// even though the mouse stayed put. When the desktop shell has focus
/// (e.g. after Win+D) there is no meaningful focused window, so the
/// monitor under the cursor is used instead, as it is when the
/// foreground monitor can't be resolved at all.
pub fn resolve_active_monitor(
    fg_monitor: Option<RECT>,
    fg_is_shell: bool,
    cursor_monitor: Option<RECT>,
) -> Option<RECT> {
    if fg_is_shell {
        return cursor_monitor;
    }
    fg_monitor.or(cursor_monitor)
}

/// Full rect (physical pixels) of `monitor`, or `None` for a null
/// handle / failed `GetMonitorInfoW`.
unsafe fn monitor_rect(monitor: windows::Win32::Graphics::Gdi::HMONITOR) -> Option<RECT> {
    use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITORINFO};

    if monitor.is_invalid() {
        return None;
    }
    let mut mi = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    GetMonitorInfoW(monitor, &mut mi)
        .as_bool()
        .then_some(mi.rcMonitor)
}

fn rect_center(r: &RECT) -> (i32, i32) {
    ((r.left + r.right) / 2, (r.top + r.bottom) / 2)
}

/// `point` if it lies inside `rect` (right/bottom exclusive), else the
/// center of `rect`. Keeps cursor-ordered labels on the active monitor
/// when the pointer has been left on another screen.
fn anchor_in_or_center(point: (i32, i32), rect: &RECT) -> (i32, i32) {
    let (x, y) = point;
    if x >= rect.left && x < rect.right && y >= rect.top && y < rect.bottom {
        point
    } else {
        rect_center(rect)
    }
}

/// Resolve the point the element picker's shortest labels should
/// radiate out from. Returns `None` for [`HintOrder::Reading`] (keep
/// enumeration order). Any Win32 failure falls back to the center of
/// the elements' combined bounding box.
pub fn hint_anchor(
    order: HintOrder,
    scope: ScopeMode,
    ctx: &PickContext,
    elements: &[Element],
) -> Option<(i32, i32)> {
    let resolved = match order {
        HintOrder::Reading => return None,
        HintOrder::Cursor => match (scope, ctx.active_monitor) {
            // The pointer may still be on another screen (e.g. after
            // the window picker moved focus); radiate from the active
            // monitor's center instead of its nearest edge.
            (ScopeMode::ActiveMonitor, Some(m)) => Some(
                ctx.cursor
                    .map_or_else(|| rect_center(&m), |p| anchor_in_or_center(p, &m)),
            ),
            _ => ctx.cursor,
        },
        HintOrder::Center => match scope {
            ScopeMode::ActiveWindow => {
                if ctx.foreground.0.is_null() {
                    None
                } else {
                    // SAFETY: `visible_frame` tolerates stale / invalid
                    // HWNDs and reports `None` for them.
                    unsafe { window_picker::visible_frame(ctx.foreground) }
                        .filter(|b| b.is_visible())
                        .map(|b| b.center())
                }
            }
            ScopeMode::ActiveMonitor => ctx.active_monitor.as_ref().map(rect_center),
            ScopeMode::AllWindows => None,
        },
    };
    resolved.or_else(|| union_center(elements))
}

/// Fraction of the window's frame area that overlaps `monitor`, in
/// [0.0, 1.0]. Phase 5 replaces the old "any pixel overlaps" check
/// for `enumerate_active_monitor` so a window that's only just
/// touching the active monitor (off-screen Slack, half-snapped Edge)
/// is no longer worth a UIA walk. The all-windows scope mode skips
/// this check entirely — it wants *every* visible HWND.
fn rect_overlap_fraction(monitor: &RECT, bounds: &Bounds) -> f32 {
    let area = (bounds.width as i64) * (bounds.height as i64);
    if area <= 0 {
        return 0.0;
    }
    let r_right = bounds.x + bounds.width;
    let r_bottom = bounds.y + bounds.height;
    let ix1 = bounds.x.max(monitor.left);
    let iy1 = bounds.y.max(monitor.top);
    let ix2 = r_right.min(monitor.right);
    let iy2 = r_bottom.min(monitor.bottom);
    let iw = (ix2 - ix1).max(0) as i64;
    let ih = (iy2 - iy1).max(0) as i64;
    let intersection = iw * ih;
    intersection as f32 / area as f32
}

/// A top-level window selected for walking by [`plan_windows`], plus
/// the frames of the higher-z windows that overlap it.
#[derive(Debug, Clone)]
struct PlannedWindow {
    win: crate::windows::window_picker::TopLevelWindow,
    /// Frames of visible higher-z windows intersecting `win.bounds`.
    /// Elements of `win` entirely under the union of these are hidden.
    occluders: Vec<Bounds>,
}

/// Decide which windows a multi-window scope should walk.
///
/// `candidates` must be in top-down z-order (as returned by
/// `enumerate_visible`). Pure function — no UIA, no Win32 — so the
/// selection logic is unit-testable with synthetic windows.
///
/// - Minimized windows are skipped and never occlude anything: their
///   `bounds` is a synthetic placeholder, not a real frame.
/// - Every other window counts as an occluder for the windows below it,
///   **including** ones `accept` rejects (a window living mostly on
///   another monitor can still cover part of this one).
/// - A window fully covered by the *union* of the windows above it is
///   skipped — except the foreground window, which is always walked.
/// - Each kept window carries the subset of higher-z frames that
///   overlap it, for per-element filtering after the walk.
fn plan_windows<F>(
    candidates: Vec<crate::windows::window_picker::TopLevelWindow>,
    foreground: HWND,
    mut accept: F,
) -> Vec<PlannedWindow>
where
    F: FnMut(&Bounds) -> bool,
{
    let mut above: Vec<Bounds> = Vec::with_capacity(candidates.len());
    let mut planned = Vec::new();
    for win in candidates {
        if win.is_minimized {
            continue;
        }
        let bounds = win.bounds;
        if accept(&bounds) {
            let occluders: Vec<Bounds> = above
                .iter()
                .filter(|o| o.intersects(&bounds))
                .copied()
                .collect();
            if win.hwnd != foreground && !bounds.is_partly_visible(&occluders) {
                tracing::debug!(?win.hwnd, title = %win.title,
                    "skipping fully occluded window");
            } else {
                planned.push(PlannedWindow { win, occluders });
            }
        }
        above.push(bounds);
    }
    planned
}

/// Keep only the items whose bounds are at least partly visible past
/// `occluders`, logging how many were dropped for `win`.
fn retain_unoccluded<T>(
    items: Vec<T>,
    occluders: &[Bounds],
    bounds_of: impl Fn(&T) -> &Bounds,
    win: &crate::windows::window_picker::TopLevelWindow,
) -> Vec<T> {
    if occluders.is_empty() {
        return items;
    }
    let before = items.len();
    let kept: Vec<T> = items
        .into_iter()
        .filter(|item| bounds_of(item).is_partly_visible(occluders))
        .collect();
    let dropped = before - kept.len();
    if dropped > 0 {
        tracing::debug!(?win.hwnd, title = %win.title, dropped, kept = kept.len(),
            "dropped occluded elements");
    }
    kept
}

/// Look up the owning process id of `hwnd`. Returns `0` if the call
/// fails (typically because the HWND was destroyed between
/// enumeration and pid lookup); callers treat `0` as a synthetic
/// "unknown" group so those windows still get walked, just on their
/// own worker.
fn window_pid(hwnd: HWND) -> u32 {
    let mut pid: u32 = 0;
    // SAFETY: `GetWindowThreadProcessId` writes through `&mut pid`
    // and tolerates a null hwnd (returns 0).
    unsafe {
        let _ = GetWindowThreadProcessId(hwnd, Some(&mut pid as *mut u32));
    }
    pid
}

impl Backend for WindowsBackend {
    fn enumerate_foreground(&mut self) -> anyhow::Result<Vec<Element>> {
        // SAFETY: `GetForegroundWindow` has no preconditions and may return a
        // null handle, which we check below.
        let hwnd = unsafe { GetForegroundWindow() };
        if hwnd.0.is_null() {
            tracing::warn!("no foreground window");
            return Ok(Vec::new());
        }

        let is_browser = is_browser_window(hwnd);
        tracing::debug!(is_browser, "enumerate_foreground: detected window kind");

        // Cache lookup before any UIA work. A hit means we were called
        // again on the same HWND inside the TTL — either the
        // "user pressed Esc, retrying" path or, with Phase 4 in
        // place, the pre-warm worker beating the user to the walk
        // after a foreground change. We only honor a hit from this
        // backend's current ID generation: an entry from another
        // backend, or from before our last reset, has ID numbers that
        // may exist in `self.elements` but name different UIA
        // elements, so picking would act on the wrong control
        // (issue #22).
        let key = hwnd_to_key(hwnd);
        tel_record_current!("cache", "miss");
        let lookup = self.cache.lock().lookup(key, self.generation);
        match lookup {
            Lookup::Hit(cached) => {
                tel_record_current!("cache", "hit");
                tracing::debug!(
                    cached = cached.len(),
                    "enumerate_foreground served from cache"
                );
                return Ok(cached.as_ref().to_vec());
            }
            Lookup::OtherGeneration => {
                tel_record_current!("cache", "hit_rejected");
                tracing::debug!(
                    "cache entry from another ID generation; rewalking to bind UIElements"
                );
            }
            Lookup::Miss => {}
        }

        // Reset the local UIElement registry on a real miss; we hand
        // out fresh IDs starting from zero so old badges from the
        // previous walk can't be invoked by mistake. Starting a new
        // generation makes every cache entry minted against the old
        // table unusable to us, since its ID numbers are about to be
        // reused.
        self.elements.clear();
        self.next_id = 0;
        self.generation = Generation::next();

        let records = match walk_window(
            &self.automation,
            &self.cache_request,
            &self.desktop_condition,
            hwnd,
            is_browser,
        ) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(?hwnd, error = ?e, "foreground window walk failed");
                return Ok(Vec::new());
            }
        };
        let out = self.register_records(records);

        // Refresh the cache with the just-collected elements so the
        // next call within `cache_ttl_ms` can short-circuit the walk
        // entirely. The cache holds an `Arc<[Element]>` so subsequent
        // cache hits are a single refcount bump even for a 2k-element
        // browser walk.
        //
        // Never cache an empty result (#53): it's usually a page
        // Chromium hadn't built yet, and the next press should walk.
        if !out.is_empty() {
            let shared: Arc<[Element]> = Arc::from(out.clone().into_boxed_slice());
            self.cache.lock().insert(key, shared, self.generation);
        }

        // Per-role breakdown helps when a user reports "jumpkey didn't see
        // button X in app Y" — the log shows whether the element was
        // dropped by the role filter or never made it through the bounds
        // / pattern checks at all.
        if tracing::enabled!(tracing::Level::DEBUG) {
            let mut by_role: HashMap<Role, usize> = HashMap::new();
            for el in &out {
                *by_role.entry(el.role).or_insert(0) += 1;
            }
            tracing::debug!(collected = out.len(), ?by_role, "enumerate_foreground done");
        }

        Ok(out)
    }

    fn perform(&mut self, id: ElementId, action: Action) -> anyhow::Result<()> {
        let el = self
            .elements
            .get(&id)
            .with_context(|| format!("unknown element id {id:?}"))?;

        match action {
            // Invoke goes through the UIA pattern cascade — no cursor warp,
            // works on virtualized / off-screen items, respects the app's
            // accessibility hooks. Mouse-click is the explicit last resort
            // for legacy controls that don't expose any pattern.
            Action::Invoke => {
                let path = invoke_smart(el).context("invoking element failed")?;
                tel_record_current!("invoke_path", tracing::field::debug(path));
                if path == InvokePath::MouseFallback {
                    tracing::warn!(?id, "no UIA pattern matched; used mouse-click fallback");
                } else {
                    tracing::debug!(?id, ?path, "element invoked via UIA pattern");
                }
            }
            // Click is the explicit "synthesize a real mouse click" action.
            // Different semantics from Invoke — caller is opting in to
            // cursor motion, useful for testing or for apps that only
            // respond to genuine input.
            Action::Click => {
                el.click().context("element.click() failed")?;
            }
            // RightClick and DoubleClick can't go through UIA — context
            // menus and double-click activation aren't expressible as
            // UIA patterns — so we synthesize real input. See `click_at`
            // for the SendInput details.
            Action::RightClick => {
                click_at(el, MouseButton::Right, 1).context("synthesizing right-click failed")?;
            }
            Action::DoubleClick => {
                click_at(el, MouseButton::Left, 2).context("synthesizing double-click failed")?;
            }
            Action::Focus => {
                el.set_focus().context("element.set_focus() failed")?;
            }
            Action::Type(s) => {
                el.send_text(&s, 0).context("element.send_text() failed")?;
            }
            Action::Scroll { dx, dy } => {
                scroll_element(el, dx, dy).context("scrolling element failed")?;
            }
        }
        Ok(())
    }
}

/// Which physical mouse button [`click_at`] should synthesize. Kept
/// here rather than in [`crate::Action`] because mapping is internal
/// to the Windows backend — other backends might express the same
/// intent differently.
#[derive(Debug, Clone, Copy)]
enum MouseButton {
    Left,
    Right,
}

impl MouseButton {
    /// `(down_flag, up_flag)` for a single press of this button. We
    /// build `MOUSEINPUT` records explicitly rather than going through
    /// a higher-level helper because we need the down/up events to land
    /// in a single `SendInput` batch (see [`click_at`] for why).
    fn flags(
        self,
    ) -> (
        windows::Win32::UI::Input::KeyboardAndMouse::MOUSE_EVENT_FLAGS,
        windows::Win32::UI::Input::KeyboardAndMouse::MOUSE_EVENT_FLAGS,
    ) {
        match self {
            MouseButton::Left => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
            MouseButton::Right => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
        }
    }
}

/// Synthesize `count` clicks of `button` at the centre of `el`.
///
/// Used by [`Action::RightClick`] (count=1, Right) and
/// [`Action::DoubleClick`] (count=2, Left). The two cases share a path
/// because both need the same three things: locate the element, warp
/// the cursor, and dispatch one or more down/up pairs through
/// `SendInput`.
///
/// Why `SendInput` and not the `uiautomation::UIElement::click()`
/// helper? `click()` only ever synthesizes a left-click and can't
/// guarantee double-click timing. We need a right-button variant and
/// we need both clicks of a double-click to land in the same input
/// batch so the OS treats them as one gesture (i.e. inside the
/// system's `GetDoubleClickTime` window with no scheduler jitter).
///
/// The cursor warp via `SetCursorPos` matters because most context
/// menus and double-click handlers are dispatched based on the cursor
/// position at the moment the button event fires, not on the HWND
/// under the keyboard focus.
fn click_at(el: &UIElement, button: MouseButton, count: u8) -> anyhow::Result<()> {
    let bounds = el
        .get_bounding_rectangle()
        .context("reading element bounds for synthesized click")?;
    // Element centre in physical pixels. `SetCursorPos` takes physical
    // pixels too thanks to PerMonitorV2 DPI awareness (set in
    // `Self::with_shared_cache`), so the two coordinate systems already
    // line up.
    let cx = bounds.get_left() + bounds.get_width() / 2;
    let cy = bounds.get_top() + bounds.get_height() / 2;

    unsafe {
        SetCursorPos(cx, cy).context("SetCursorPos failed")?;
    }

    let (down, up) = button.flags();
    // Pre-size for the worst case (double-click = two down/up pairs).
    // Single click only consumes the first two entries; the rest stay
    // initialized but never get passed to `SendInput`.
    let mut inputs: Vec<INPUT> = Vec::with_capacity((count as usize) * 2);
    for _ in 0..count {
        for &flag in &[down, up] {
            inputs.push(INPUT {
                r#type: INPUT_MOUSE,
                Anonymous: INPUT_0 {
                    mi: MOUSEINPUT {
                        dx: 0,
                        dy: 0,
                        mouseData: 0,
                        dwFlags: flag,
                        time: 0,
                        dwExtraInfo: JUMPKEY_INPUT_TAG,
                    },
                },
            });
        }
    }

    let cb = std::mem::size_of::<INPUT>() as i32;
    let sent = unsafe { SendInput(&inputs, cb) };
    if (sent as usize) != inputs.len() {
        anyhow::bail!(
            "SendInput injected {sent} of {} events (blocked by UIPI or another input desktop)",
            inputs.len()
        );
    }

    tracing::debug!(?button, count, cx, cy, "synthesized click via SendInput");
    Ok(())
}

/// Which path through [`invoke_smart`] succeeded. Surfaced via tracing so we
/// can spot apps that consistently fall through to the mouse fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InvokePath {
    Invoke,
    Toggle,
    SelectionItem,
    ExpandCollapse,
    LegacyDefaultAction,
    MouseFallback,
}

/// Try every UIA pattern that could plausibly "invoke" `el`, in order of
/// specificity, and return which one worked. Falls back to a synthesized
/// mouse click only when every pattern path has been exhausted.
///
/// Pattern cascade rationale:
///
/// 1. [`UIInvokePattern`] — the canonical "do the primary action" interface.
///    Buttons, links, menu items, split buttons all expose it.
/// 2. [`UITogglePattern`] — checkboxes, toggle buttons.
/// 3. [`UISelectionItemPattern`] — tabs, radios, list/tree items.
/// 4. [`UIExpandCollapsePattern`] — combo boxes, tree expanders.
/// 5. [`UILegacyIAccessiblePattern::do_default_action`] — broad
///    MSAA-compatible fallback that often works when the modern patterns
///    aren't exposed.
/// 6. [`UIElement::click`] — synthesized mouse click. Last resort: warps
///    the cursor and is the only thing that works for some pre-UIA Win32
///    controls. We prefer never to hit this path.
fn invoke_smart(el: &UIElement) -> anyhow::Result<InvokePath> {
    if let Ok(p) = el.get_pattern::<UIInvokePattern>() {
        if p.invoke().is_ok() {
            return Ok(InvokePath::Invoke);
        }
    }
    if let Ok(p) = el.get_pattern::<UITogglePattern>() {
        if p.toggle().is_ok() {
            return Ok(InvokePath::Toggle);
        }
    }
    if let Ok(p) = el.get_pattern::<UISelectionItemPattern>() {
        if p.select().is_ok() {
            return Ok(InvokePath::SelectionItem);
        }
    }
    if let Ok(p) = el.get_pattern::<UIExpandCollapsePattern>() {
        // Expand-only: for an already-expanded element this is a no-op on
        // most controls. Collapsing on Invoke would surprise the user (you
        // pressed a hint to *do* the thing, not to undo it).
        if p.expand().is_ok() {
            return Ok(InvokePath::ExpandCollapse);
        }
    }
    if let Ok(p) = el.get_pattern::<UILegacyIAccessiblePattern>() {
        if p.do_default_action().is_ok() {
            return Ok(InvokePath::LegacyDefaultAction);
        }
    }
    el.click().context("element.click() failed")?;
    Ok(InvokePath::MouseFallback)
}

/// Scroll a UIA element by the requested pixel delta.
///
/// UIA exposes scroll in two modes:
/// 1. Discrete amounts via [`UIScrollPattern::scroll`] — Page/Line/None,
///    matching what Page-Up / arrow keys do natively. We map our pixel
///    delta to the closest amount based on magnitude.
/// 2. Continuous percent via [`UIScrollPattern::set_scroll_percent`]
///    (0.0..=100.0). We don't use this here because we're given a pixel
///    delta, not an absolute target — translating delta → percent would
///    require querying viewport size which adds round-trips for little
///    benefit.
///
/// Choice of amount per axis:
/// - `|delta| >= 100` → page-sized scroll (large increment / decrement).
/// - `0 < |delta| < 100` → small increment / decrement (one line).
/// - `delta == 0` → `NoAmount` so the axis is skipped entirely.
///
/// Returns an error if the element doesn't support a scroll pattern at
/// all — callers can decide whether to fall back to synthesized
/// `WM_VSCROLL` (not implemented today; most modern controls expose UIA).
fn scroll_element(el: &UIElement, dx: i32, dy: i32) -> anyhow::Result<()> {
    let pattern = el
        .get_pattern::<UIScrollPattern>()
        .context("element does not support UIScrollPattern")?;

    let h = pixel_delta_to_amount(dx);
    let v = pixel_delta_to_amount(dy);
    pattern
        .scroll(h, v)
        .context("UIScrollPattern::scroll failed")?;
    tracing::debug!(dx, dy, ?h, ?v, "scrolled element via UIA");
    Ok(())
}

fn pixel_delta_to_amount(delta: i32) -> ScrollAmount {
    // Threshold chosen to match the "one mouse-wheel notch ≈ 100px"
    // convention Windows uses for `WM_MOUSEWHEEL` (`WHEEL_DELTA == 120`).
    // Anything bigger than that warrants a page-sized scroll; anything
    // smaller is a line-sized nudge.
    const PAGE_THRESHOLD: i32 = 100;
    match delta {
        0 => ScrollAmount::NoAmount,
        d if d >= PAGE_THRESHOLD => ScrollAmount::LargeIncrement,
        d if d <= -PAGE_THRESHOLD => ScrollAmount::LargeDecrement,
        d if d > 0 => ScrollAmount::SmallIncrement,
        _ => ScrollAmount::SmallDecrement,
    }
}

/// Phase-5 free walker. Walks one HWND with the supplied UIA client
/// state and returns the captured [`WalkRecord`]s. Stateless w.r.t.
/// `WindowsBackend` so this can run on a rayon worker that owns its
/// own `UIAutomation` (each thread is MTA-initialized when its
/// `UIAutomation` is constructed). The caller assigns `ElementId`s
/// and registers `UIElement`s in the backend after merging.
pub(crate) fn walk_window(
    automation: &UIAutomation,
    cache_request: &UICacheRequest,
    desktop_condition: &uiautomation::core::UICondition,
    hwnd: HWND,
    is_browser: bool,
) -> anyhow::Result<Vec<WalkRecord>> {
    let walk = tel_span!(
        "walk",
        kind = if is_browser { "browser" } else { "desktop" },
        // SAFETY: no preconditions.
        foreground = unsafe { GetForegroundWindow() } == hwnd,
        process.executable.name = tracing::field::Empty,
        visited = tracing::field::Empty,
        pre_dedup = tracing::field::Empty,
        collected = tracing::field::Empty,
        max_depth = tracing::field::Empty,
        hit_depth_cap = tracing::field::Empty,
        hit_element_cap = tracing::field::Empty,
        attempts = tracing::field::Empty,
        documents = tracing::field::Empty,
        empty_documents = tracing::field::Empty,
        tree_cached = tracing::field::Empty
    )
    .entered();
    #[cfg(feature = "telemetry")]
    if let Some(exe) = window_exe_basename(hwnd) {
        tel_record!(walk, "process.executable.name", exe.as_str());
    }
    let key = hwnd_to_key(hwnd);
    let skip_retry = is_browser && {
        let mut map = empty_walk_windows().lock();
        recently_empty(&mut map, key, Instant::now(), EMPTY_WALK_SKIP_FOR)
    };
    let handle = Handle::from(hwnd.0 as isize);
    let mut retries: u32 = 0;
    let (out, stats, tree_cached) = loop {
        let hosts_web_content = is_browser && {
            let _s = tel_span!("activate").entered();
            activate_browser_accessibility(hwnd)
        };
        // Browser windows: fetch the whole control-view tree in one UIA
        // call and walk it in memory (roadmap step 2, #41). The old
        // hop-by-hop walk made two cross-process calls per node, ~98 %
        // of a press on a big page. If the one-call fetch fails (e.g. a
        // timeout on a huge page), fall back to the hop-by-hop walk.
        let search = tel_span!("search").entered();
        let cached_tree = if is_browser {
            match fetch_browser_tree(automation, handle) {
                Ok(root) => Some(root),
                Err(e) => {
                    tracing::warn!(error = ?e,
                        "fetching the browser tree in one call failed; walking node by node");
                    None
                }
            }
        } else {
            None
        };
        let tree_cached = cached_tree.is_some();
        let root = match cached_tree {
            Some(root) => root,
            None => {
                let _s = tel_span!("root").entered();
                automation
                    .element_from_handle_build_cache(handle, cache_request)
                    .context("element_from_handle_build_cache failed")?
            }
        };

        let mut out = Vec::with_capacity(64);
        let mut stats = WalkStats {
            window_rect: cached_bounds(&root),
            ..WalkStats::default()
        };
        if tree_cached {
            walk_cached(&root, 0, &mut out, &mut stats);
        } else if is_browser {
            let walker = automation
                .get_control_view_walker()
                .context("creating control-view tree walker failed")?;
            walk_recurse(&walker, cache_request, &root, 0, true, &mut out, &mut stats);
        } else {
            find_all_desktop(
                automation,
                cache_request,
                desktop_condition,
                &root,
                &mut out,
                &mut stats,
            );
        }
        drop(search);
        if !is_browser {
            break (out, stats, tree_cached);
        }
        let unbuilt = stats.looks_unbuilt(out.len(), hosts_web_content);
        match walk_retry_delay(retries, unbuilt, skip_retry) {
            Some(delay) => {
                tracing::debug!(
                    collected = out.len(),
                    documents = stats.documents,
                    empty_documents = stats.empty_documents,
                    retries,
                    "browser page looks unbuilt; retrying the walk"
                );
                std::thread::sleep(delay);
                retries += 1;
            }
            None => {
                let mut map = empty_walk_windows().lock();
                if !unbuilt {
                    map.remove(&key);
                } else if retries > 0 {
                    // Still unbuilt after retrying: likely a genuinely
                    // empty page. Don't make every press wait again.
                    map.insert(key, Instant::now());
                }
                break (out, stats, tree_cached);
            }
        }
    };
    // Post-walk dedup: collapse nested clickables so the overlay only
    // surfaces the most-specific target per visual region. UIA loves to
    // expose both an outer ListItem/Group and the inner Button/Hyperlink
    // it wraps; without this pass the user sees stacked badges fighting
    // for the same screen real estate.
    let pre_dedup = out.len();
    let out = {
        let _s = tel_span!("dedup").entered();
        dedup_nested_records(out)
    };
    let post_dedup = out.len();
    tel_record!(walk, "visited", stats.visited as u64);
    tel_record!(walk, "pre_dedup", pre_dedup as u64);
    tel_record!(walk, "collected", post_dedup as u64);
    tel_record!(walk, "max_depth", stats.max_depth as u64);
    tel_record!(walk, "hit_depth_cap", stats.hit_depth_cap);
    tel_record!(walk, "hit_element_cap", stats.hit_element_cap);
    tel_record!(walk, "attempts", u64::from(retries) + 1);
    tel_record!(walk, "documents", stats.documents as u64);
    tel_record!(walk, "empty_documents", stats.empty_documents as u64);
    tel_record!(walk, "tree_cached", tree_cached);
    if tracing::enabled!(tracing::Level::DEBUG) {
        tracing::debug!(
            collected = post_dedup,
            dedup_dropped = pre_dedup - post_dedup,
            visited = stats.visited,
            max_depth = stats.max_depth,
            hit_depth_cap = stats.hit_depth_cap,
            hit_element_cap = stats.hit_element_cap,
            attempts = retries + 1,
            documents = stats.documents,
            empty_documents = stats.empty_documents,
            tree_cached,
            ?hwnd,
            is_browser,
            "walk_window done"
        );
    }
    #[cfg(feature = "telemetry")]
    {
        let exe = window_exe_basename(hwnd);
        let window = stats.window_rect;
        let elements: Vec<(Role, Bounds)> = out.iter().map(|r| (r.role, r.bounds)).collect();
        walk_dump::record(&walk_dump::WalkSummary {
            exe: exe.as_deref(),
            is_browser,
            // SAFETY: no preconditions.
            foreground: unsafe { GetForegroundWindow() } == hwnd,
            window,
            visited: stats.visited,
            max_depth: stats.max_depth,
            hit_element_cap: stats.hit_element_cap,
            elements: &elements,
        });
    }
    Ok(out)
}

/// Recursive control-view walker. Used for browser windows where
/// `FindAll(Subtree, …)` would over-match on DOM nodes.
fn walk_recurse(
    walker: &UITreeWalker,
    cache_request: &UICacheRequest,
    el: &UIElement,
    depth: usize,
    is_browser: bool,
    out: &mut Vec<WalkRecord>,
    stats: &mut WalkStats,
) {
    stats.visited += 1;
    if depth > stats.max_depth {
        stats.max_depth = depth;
    }
    let (max_elements, max_depth) = if is_browser {
        (MAX_ELEMENTS_BROWSER, MAX_TREE_DEPTH_BROWSER)
    } else {
        (MAX_ELEMENTS_DESKTOP, MAX_TREE_DEPTH_DESKTOP)
    };
    if out.len() >= max_elements {
        stats.hit_element_cap = true;
        return;
    }
    if let Some(record) = try_record(el, is_browser) {
        out.push(record);
    }
    let is_document =
        is_browser && el.get_cached_control_type().ok() == Some(ControlType::Document);
    if is_document {
        stats.documents += 1;
    }
    if depth >= max_depth {
        stats.hit_depth_cap = true;
        return;
    }
    // `*_build_cache` walker variants prefetch the configured
    // properties/patterns on the returned child/sibling in the same
    // IPC that produces the new node. Without this, every
    // `get_cached_*` inside `try_record` would fail because the new
    // element wasn't in the cache scope of the original
    // `element_from_handle_build_cache` call (cache scope is
    // per-element, not transitive across walker hops).
    if let Ok(first) = walker.get_first_child_build_cache(el, cache_request) {
        let mut current = first;
        loop {
            walk_recurse(
                walker,
                cache_request,
                &current,
                depth + 1,
                is_browser,
                out,
                stats,
            );
            match walker.get_next_sibling_build_cache(&current, cache_request) {
                Ok(next) => current = next,
                Err(_) => break,
            }
        }
    } else if is_document && is_on_screen(cached_bounds(el), stats.window_rect) {
        stats.empty_documents += 1;
    }
}

/// Fetch a browser window's whole control-view tree, with every
/// property and pattern the walker reads, in **one** UIA call. The
/// returned root's descendants are reachable with `get_cached_children`
/// without further cross-process calls; see [`walk_cached`].
///
/// The cache request is built per walk: that's a handful of in-process
/// COM calls, negligible next to the fetch, and it keeps `walk_window`'s
/// callers (including the parallel per-process workers) unchanged.
fn fetch_browser_tree(automation: &UIAutomation, handle: Handle) -> anyhow::Result<UIElement> {
    let req = build_cache_request(automation).context("building the tree cache request")?;
    req.set_tree_scope(uiautomation::types::TreeScope::Subtree)?;
    // Same view as the hop-by-hop walk's `get_control_view_walker`.
    req.set_tree_filter(automation.get_control_view_condition()?)?;
    let _s = tel_span!("fetch_tree").entered();
    automation
        .element_from_handle_build_cache(handle, &req)
        .context("fetching the browser tree")
}

/// [`walk_recurse`] over a tree fetched by [`fetch_browser_tree`]:
/// same limits, filters and stats, but children come from the cache
/// instead of two cross-process calls per node.
fn walk_cached(el: &UIElement, depth: usize, out: &mut Vec<WalkRecord>, stats: &mut WalkStats) {
    stats.visited += 1;
    stats.max_depth = stats.max_depth.max(depth);
    if out.len() >= MAX_ELEMENTS_BROWSER {
        stats.hit_element_cap = true;
        return;
    }
    if let Some(record) = try_record(el, true) {
        out.push(record);
    }
    let is_document = el.get_cached_control_type().ok() == Some(ControlType::Document);
    if is_document {
        stats.documents += 1;
    }
    if depth >= MAX_TREE_DEPTH_BROWSER {
        stats.hit_depth_cap = true;
        return;
    }
    let children = el.get_cached_children().unwrap_or_default();
    if children.is_empty() {
        if is_document && is_on_screen(cached_bounds(el), stats.window_rect) {
            stats.empty_documents += 1;
        }
        return;
    }
    for child in &children {
        walk_cached(child, depth + 1, out, stats);
    }
}

/// A rect with a real size inside the window. Used for childless page
/// `Document`s: an on-screen one is a tab Chromium hasn't built yet
/// (#53); off-screen or zero-size ones (hidden panels, background tabs)
/// don't count.
fn is_on_screen(rect: Option<Bounds>, window: Option<Bounds>) -> bool {
    let Some(rect) = rect else {
        return false;
    };
    rect.width > 0 && rect.height > 0 && window.map_or(true, |w| w.intersects(&rect))
}

/// A node's cached bounding rectangle as [`Bounds`].
fn cached_bounds(el: &UIElement) -> Option<Bounds> {
    let r = el.get_cached_bounding_rectangle().ok()?;
    Some(Bounds {
        x: r.get_left(),
        y: r.get_top(),
        width: r.get_width(),
        height: r.get_height(),
    })
}

/// Phase-2 desktop walker: hand the whole subtree filter to UIA via
/// `find_all_build_cache(TreeScope::Subtree, …)` and let the target
/// process do the filtering on its own UI thread.
fn find_all_desktop(
    automation: &UIAutomation,
    cache_request: &UICacheRequest,
    desktop_condition: &uiautomation::core::UICondition,
    root: &UIElement,
    out: &mut Vec<WalkRecord>,
    stats: &mut WalkStats,
) {
    match root.find_all_build_cache(
        uiautomation::types::TreeScope::Subtree,
        desktop_condition,
        cache_request,
    ) {
        Ok(elements) => {
            stats.visited = elements.len();
            for el in elements.iter() {
                if out.len() >= MAX_ELEMENTS_DESKTOP {
                    stats.hit_element_cap = true;
                    break;
                }
                if let Some(record) = try_record_desktop_after_filter(el) {
                    out.push(record);
                }
            }
        }
        Err(e) => {
            // `FindAll` over a busy subtree can time out, or fail
            // when an Electron renderer reattaches its accessibility
            // tree mid-walk. Falling back keeps the picker functional.
            tracing::warn!(error = ?e,
                "find_all_build_cache failed on desktop subtree; falling back to recursive walk");
            if let Ok(walker) = automation.get_control_view_walker() {
                walk_recurse(&walker, cache_request, root, 0, false, out, stats);
            }
        }
    }
}

/// Smallest dimension a hint can usefully label. Anything thinner than this
/// can't fit even a single-letter badge without overflowing the target,
/// and is almost always either a decorative pixel-strip (separator, focus
/// rectangle) or a UIA noise node Windows leaves behind. Dropping these up
/// front is the single biggest win for "screen full of badges" complaints.
const MIN_HINT_PX: i32 = 12;

/// Largest dimension a hint can plausibly belong to before it stops being
/// a discrete click target. Real buttons / links / inputs cap out around
/// ~1000px in practice; anything bigger is almost certainly a panel,
/// document body, or layout container that exposes an action pattern by
/// accident (e.g. an Edit covering the entire chat pane). The cap is
/// generous on purpose so 4K monitors aren't punished for genuinely big
/// affordances like full-width hero buttons.
const MAX_HINT_PX: i32 = 1500;

/// ControlTypes that masquerade as click targets in the UIA tree but are
/// usually inert containers unless they're explicitly named, focusable,
/// or expose an action pattern. Without this guard, `FindAll(Subtree, …)`
/// pulls in every unnamed ListItem in a virtualized list and every <img>
/// in a Chromium-based app, which is the dominant source of badge spam.
fn is_structural_clickable_type(ct: ControlType) -> bool {
    matches!(
        ct,
        ControlType::ListItem | ControlType::DataItem | ControlType::TreeItem | ControlType::Image
    )
}

/// Slim variant of [`try_record`] used after the server-side pre-filter
/// has already discarded offscreen / non-control elements. Adds the
/// post-Phase-7 UX guards that the broad `FindAll(Subtree, …)` condition
/// can't express server-side: minimum/maximum bounds, and a stricter
/// "must be obviously interactable" check for structural ControlTypes.
fn try_record_desktop_after_filter(el: &UIElement) -> Option<WalkRecord> {
    let bounds = el.get_cached_bounding_rectangle().ok()?;
    let w = bounds.get_width();
    let h = bounds.get_height();
    if w < MIN_HINT_PX || h < MIN_HINT_PX {
        return None;
    }
    if w > MAX_HINT_PX || h > MAX_HINT_PX {
        return None;
    }
    if !cached_bool(el, UIProperty::IsEnabled).unwrap_or(true) {
        return None;
    }
    let ct = el.get_cached_control_type().ok()?;
    if is_structural_clickable_type(ct) {
        let has_name = el
            .get_cached_name()
            .ok()
            .map(|s| !s.is_empty())
            .unwrap_or(false);
        let has_pattern = has_any_action_pattern(el);
        let focusable = cached_bool(el, UIProperty::IsKeyboardFocusable).unwrap_or(false);
        if !(has_name || has_pattern || focusable) {
            return None;
        }
    }
    try_record_desktop_element(el, ct)
}

/// Drop records that are visually redundant with another record in the
/// same window.
///
/// Two rules (applied per record):
///
///  1. **Area-ratio rule:** when a record's bounds fully contain another
///     and its area exceeds 1.5× the inner's area, the outer is a
///     container disguised as clickable — drop it, keep the inner.
///
///  2. **Single-child rule:** when a record contains *exactly one* other
///     record (regardless of area ratio), the outer is a thin wrapper —
///     drop it and keep the child. This handles toolbar rows, list items,
///     and navigation links where a container and its sole actionable
///     child would otherwise both get badges (issue #19).
///
/// The 3:2 area guard in rule 1 prevents near-equal records from
/// cannibalizing each other (e.g. a Button and its inner Text node
/// sharing bounds) — in those cases the walker emits both and the
/// layout collision logic hides the duplicate naturally. Rule 2 only
/// fires for single-child wrappers, so multi-element containers that
/// happen to share bounds are left alone.
///
/// Worst-case complexity is O(n²), but `n` is bounded by
/// [`MAX_ELEMENTS_DESKTOP`]/[`MAX_ELEMENTS_BROWSER`] so the absolute cost
/// stays well under a millisecond per window even on the busiest pages.
fn dedup_nested_records(records: Vec<WalkRecord>) -> Vec<WalkRecord> {
    let n = records.len();
    if n <= 1 {
        return records;
    }
    let areas: Vec<i64> = records
        .iter()
        .map(|r| (r.bounds.width as i64) * (r.bounds.height as i64))
        .collect();
    let mut keep = vec![true; n];
    for i in 0..n {
        if !keep[i] {
            continue;
        }
        let outer = &records[i].bounds;
        let outer_area = areas[i];
        let mut contained_count: u32 = 0;
        for j in 0..n {
            if i == j || !keep[j] {
                continue;
            }
            let inner = &records[j].bounds;
            if !outer.contains(inner) {
                continue;
            }
            contained_count += 1;
            if outer_area > areas[j].saturating_mul(3) / 2 {
                keep[i] = false;
                break;
            }
        }
        if keep[i] && contained_count == 1 {
            keep[i] = false;
        }
    }
    records
        .into_iter()
        .enumerate()
        .filter_map(|(i, r)| if keep[i] { Some(r) } else { None })
        .collect()
}

/// Decide whether to record `el`, branching on browser vs desktop.
fn try_record(el: &UIElement, is_browser: bool) -> Option<WalkRecord> {
    if cached_bool(el, UIProperty::IsOffscreen).unwrap_or(false) {
        return None;
    }
    let bounds = el.get_cached_bounding_rectangle().ok()?;
    if bounds.get_width() <= 0 || bounds.get_height() <= 0 {
        return None;
    }
    if !cached_bool(el, UIProperty::IsEnabled).unwrap_or(true) {
        return None;
    }
    let ct = el.get_cached_control_type().ok()?;
    if is_browser {
        try_record_web_element(el, ct)
    } else {
        try_record_desktop_element(el, ct)
    }
}

/// Desktop-app detection. Matches v0.2.0 behaviour for known
/// interactable ControlTypes, plus a pattern fallback for custom
/// controls (Pane / Group / Custom that nonetheless expose an action
/// pattern).
fn try_record_desktop_element(el: &UIElement, ct: ControlType) -> Option<WalkRecord> {
    if let Some(role) = map_role(ct) {
        return create_walk_record(el, role);
    }
    if has_any_action_pattern(el) {
        let bounds = el.get_cached_bounding_rectangle().ok()?;
        if bounds.get_width() >= 10 && bounds.get_height() >= 10 {
            return create_walk_record(el, Role::Other);
        }
    }
    None
}

/// Web-page / browser-content detection. Stricter than the desktop
/// path because the UIA tree mirrors the entire DOM and includes
/// piles of layout containers and decorative nodes.
///
/// Detection cascade:
///
/// 1. **High-confidence ControlTypes** (`Button`, `Hyperlink`,
///    `Edit`, …) win immediately — these are unambiguous.
/// 2. **Structural types** (`Pane`, `Group`, `Image`, `Custom`,
///    `Text`, `Document`) are only kept when they *also* expose an
///    action pattern, an ARIA-mapped MSAA role, or
///    `IsKeyboardFocusable=true`. This is the path that catches
///    `<div role="button">`, `<a>` tags Chromium classifies as
///    `Custom`, and click-handler-only divs that are still
///    keyboard-tabbable (the modern accessibility floor).
/// 3. **Anything else** — last-chance ARIA / focus heuristic so
///    the picker doesn't drop a target just because Chromium
///    picked an exotic ControlType for it.
///
/// Size filtering is split: known-interactive elements only need
/// to clear [`MIN_WEB_FOCUSABLE_SIZE`] (10px), so we still pick up
/// small icon buttons and chip "×" controls. Everything else has to
/// clear the stricter [`MIN_WEB_ELEMENT_SIZE`] (16px) — that's
/// where the spacer-element noise lives.
fn try_record_web_element(el: &UIElement, ct: ControlType) -> Option<WalkRecord> {
    use ControlType::*;

    let bounds = el.get_cached_bounding_rectangle().ok()?;
    let w = bounds.get_width();
    let h = bounds.get_height();
    // Hard floor: anything smaller than `MIN_WEB_FOCUSABLE_SIZE` is
    // almost certainly noise even for "interactive" elements. This
    // catches accessibility nodes Chromium materialises with a
    // 1×1 / 4×4 hit-box for offscreen content.
    if w < MIN_WEB_FOCUSABLE_SIZE || h < MIN_WEB_FOCUSABLE_SIZE {
        return None;
    }
    let big_enough_for_anything = w >= MIN_WEB_ELEMENT_SIZE && h >= MIN_WEB_ELEMENT_SIZE;

    // Phase 1: high-confidence ControlTypes always win — even at
    // the smaller 10px size, since something the browser explicitly
    // labelled as e.g. a Button is clearly clickable.
    let role = match ct {
        Button | SplitButton => Some(Role::Button),
        Hyperlink => Some(Role::Link),
        Edit => Some(Role::TextInput),
        CheckBox => Some(Role::Checkbox),
        RadioButton => Some(Role::Radio),
        ComboBox => Some(Role::ComboBox),
        MenuItem => Some(Role::MenuItem),
        TabItem => Some(Role::Tab),
        ListItem | TreeItem | DataItem => Some(Role::ListItem),

        // Phase 2: ambiguous / structural types need *some* signal
        // that they're interactive before we count them.
        Image | Custom | Pane | Group | Text | Document => {
            let interactive = has_any_action_pattern(el)
                || looks_clickable_web(el)
                || cached_bool(el, UIProperty::IsKeyboardFocusable).unwrap_or(false);
            if !interactive {
                None
            } else if matches!(ct, Image) {
                Some(Role::Button)
            } else {
                Some(Role::Other)
            }
        }
        _ => None,
    };

    if let Some(role) = role {
        // Big_enough_for_anything is only enforced for Phase 2 noise:
        // a real `Button` ControlType clears the smaller floor by
        // design, even if the browser sized it down.
        let needs_big = matches!(ct, Image | Custom | Pane | Group | Text | Document);
        if needs_big && !big_enough_for_anything {
            return None;
        }
        return create_walk_record(el, role);
    }

    // Phase 3: last-chance heuristic for ControlTypes we didn't
    // match above. Catches things like ARIA `role="button"` exposed
    // only via the legacy IAccessible bridge, or pure-JS clickable
    // elements that show up under exotic ControlTypes but are
    // tabbable through keyboard navigation.
    if big_enough_for_anything
        && (looks_clickable_web(el)
            || cached_bool(el, UIProperty::IsKeyboardFocusable).unwrap_or(false))
    {
        return create_walk_record(el, Role::Button);
    }

    None
}

/// Build a [`WalkRecord`] for a recorded UIA element. ID assignment
/// and registration in [`WindowsBackend::elements`] happen later, on
/// the merging thread; this function only does in-process reads
/// (cached bounding rect + name) so it is safe to call from any
/// MTA-initialized worker.
fn create_walk_record(el: &UIElement, role: Role) -> Option<WalkRecord> {
    let bounds = el.get_cached_bounding_rectangle().ok()?;
    let name = el.get_cached_name().ok().filter(|s| !s.is_empty());
    Some(WalkRecord {
        role,
        name,
        bounds: Bounds {
            x: bounds.get_left(),
            y: bounds.get_top(),
            width: bounds.get_width(),
            height: bounds.get_height(),
        },
        element: el.clone(),
    })
}

/// True when `el` advertises any of the patterns we know how to invoke.
/// Used by the desktop pattern-fallback path to catch custom controls
/// whose ControlType doesn't tell us they're interactable.
fn has_any_action_pattern(el: &UIElement) -> bool {
    // `get_cached_pattern` is the in-process counterpart of `get_pattern`:
    // it succeeds iff the cache request enabled the pattern *and* the
    // remote element actually advertised it. The four patterns here must
    // stay in lock-step with the `add_pattern` calls in
    // `build_cache_request` — otherwise this function silently returns
    // `false` for everything and the desktop pattern-fallback path goes
    // dark.
    el.get_cached_pattern::<UIInvokePattern>().is_ok()
        || el.get_cached_pattern::<UITogglePattern>().is_ok()
        || el.get_cached_pattern::<UISelectionItemPattern>().is_ok()
        || el.get_cached_pattern::<UIExpandCollapsePattern>().is_ok()
}

/// Heuristic for "this looks clickable even though no UIA pattern says so".
///
/// Web pages with ARIA `role="button"` / `role="link"` typically end up
/// exposing the role through the legacy IAccessible bridge but don't
/// publish an InvokePattern. Without this check those elements would be
/// invisible to the picker even though they're the entire reason a user
/// hits the hotkey on a web page.
///
/// We also pick up the broader interactive-but-stateful set (tabs,
/// tree items, grid cells, dropdown buttons, comboboxes) that
/// frequently show up only via the legacy bridge in modern web apps
/// (Gmail's row toggles, GitHub's file tree, dashboard grids, …).
fn looks_clickable_web(el: &UIElement) -> bool {
    let Ok(p) = el.get_cached_pattern::<UILegacyIAccessiblePattern>() else {
        return false;
    };
    let Ok(role) = p.get_cached_role() else {
        return false;
    };
    matches!(
        role as i32,
        MSAA_ROLE_LINK
            | MSAA_ROLE_PUSHBUTTON
            | MSAA_ROLE_CHECKBUTTON
            | MSAA_ROLE_RADIOBUTTON
            | MSAA_ROLE_MENU_ITEM
            | MSAA_ROLE_PAGE_TAB
            | MSAA_ROLE_OUTLINE_ITEM
            | MSAA_ROLE_LIST_ITEM
            | MSAA_ROLE_CELL
            | MSAA_ROLE_COMBOBOX
            | MSAA_ROLE_BUTTON_DROPDOWN
            | MSAA_ROLE_BUTTON_MENU
    )
}

fn map_role(ct: ControlType) -> Option<Role> {
    use ControlType::*;
    Some(match ct {
        // Canonical interactive controls — these always count.
        Button | SplitButton => Role::Button,
        Hyperlink => Role::Link,
        Edit => Role::TextInput,
        MenuItem => Role::MenuItem,
        TabItem => Role::Tab,
        CheckBox => Role::Checkbox,
        RadioButton => Role::Radio,
        ComboBox => Role::ComboBox,
        ListItem => Role::ListItem,
        TreeItem => Role::TreeItem,

        // Often-clickable controls in modern Win32 / WinUI / Electron
        // apps. Including them by ControlType (rather than via the
        // pattern fallback) means we catch them even when their app
        // forgets to publish InvokePattern.
        DataItem => Role::ListItem,
        Image => Role::Button,

        // Structural / non-interactable controls (Pane, Group, Text,
        // Document, Window, TitleBar, …) are skipped here on purpose —
        // they're handled by the pattern fallback in `try_record_desktop_element`,
        // which only records them when they actually expose an action.
        _ => return None,
    })
}

/// True iff `hwnd` belongs to a process whose executable name looks
/// like a known browser. Cheap (one process-handle open + one image-name
/// query) and runs once per overlay invocation.
///
/// We match on substrings of the lower-cased exe filename. Misses:
///  - Browsers shipped under exotic names (Yandex, Vivaldi, etc.) — easy
///    to add when reported.
///  - Electron apps that *embed* Chromium (VS Code, Discord, …) — these
///    are explicitly *not* browsers for our purposes; their chrome is
///    native and the web-strict filter would over-prune their controls.
fn is_browser_window(hwnd: HWND) -> bool {
    let Some(basename) = window_exe_basename(hwnd) else {
        return false;
    };
    matches!(
        basename.as_str(),
        "chrome.exe"
            | "msedge.exe"
            | "firefox.exe"
            | "brave.exe"
            | "opera.exe"
            | "vivaldi.exe"
            | "iexplore.exe"
            | "arc.exe"
    )
}

/// Lowercased file name of the exe owning `hwnd` (`vivaldi.exe`), or
/// `None` if the process can't be queried.
fn window_exe_basename(hwnd: HWND) -> Option<String> {
    let mut process_id: u32 = 0;
    // SAFETY: `GetWindowThreadProcessId` writes through `&mut process_id`
    // and tolerates a null `hwnd` (returns 0). We guarantee `process_id`
    // is a valid stack location for the duration of the call.
    unsafe {
        let _ = GetWindowThreadProcessId(hwnd, Some(&mut process_id as *mut u32));
    }
    if process_id == 0 {
        return None;
    }

    let exe_path = unsafe {
        let h_process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id).ok()?;
        let mut buf = vec![0u16; 1024];
        let mut size: u32 = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(
            h_process,
            PROCESS_NAME_FORMAT(0),
            PWSTR(buf.as_mut_ptr()),
            &mut size,
        );
        let _ = CloseHandle(h_process);
        if ok.is_err() || size == 0 {
            return None;
        }
        String::from_utf16_lossy(&buf[..size as usize]).to_lowercase()
    };

    // Basename only — full paths add noise (e.g. `chrome` is
    // legitimately part of `\chrome\` directories for non-browser apps).
    let basename = exe_path.rsplit(['\\', '/']).next().unwrap_or(&exe_path);
    Some(basename.to_string())
}

/// MSAA object id for the UI Automation root, returned by Chromium's
/// `WM_GETOBJECT` handler. Defined as `-25` in `UIAutomationCore.h`.
const UIA_ROOT_OBJECT_ID: i32 = -25;
/// MSAA object id for the standard `IAccessible` root. Documented at
/// <https://learn.microsoft.com/en-us/windows/win32/winauto/object-identifiers>.
const OBJID_CLIENT: i32 = -4;
/// Wait at most this long for a single `WM_GETOBJECT` round trip when
/// activating browser accessibility. Browser UI threads can occasionally
/// stall (loading a giant page, devtools open, …); a short timeout means
/// our hotkey path stays snappy even when the browser doesn't reply.
const ACCESSIBILITY_ACTIVATION_TIMEOUT_MS: u32 = 50;

/// Nudge a Chromium-based browser into building its full per-tab
/// accessibility tree.
///
/// Chrome / Edge / Brave / Opera / Vivaldi / Arc all share Chromium's
/// `BrowserAccessibilityState`, which only enables the renderer-side
/// accessibility tree on demand. Without this, `EnumWindows`-style
/// walks see the browser chrome but the per-tab `Document` element
/// is a single placeholder with no children — exactly the symptom the
/// user reported (badges on tabs / address bar but never on the page).
///
/// The activation channel is `WM_GETOBJECT`. Sending it with
/// [`UIA_ROOT_OBJECT_ID`] (`-25`) tells Chromium "a UIA client is about
/// to query you", which sets the right accessibility mode flags.
/// We send to every descendant HWND, not just the top-level browser
/// window, because each tab's renderer lives in its own
/// `Chrome_RenderWidgetHostHWND` child window.
///
/// We also send [`OBJID_CLIENT`] as a belt-and-braces signal — Firefox
/// and IE classic respond to that path, while modern Chromium prefers
/// the UIA root id. The message is fire-and-forget; we ignore the
/// returned IAccessible pointer.
///
/// Returns whether the window hosts web content, i.e. has at least one
/// `Chrome_RenderWidgetHostHWND` child. Browser dialogs (Vivaldi's own
/// prompts, the Save As box) don't, and never contain a `Document`, so
/// the "no `Document` yet" retry rule (#53) only applies when this is
/// true.
fn activate_browser_accessibility(hwnd: HWND) -> bool {
    let mut renderers: usize = 0;
    unsafe {
        send_accessibility_probe(hwnd);
        // Walk the child HWNDs too — each renderer process surfaces
        // the page document via its own child window.
        let _ = EnumChildWindows(
            hwnd,
            Some(activation_enum_proc),
            LPARAM(&mut renderers as *mut usize as isize),
        );
    }
    renderers > 0
}

/// Window class of a Chromium renderer's child window.
const CHROME_RENDERER_CLASS: &str = "Chrome_RenderWidgetHostHWND";

unsafe extern "system" fn activation_enum_proc(
    hwnd: HWND,
    lparam: LPARAM,
) -> windows::Win32::Foundation::BOOL {
    send_accessibility_probe(hwnd);
    let mut buf = [0u16; 64];
    let len = GetClassNameW(hwnd, &mut buf);
    if len > 0 && String::from_utf16_lossy(&buf[..len as usize]) == CHROME_RENDERER_CLASS {
        // SAFETY: `lparam` is the `&mut usize` passed by
        // `activate_browser_accessibility`, alive for the whole
        // synchronous `EnumChildWindows` call.
        *(lparam.0 as *mut usize) += 1;
    }
    windows::Win32::Foundation::BOOL(1)
}

/// Send the two `WM_GETOBJECT` flavors browsers care about, with a
/// short timeout so a wedged target window can't stall the picker.
unsafe fn send_accessibility_probe(hwnd: HWND) {
    for object_id in [UIA_ROOT_OBJECT_ID, OBJID_CLIENT] {
        let _ = SendMessageTimeoutW(
            hwnd,
            WM_GETOBJECT,
            WPARAM(0),
            LPARAM(object_id as isize),
            SMTO_ABORTIFHUNG,
            ACCESSIBILITY_ACTIVATION_TIMEOUT_MS,
            None,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- empty first press (#53) ---------------------------------------

    fn stats(documents: usize, empty_documents: usize) -> WalkStats {
        WalkStats {
            documents,
            empty_documents,
            ..WalkStats::default()
        }
    }

    #[test]
    fn a_walk_that_found_nothing_looks_unbuilt() {
        assert!(stats(2, 0).looks_unbuilt(0, true));
        assert!(stats(0, 0).looks_unbuilt(0, false));
    }

    #[test]
    fn a_web_window_without_any_document_looks_unbuilt() {
        assert!(stats(0, 0).looks_unbuilt(3, true));
    }

    #[test]
    fn a_browser_dialog_without_a_document_is_fine() {
        // Seen live: a Vivaldi prompt (checkbox + two buttons) has no
        // renderer child and no Document; it must not be retried.
        assert!(!stats(0, 0).looks_unbuilt(3, false));
    }

    #[test]
    fn an_empty_page_document_looks_unbuilt_even_with_chrome_labels() {
        // Browser UI (tabs, address bar) found, but the page isn't built.
        assert!(stats(2, 1).looks_unbuilt(12, true));
    }

    #[test]
    fn a_built_page_does_not_look_unbuilt() {
        assert!(!stats(2, 0).looks_unbuilt(40, true));
    }

    #[test]
    fn retries_an_unbuilt_walk() {
        assert_eq!(
            walk_retry_delay(0, true, false),
            Some(EMPTY_WALK_RETRY_DELAY)
        );
        assert_eq!(walk_retry_delay(0, false, false), None);
    }

    #[test]
    fn stops_after_the_last_retry() {
        assert!(walk_retry_delay(EMPTY_WALK_MAX_RETRIES - 1, true, false).is_some());
        assert_eq!(walk_retry_delay(EMPTY_WALK_MAX_RETRIES, true, false), None);
    }

    #[test]
    fn skips_retrying_a_window_that_recently_stayed_unbuilt() {
        assert_eq!(walk_retry_delay(0, true, true), None);
    }

    #[test]
    fn only_real_rects_inside_the_window_are_on_screen() {
        let window = Some(bnd(0, 0, 1600, 1000));
        assert!(is_on_screen(Some(bnd(10, 80, 1580, 900)), window));
        assert!(!is_on_screen(Some(bnd(10, 80, 0, 900)), window));
        assert!(!is_on_screen(Some(bnd(2000, 80, 400, 400)), window));
        assert!(!is_on_screen(None, window));
        assert!(is_on_screen(Some(bnd(10, 80, 100, 100)), None));
    }

    #[test]
    fn recently_empty_windows_expire() {
        let key = hwnd_to_key(hwnd(7));
        let other = hwnd_to_key(hwnd(8));
        let t0 = Instant::now();
        let skip = Duration::from_secs(30);
        let mut map = HashMap::new();
        map.insert(key, t0);
        assert!(recently_empty(
            &mut map,
            key,
            t0 + Duration::from_secs(29),
            skip
        ));
        assert!(!recently_empty(&mut map, other, t0, skip));
        assert!(!recently_empty(&mut map, key, t0 + skip, skip));
        assert!(map.is_empty(), "expired entries are forgotten");
    }

    // ---- plan_windows -------------------------------------------------

    use crate::windows::window_picker::TopLevelWindow;

    fn bnd(x: i32, y: i32, width: i32, height: i32) -> Bounds {
        Bounds {
            x,
            y,
            width,
            height,
        }
    }

    fn hwnd(n: usize) -> HWND {
        HWND(n as *mut std::ffi::c_void)
    }

    fn win(n: usize, bounds: Bounds) -> TopLevelWindow {
        TopLevelWindow {
            hwnd: hwnd(n),
            title: format!("win{n}"),
            bounds,
            is_minimized: false,
        }
    }

    fn planned_ids(planned: &[PlannedWindow]) -> Vec<usize> {
        planned.iter().map(|p| p.win.hwnd.0 as usize).collect()
    }

    const SCREEN: Bounds = Bounds {
        x: 0,
        y: 0,
        width: 1920,
        height: 1080,
    };

    #[test]
    fn plan_small_over_maximized_keeps_both_with_occluder() {
        let small = bnd(400, 300, 800, 500);
        let planned = plan_windows(vec![win(1, small), win(2, SCREEN)], hwnd(1), |_| true);
        assert_eq!(planned_ids(&planned), vec![1, 2]);
        assert!(planned[0].occluders.is_empty());
        assert_eq!(planned[1].occluders, vec![small]);
    }

    #[test]
    fn plan_skips_window_hidden_by_union() {
        let left = bnd(0, 0, 960, 1080);
        let right = bnd(960, 0, 960, 1080);
        let under = bnd(500, 200, 900, 600);
        let planned = plan_windows(
            vec![win(1, left), win(2, right), win(3, under)],
            hwnd(1),
            |_| true,
        );
        assert_eq!(planned_ids(&planned), vec![1, 2]);
    }

    #[test]
    fn plan_never_skips_foreground() {
        let planned = plan_windows(
            vec![win(1, SCREEN), win(2, bnd(100, 100, 200, 200))],
            hwnd(2),
            |_| true,
        );
        assert_eq!(planned_ids(&planned), vec![1, 2]);
    }

    #[test]
    fn plan_ignores_minimized_windows() {
        let mut minimized = win(1, bnd(0, 0, 200, 40));
        minimized.is_minimized = true;
        let planned = plan_windows(vec![minimized, win(2, SCREEN)], hwnd(2), |_| true);
        assert_eq!(planned_ids(&planned), vec![2]);
        assert!(planned[0].occluders.is_empty());
    }

    #[test]
    fn plan_rejected_window_still_occludes() {
        // Window 1 lives mostly on a monitor to the left and is rejected
        // by the scope filter, but its right edge covers part of window 2.
        let straddler = bnd(-1800, 100, 2000, 600);
        let on_monitor = bnd(0, 0, 1920, 1080);
        let planned = plan_windows(vec![win(1, straddler), win(2, on_monitor)], hwnd(2), |b| {
            b.x >= 0
        });
        assert_eq!(planned_ids(&planned), vec![2]);
        assert_eq!(planned[0].occluders, vec![straddler]);
    }

    #[test]
    fn plan_occluders_only_include_overlapping_windows() {
        let far = bnd(3000, 0, 500, 500);
        let near = bnd(100, 100, 300, 300);
        let planned = plan_windows(
            vec![win(1, far), win(2, near), win(3, SCREEN)],
            hwnd(2),
            |_| true,
        );
        assert_eq!(planned[2].occluders, vec![near]);
    }

    #[test]
    fn retain_unoccluded_drops_hidden_elements() {
        let w = win(1, SCREEN);
        let occ = [bnd(400, 300, 800, 500)];
        let items = vec![
            bnd(500, 400, 50, 20),  // fully under the occluder
            bnd(1180, 400, 50, 20), // straddles the occluder's right edge
            bnd(10, 10, 50, 20),    // clear
        ];
        let kept = retain_unoccluded(items, &occ, |b| b, &w);
        assert_eq!(kept, vec![bnd(1180, 400, 50, 20), bnd(10, 10, 50, 20)]);
    }

    fn rect(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
        RECT {
            left,
            top,
            right,
            bottom,
        }
    }

    const PRIMARY: RECT = RECT {
        left: 0,
        top: 0,
        right: 1920,
        bottom: 1080,
    };
    const SECONDARY: RECT = RECT {
        left: 1920,
        top: 0,
        right: 4480,
        bottom: 1440,
    };

    #[test]
    fn active_monitor_prefers_foreground_monitor() {
        assert_eq!(
            resolve_active_monitor(Some(SECONDARY), false, Some(PRIMARY)),
            Some(SECONDARY)
        );
    }

    #[test]
    fn active_monitor_uses_cursor_when_shell_focused() {
        assert_eq!(
            resolve_active_monitor(Some(PRIMARY), true, Some(SECONDARY)),
            Some(SECONDARY)
        );
        assert_eq!(
            resolve_active_monitor(None, true, Some(SECONDARY)),
            Some(SECONDARY)
        );
    }

    #[test]
    fn active_monitor_uses_cursor_without_foreground() {
        assert_eq!(
            resolve_active_monitor(None, false, Some(PRIMARY)),
            Some(PRIMARY)
        );
    }

    #[test]
    fn active_monitor_none_when_nothing_resolves() {
        assert_eq!(resolve_active_monitor(None, false, None), None);
        assert_eq!(resolve_active_monitor(Some(PRIMARY), true, None), None);
    }

    #[test]
    fn anchor_keeps_point_inside_rect() {
        assert_eq!(anchor_in_or_center((2000, 100), &SECONDARY), (2000, 100));
        // Left/top edges are inclusive.
        assert_eq!(anchor_in_or_center((1920, 0), &SECONDARY), (1920, 0));
    }

    #[test]
    fn anchor_falls_back_to_center_outside_rect() {
        assert_eq!(anchor_in_or_center((500, 500), &SECONDARY), (3200, 720));
        // Right/bottom edges are exclusive (they belong to the next monitor).
        assert_eq!(anchor_in_or_center((1920, 500), &PRIMARY), (960, 540));
        assert_eq!(
            anchor_in_or_center((-10, -10), &rect(-1920, 0, 0, 1080)),
            (-960, 540)
        );
    }
}
