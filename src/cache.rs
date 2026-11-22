//! Time-bounded cache of accessibility-tree enumeration results.
//!
//! UIA tree walks are by far the most expensive thing jumpkey does on a
//! single hotkey press — for content-rich pages (Gmail, GitHub, large
//! dashboards) we routinely descend 25+ levels and visit a few thousand
//! nodes. When the user fires the hotkey, dismisses with `Esc`, and
//! immediately tries again — a common "I missed the right hint" flow —
//! recomputing the entire tree is pure waste.
//!
//! [`CacheManager`] memoises [`crate::Element`] vectors keyed by an
//! opaque `WindowKey` (the platform backend converts its native handle
//! into a `u64` so this module stays cross-platform). Entries expire
//! either by **age** (`cache_ttl_ms`) or by an **explicit invalidation
//! signal** the backend posts when it knows the underlying window
//! changed (resized, scrolled, lost focus, …).
//!
//! The cache is intentionally simple — just a `HashMap` plus a `Clock`
//! abstraction so unit tests don't need to call `Instant::now`. We
//! never spawn a sweeper thread; stale entries get evicted when the
//! same key is asked for again or when [`CacheManager::sweep`] is
//! called manually (the backend may do this on focus change to keep
//! the map from accumulating long-dead HWNDs).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::Element;

/// Opaque per-window cache key. The Windows backend stuffs `HWND` into
/// a `u64`; future platforms (X11 `Window`, Wayland surface ID, AT-SPI
/// path hash, …) can do likewise. Using a dumb integer here keeps this
/// module free of `cfg`-gated platform types.
pub type WindowKey = u64;

/// Identifies one generation of element IDs: the ID table a backend
/// had when it minted a cache entry's [`crate::ElementId`]s.
///
/// IDs are only meaningful to the backend (and the table) that created
/// them. Two backends share this cache (the hotkey path and the
/// cache-warming thread), and a backend can reset its table, after
/// which old ID numbers are reused for different elements. A backend
/// takes a fresh `Generation` whenever it starts a new ID table and
/// stores it with every entry it inserts; [`CacheManager::lookup`] only
/// returns entries from the caller's current generation. Generations
/// are unique across the whole process, so one comparison covers both
/// "another backend made this" and "made before my last reset".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Generation(u64);

impl Generation {
    /// A generation no other caller has ever received.
    pub fn next() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

/// Result of [`CacheManager::lookup`].
#[derive(Debug)]
pub enum Lookup {
    /// A fresh entry from the caller's generation; its IDs are valid
    /// for the caller.
    Hit(Arc<[Element]>),
    /// A fresh entry exists, but its IDs belong to another generation
    /// (another backend, or before the caller reset its ID table).
    /// The caller must search again; the entry is left in place.
    OtherGeneration,
    /// No usable entry: none cached, expired, or caching disabled.
    Miss,
}

/// Pluggable monotonic clock so unit tests don't have to sleep.
///
/// The default impl is [`SystemClock`] which delegates to
/// [`Instant::now`]. Tests inject [`MockClock`] instead.
pub trait Clock: Send + Sync + 'static {
    /// Return the current "now" tick. Only the *delta* between two
    /// calls matters; absolute values are not exposed.
    fn now(&self) -> Instant;
}

/// Production clock. One per `CacheManager`; ZST so creating a
/// manager doesn't allocate.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// Single cache entry: the enumerated elements plus when they were
/// captured. Held by-value inside the `HashMap` — we hand callers a
/// refcount-bumped `Arc<[Element]>` rather than allocating + copying
/// the vector on every hit. With Phase 4 of the perf plan the same
/// slice is shared between the synchronous hotkey path and the
/// pre-warm worker; using `Arc` means neither side has to clone the
/// payload to pass it across the (`parking_lot::Mutex`-guarded) cache.
#[derive(Debug, Clone)]
pub struct CacheEntry {
    /// Enumerated elements as a shared, immutable slice. `Arc::clone`
    /// is a single atomic increment — independent of element count —
    /// which matters for browser windows where the slice can hold
    /// thousands of entries.
    pub elements: Arc<[Element]>,
    /// When this entry was inserted, used to compute age against the
    /// configured TTL.
    pub captured_at: Instant,
    /// The ID generation the elements' IDs belong to.
    pub generation: Generation,
}

/// In-memory cache of [`CacheEntry`]s keyed by [`WindowKey`].
///
/// Not thread-safe by design: the [`crate::Backend`] trait is `&mut
/// self` so the backend (and therefore its cache) only exists on one
/// thread at a time. Adding a `Mutex` here would just be tax with no
/// benefit.
pub struct CacheManager<C: Clock = SystemClock> {
    entries: HashMap<WindowKey, CacheEntry>,
    ttl: Duration,
    /// `false` disables the cache entirely — every `get` returns
    /// `None` and `insert` is a no-op. Lets users toggle caching from
    /// Settings without restarting.
    enabled: bool,
    clock: C,
}

impl CacheManager<SystemClock> {
    /// Build a cache that uses the real [`Instant::now`] clock.
    pub fn new(ttl_ms: u64, enabled: bool) -> Self {
        Self::with_clock(ttl_ms, enabled, SystemClock)
    }
}

impl<C: Clock> CacheManager<C> {
    /// Construct a cache with an explicit clock implementation. Tests
    /// pass [`MockClock`]; production calls [`Self::new`].
    pub fn with_clock(ttl_ms: u64, enabled: bool, clock: C) -> Self {
        Self {
            entries: HashMap::new(),
            ttl: Duration::from_millis(ttl_ms),
            enabled,
            clock,
        }
    }

    /// Update runtime knobs without losing existing entries (the
    /// Settings dialog can flip caching on/off mid-session).
    pub fn reconfigure(&mut self, ttl_ms: u64, enabled: bool) {
        self.ttl = Duration::from_millis(ttl_ms);
        self.enabled = enabled;
        if !enabled {
            self.entries.clear();
        }
    }

    /// Look up `key` for a caller whose current ID generation is
    /// `generation`. Returns [`Lookup::Hit`] only for an entry younger
    /// than `ttl_ms` *and* from that generation, so the caller can use
    /// its IDs directly. Stale entries are removed in passing. A hit's
    /// `Arc` is a refcount bump: no allocation, no element copies.
    pub fn lookup(&mut self, key: WindowKey, generation: Generation) -> Lookup {
        if !self.enabled {
            return Lookup::Miss;
        }
        let now = self.clock.now();
        match self.entries.get(&key) {
            Some(entry) if now.duration_since(entry.captured_at) <= self.ttl => {
                let age_ms = now.duration_since(entry.captured_at).as_millis() as u64;
                if entry.generation == generation {
                    tracing::debug!(key, age_ms, "cache hit");
                    Lookup::Hit(Arc::clone(&entry.elements))
                } else {
                    tracing::debug!(key, age_ms, "cache entry from another ID generation");
                    Lookup::OtherGeneration
                }
            }
            Some(_) => {
                tracing::debug!(key, "cache entry expired");
                self.entries.remove(&key);
                Lookup::Miss
            }
            None => Lookup::Miss,
        }
    }

    /// True when `key` has an entry younger than `ttl_ms`, from any
    /// generation. The cache-warming thread uses this to skip windows
    /// that are already cached, so it doesn't replace the hotkey
    /// path's own (usable) entry with one the hotkey path can't use.
    /// Stale entries are removed in passing.
    pub fn has_fresh(&mut self, key: WindowKey) -> bool {
        if !self.enabled {
            return false;
        }
        let now = self.clock.now();
        match self.entries.get(&key) {
            Some(entry) if now.duration_since(entry.captured_at) <= self.ttl => true,
            Some(_) => {
                self.entries.remove(&key);
                false
            }
            None => false,
        }
    }

    /// Insert a fresh enumeration. Replaces any existing entry for
    /// the same key. No-op when caching is disabled — the backend
    /// always calls `insert` after a successful walk so it doesn't
    /// have to branch on `enabled` at every call site.
    ///
    /// The slice is moved (not cloned) into the entry; the same
    /// `Arc` is then handed back from every subsequent
    /// [`Self::lookup`] until the entry expires. `generation` must be
    /// the inserting backend's current ID generation.
    pub fn insert(&mut self, key: WindowKey, elements: Arc<[Element]>, generation: Generation) {
        if !self.enabled {
            return;
        }
        let captured_at = self.clock.now();
        self.entries.insert(
            key,
            CacheEntry {
                elements,
                captured_at,
                generation,
            },
        );
    }

    /// Drop the entry for `key`. Used when the backend has positive
    /// evidence the cache is stale (window moved/resized, scroll
    /// happened, focus left and came back). Idempotent — invalidating
    /// a key that isn't cached is fine.
    pub fn invalidate(&mut self, key: WindowKey) {
        if self.entries.remove(&key).is_some() {
            tracing::debug!(key, "cache invalidated");
        }
    }

    /// Drop every entry. Cheap — just `HashMap::clear`. Called by
    /// the backend on focus change so we don't accumulate entries
    /// for HWNDs the user may never visit again.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Drop all entries older than `ttl_ms`. Optional housekeeping —
    /// `get` already evicts stale entries lazily, but calling
    /// `sweep` on focus change keeps the map small.
    pub fn sweep(&mut self) {
        let now = self.clock.now();
        let ttl = self.ttl;
        self.entries
            .retain(|_, entry| now.duration_since(entry.captured_at) <= ttl);
    }

    /// Number of entries currently held (incl. stale ones not yet
    /// evicted). Exposed for diagnostics / tests.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True iff the cache holds zero entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// True when caching is currently turned on.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }
}

#[cfg(test)]
pub(crate) mod test_clock {
    //! Mockable clock for deterministic cache TTL tests. Public to the
    //! crate (not the world) so the integration tests in this module
    //! and any later ones can drive time without sleeping.

    use std::cell::Cell;
    use std::time::{Duration, Instant};

    use super::Clock;

    pub struct MockClock {
        // Cell so `now(&self)` can mutate without `&mut`.
        // We anchor against a real Instant snapshot so
        // `duration_since` semantics match production.
        anchor: Instant,
        offset: Cell<Duration>,
    }

    impl MockClock {
        pub fn new() -> Self {
            Self {
                anchor: Instant::now(),
                offset: Cell::new(Duration::ZERO),
            }
        }

        pub fn advance(&self, by: Duration) {
            self.offset.set(self.offset.get() + by);
        }
    }

    // SAFETY: MockClock holds only Send+Sync data (Instant, Cell<Duration>)
    // but Cell is !Sync. The cache is single-threaded by construction so
    // we hand-implement Send/Sync to satisfy the trait bound. Production
    // code never touches MockClock.
    unsafe impl Send for MockClock {}
    unsafe impl Sync for MockClock {}

    impl Clock for MockClock {
        fn now(&self) -> Instant {
            self.anchor + self.offset.get()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_clock::MockClock;
    use super::*;
    use crate::{Bounds, ElementId, Role};

    fn fake_element(id: u64) -> Element {
        Element {
            id: ElementId(id),
            role: Role::Button,
            name: Some(format!("el {id}")),
            bounds: Bounds {
                x: 0,
                y: 0,
                width: 10,
                height: 10,
            },
        }
    }

    fn cache(ttl_ms: u64) -> CacheManager<MockClock> {
        CacheManager::with_clock(ttl_ms, true, MockClock::new())
    }

    thread_local! {
        /// The generation the single-generation tests insert and look
        /// up with.
        static GEN: Generation = Generation::next();
    }

    fn generation() -> Generation {
        GEN.with(|g| *g)
    }

    /// Single-generation helpers so the TTL tests read as before.
    trait SameGeneration {
        fn put(&mut self, key: WindowKey, elements: Arc<[Element]>);
        fn get(&mut self, key: WindowKey) -> Option<Arc<[Element]>>;
    }

    impl SameGeneration for CacheManager<MockClock> {
        fn put(&mut self, key: WindowKey, elements: Arc<[Element]>) {
            self.insert(key, elements, generation());
        }
        fn get(&mut self, key: WindowKey) -> Option<Arc<[Element]>> {
            match self.lookup(key, generation()) {
                Lookup::Hit(e) => Some(e),
                Lookup::OtherGeneration => panic!("unexpected other generation"),
                Lookup::Miss => None,
            }
        }
    }

    #[test]
    fn empty_cache_returns_none() {
        let mut c = cache(500);
        assert!(c.get(42).is_none());
    }

    fn arc_of(elements: Vec<Element>) -> Arc<[Element]> {
        Arc::from(elements.into_boxed_slice())
    }

    #[test]
    fn insert_then_get_round_trips() {
        let mut c = cache(500);
        c.put(1, arc_of(vec![fake_element(1), fake_element(2)]));
        let got = c.get(1).expect("fresh entry should hit");
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn entry_expires_after_ttl() {
        let mut c = cache(100);
        c.put(1, arc_of(vec![fake_element(1)]));
        c.clock.advance(Duration::from_millis(101));
        assert!(c.get(1).is_none(), "entry past TTL must miss");
    }

    #[test]
    fn entry_within_ttl_hits() {
        let mut c = cache(500);
        c.put(1, arc_of(vec![fake_element(1)]));
        c.clock.advance(Duration::from_millis(499));
        assert!(c.get(1).is_some(), "entry inside TTL must hit");
    }

    #[test]
    fn invalidate_drops_entry() {
        let mut c = cache(500);
        c.put(1, arc_of(vec![fake_element(1)]));
        c.invalidate(1);
        assert!(c.get(1).is_none());
    }

    #[test]
    fn invalidate_unknown_key_is_noop() {
        let mut c = cache(500);
        c.invalidate(999);
        assert_eq!(c.len(), 0);
    }

    #[test]
    fn clear_empties_cache() {
        let mut c = cache(500);
        c.put(1, arc_of(vec![fake_element(1)]));
        c.put(2, arc_of(vec![fake_element(2)]));
        c.clear();
        assert!(c.is_empty());
    }

    #[test]
    fn disabled_cache_misses_everything() {
        let mut c: CacheManager<MockClock> = CacheManager::with_clock(500, false, MockClock::new());
        c.put(1, arc_of(vec![fake_element(1)]));
        assert!(c.get(1).is_none());
        assert_eq!(c.len(), 0, "insert should be a no-op when disabled");
    }

    #[test]
    fn reconfigure_disabling_clears_entries() {
        let mut c = cache(500);
        c.put(1, arc_of(vec![fake_element(1)]));
        c.reconfigure(500, false);
        assert!(c.is_empty());
        assert!(!c.is_enabled());
    }

    #[test]
    fn sweep_evicts_stale_keeps_fresh() {
        let mut c = cache(100);
        c.put(1, arc_of(vec![fake_element(1)]));
        c.clock.advance(Duration::from_millis(150));
        c.put(2, arc_of(vec![fake_element(2)]));
        c.sweep();
        assert!(c.get(1).is_none(), "stale entry should be swept");
        assert!(c.get(2).is_some(), "fresh entry should survive sweep");
    }

    #[test]
    fn get_evicts_stale_lazily() {
        let mut c = cache(100);
        c.put(1, arc_of(vec![fake_element(1)]));
        c.clock.advance(Duration::from_millis(150));
        let _ = c.get(1);
        assert_eq!(c.len(), 0, "stale entry should be evicted by get()");
    }

    #[test]
    fn cache_hit_is_refcount_bump_not_clone() {
        let mut c = cache(500);
        let original = arc_of(vec![fake_element(1), fake_element(2)]);
        c.put(1, Arc::clone(&original));
        let got = c.get(1).expect("fresh entry should hit");
        assert!(
            Arc::ptr_eq(&got, &original),
            "cache hit must hand back the same allocation"
        );
    }

    #[test]
    fn entry_from_another_generation_is_rejected() {
        // The cache-warming thread's backend inserts with its own
        // generation; the hotkey backend must not accept those IDs.
        let mut c = cache(500);
        let warmer = Generation::next();
        c.insert(1, arc_of(vec![fake_element(0)]), warmer);
        assert!(matches!(c.lookup(1, generation()), Lookup::OtherGeneration));
        // Left in place for its owner.
        assert!(matches!(c.lookup(1, warmer), Lookup::Hit(_)));
    }

    #[test]
    fn entry_from_before_a_reset_is_rejected() {
        // A backend that resets its ID table reuses ID numbers for
        // different elements, so its own older entries become invalid.
        let mut c = cache(500);
        let before = Generation::next();
        c.insert(1, arc_of(vec![fake_element(0)]), before);
        let after = Generation::next();
        assert!(matches!(c.lookup(1, after), Lookup::OtherGeneration));
    }

    #[test]
    fn expiry_wins_over_generation_mismatch() {
        let mut c = cache(100);
        c.insert(1, arc_of(vec![fake_element(0)]), Generation::next());
        c.clock.advance(Duration::from_millis(101));
        assert!(matches!(c.lookup(1, generation()), Lookup::Miss));
        assert_eq!(c.len(), 0, "expired entry should be evicted");
    }

    #[test]
    fn disabled_cache_misses_any_generation() {
        let mut c: CacheManager<MockClock> = CacheManager::with_clock(500, false, MockClock::new());
        let g = Generation::next();
        c.insert(1, arc_of(vec![fake_element(0)]), g);
        assert!(matches!(c.lookup(1, g), Lookup::Miss));
    }

    #[test]
    fn has_fresh_ignores_generation_and_respects_ttl() {
        let mut c = cache(100);
        assert!(!c.has_fresh(1));
        c.insert(1, arc_of(vec![fake_element(0)]), Generation::next());
        assert!(c.has_fresh(1), "any generation counts as cached");
        c.clock.advance(Duration::from_millis(101));
        assert!(!c.has_fresh(1));
        assert_eq!(c.len(), 0, "expired entry should be evicted");
    }

    #[test]
    fn generations_are_unique_across_threads() {
        let handles: Vec<_> = (0..8)
            .map(|_| {
                std::thread::spawn(|| (0..1000).map(|_| Generation::next()).collect::<Vec<_>>())
            })
            .collect();
        let mut all = std::collections::HashSet::new();
        for h in handles {
            for g in h.join().unwrap() {
                assert!(all.insert(g), "duplicate generation");
            }
        }
    }
}
