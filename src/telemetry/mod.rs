//! In-app timing of the element and window pickers.
//!
//! Behind the `telemetry` cargo feature (off by default, never in
//! released builds). With the feature on, each hotkey press produces one
//! JSON line in `%LOCALAPPDATA%\jumpkey\telemetry\telemetry.YYYY-MM-DD.jsonl`
//! describing where the time went: a `pick` root span, its child spans
//! (enumerate, walk, labels, overlay, perform …) with wall time and CPU
//! cycles, and milestone events (`hotkey`, `elements_ready`, `shown` …).
//! See `docs/TELEMETRY.md` for the record format and the milestone
//! contract.
//!
//! With the feature off, every macro here expands to nothing and every
//! function is an empty `#[inline]` stub, so call sites don't need
//! `cfg` attributes and nothing is compiled into the binary.
//!
//! Privacy (architecture review #36): records hold ids, counts,
//! timings, enums and exe basenames only. The layer drops every string
//! field that isn't on a short whitelist, so a window title or element
//! name passed by mistake never reaches the file.
//!
//! # Usage
//!
//! ```ignore
//! use jumpkey::{tel_event, tel_record, tel_span};
//! let span = tel_span!("enumerate", collected = tracing::field::Empty).entered();
//! // …
//! tel_record!(span, "collected", elements.len());
//! tel_event!("elements_ready");
//! ```

#[cfg(all(feature = "telemetry", windows))]
mod clock;
#[cfg(all(feature = "telemetry", windows))]
mod layer;
#[cfg(all(feature = "telemetry", windows))]
mod machine;
#[cfg(all(feature = "telemetry", windows))]
mod pick;

#[cfg(all(feature = "telemetry", windows))]
pub use layer::{layer, shutdown, TelemetryGuard, TelemetryLayer};
#[cfg(all(feature = "telemetry", windows))]
pub use pick::{note_backspace, note_cancel, note_key, note_message, start, Pick, Started};

/// `tracing` target of every telemetry span and event. The normal log
/// filters it out; only the telemetry layer listens to it.
pub const TARGET: &str = "jumpkey::telemetry";

/// Version of the JSON record format. Bump when a field is renamed or
/// removed, or its meaning changes (adding fields doesn't need a bump).
pub const SCHEMA: u32 = 1;

/// Config facts written once per session in the `session` record.
#[derive(Debug, Clone, Default)]
pub struct SessionInfo {
    /// `[scope] mode`, e.g. `ActiveWindow`.
    pub scope: String,
    /// `hints.order`, e.g. `Center`.
    pub hint_order: String,
    /// Number of characters in the resolved hint alphabet.
    pub alphabet_len: usize,
    /// `performance.enable_caching`.
    pub caching: bool,
    /// `performance.cache_ttl_ms`.
    pub cache_ttl_ms: u64,
    /// `scope.max_elements`.
    pub max_elements: usize,
}

// ---------------------------------------------------------------------------
// Feature off: no-op stand-ins with the same API.
// ---------------------------------------------------------------------------

/// Stand-in for a span when the `telemetry` feature is off. Has an
/// (empty) `Drop` so `drop(span)` reads the same with the feature on or
/// off without tripping lints.
#[derive(Debug, Clone, Default)]
pub struct NoSpan;

impl Drop for NoSpan {
    #[inline(always)]
    fn drop(&mut self) {}
}

impl NoSpan {
    /// Mirrors `tracing::Span::entered`.
    #[inline(always)]
    pub fn entered(self) -> Self {
        self
    }
}

#[cfg(not(all(feature = "telemetry", windows)))]
mod off {
    use super::SessionInfo;

    /// Root of one picker trace. No-op without the `telemetry` feature.
    #[derive(Debug, Default)]
    pub struct Pick;

    impl Pick {
        /// Start a trace for one hotkey press.
        #[inline(always)]
        pub fn begin(_kind: &'static str, _scope: &dyn std::fmt::Debug) -> Self {
            Pick
        }
        /// Number of hints shown.
        #[inline(always)]
        pub fn count(&self, _n: usize) {}
        /// How the pick ended: `picked`, `cancelled`, `empty`.
        #[inline(always)]
        pub fn outcome(&self, _outcome: &'static str) {}
        /// Modifier held on the completing keystroke.
        #[inline(always)]
        pub fn modifier(&self, _modifier: &'static str) {}
    }

    /// Handle returned by [`start`]; no-op without the feature.
    #[derive(Debug)]
    pub struct Started;

    /// Write the `session` record and start foreground tracking.
    #[inline(always)]
    pub fn start(_info: SessionInfo) -> Started {
        Started
    }

    /// Called by the main loop for every message, before dispatch.
    #[inline(always)]
    pub fn note_message(_message: u32, _time: u32) {}

    /// A character typed into the overlay.
    #[inline(always)]
    pub fn note_key() {}

    /// Backspace pressed in the overlay.
    #[inline(always)]
    pub fn note_backspace() {}

    /// The overlay closed without a pick, and why.
    #[inline(always)]
    pub fn note_cancel(_reason: &'static str) {}

    /// Flush open traces before exit.
    #[inline(always)]
    pub fn shutdown() {}
}

#[cfg(not(all(feature = "telemetry", windows)))]
pub use off::{
    note_backspace, note_cancel, note_key, note_message, shutdown, start, Pick, Started,
};

// ---------------------------------------------------------------------------
// Macros
// ---------------------------------------------------------------------------

/// Create a telemetry span (`tracing::info_span!` on target
/// [`TARGET`]). Expands to [`NoSpan`] without the feature.
///
/// `tel_span!("name", field = value, …)` or
/// `tel_span!(parent: &span, "name", …)` for an explicit parent (needed
/// on worker threads, which don't inherit the caller's current span).
#[cfg(all(feature = "telemetry", windows))]
#[macro_export]
macro_rules! tel_span {
    (parent: $parent:expr, $name:literal $(, $($fields:tt)+)?) => {
        ::tracing::info_span!(target: "jumpkey::telemetry", parent: $parent, $name $(, $($fields)+)?)
    };
    ($name:literal $(, $($fields:tt)+)?) => {
        ::tracing::info_span!(target: "jumpkey::telemetry", $name $(, $($fields)+)?)
    };
}

/// Create a telemetry span. Expands to [`NoSpan`] without the feature.
#[cfg(not(all(feature = "telemetry", windows)))]
#[macro_export]
macro_rules! tel_span {
    (parent: $parent:expr, $($rest:tt)*) => {{
        let _ = &$parent;
        $crate::telemetry::NoSpan
    }};
    ($($rest:tt)*) => {
        $crate::telemetry::NoSpan
    };
}

/// The current span, to pass as an explicit parent to worker threads.
#[cfg(all(feature = "telemetry", windows))]
#[macro_export]
macro_rules! tel_current {
    () => {
        ::tracing::Span::current()
    };
}

/// The current span. Expands to [`NoSpan`] without the feature.
#[cfg(not(all(feature = "telemetry", windows)))]
#[macro_export]
macro_rules! tel_current {
    () => {
        $crate::telemetry::NoSpan
    };
}

/// Record a milestone on the current trace: `tel_event!("shown")`.
#[cfg(all(feature = "telemetry", windows))]
#[macro_export]
macro_rules! tel_event {
    ($name:literal) => {
        ::tracing::info!(target: "jumpkey::telemetry", milestone = $name)
    };
}

/// Record a milestone. Expands to nothing without the feature.
#[cfg(not(all(feature = "telemetry", windows)))]
#[macro_export]
macro_rules! tel_event {
    ($name:literal) => {};
}

/// Record a field declared (as `tracing::field::Empty`) when the span
/// was created: `tel_record!(span, "collected", n)`.
#[cfg(all(feature = "telemetry", windows))]
#[macro_export]
macro_rules! tel_record {
    ($span:expr, $field:literal, $value:expr) => {
        $span.record($field, $value);
    };
}

/// Record a span field. Expands to nothing without the feature.
#[cfg(not(all(feature = "telemetry", windows)))]
#[macro_export]
macro_rules! tel_record {
    ($span:expr, $field:literal, $value:expr) => {
        let _ = &$span;
    };
}

/// Record a field on whichever span is current (for code that doesn't
/// own the span, e.g. `invoke_path` inside `perform`).
#[cfg(all(feature = "telemetry", windows))]
#[macro_export]
macro_rules! tel_record_current {
    ($field:literal, $value:expr) => {
        ::tracing::Span::current().record($field, $value);
    };
}

/// Record a field on the current span. Expands to nothing without the
/// feature.
#[cfg(not(all(feature = "telemetry", windows)))]
#[macro_export]
macro_rules! tel_record_current {
    ($field:literal, $value:expr) => {};
}

/// Golden schema test: the records the layer writes must have every
/// field of `bench/schema-example.jsonl`, with the same JSON types. The
/// analysis scripts (PR 3) parse the same file, so a format change has
/// to update it (and bump [`SCHEMA`] if a field is renamed or removed).
#[cfg(all(test, feature = "telemetry", windows))]
mod schema_tests {
    use serde_json::Value;
    use windows::Win32::System::SystemInformation::GetTickCount;
    use windows::Win32::UI::WindowsAndMessaging::WM_HOTKEY;

    use super::{clock, layer, machine, pick, Pick, SessionInfo};

    const EXAMPLE: &str = include_str!("../../bench/schema-example.jsonl");

    fn type_name(v: &Value) -> &'static str {
        match v {
            Value::Null => "null",
            Value::Bool(_) => "bool",
            Value::Number(_) => "number",
            Value::String(_) => "string",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
        }
    }

    /// Every key of `example` must exist in `actual` with the same type.
    /// `null` in the example means "any type, may be null". Arrays: each
    /// actual element is checked against the example's first element.
    /// Span attributes differ per span name, so only their type is
    /// checked.
    fn check(example: &Value, actual: &Value, path: &str, errors: &mut Vec<String>) {
        if example.is_null() {
            return;
        }
        if type_name(example) != type_name(actual) {
            errors.push(format!(
                "{path}: expected {}, got {}",
                type_name(example),
                type_name(actual)
            ));
            return;
        }
        match (example, actual) {
            (Value::Object(e), Value::Object(a)) => {
                if path.ends_with("spans[].attributes") {
                    return;
                }
                for (k, ev) in e {
                    match a.get(k) {
                        Some(av) => check(ev, av, &format!("{path}.{k}"), errors),
                        None => errors.push(format!("{path}.{k}: missing")),
                    }
                }
            }
            (Value::Array(e), Value::Array(a)) => {
                if let Some(first) = e.first() {
                    for av in a {
                        check(first, av, &format!("{path}[]"), errors);
                    }
                }
            }
            _ => {}
        }
    }

    fn fake_pick(kind: &'static str, outcome: &'static str) {
        // SAFETY: no preconditions.
        pick::note_message(WM_HOTKEY, unsafe { GetTickCount() });
        let p = Pick::begin(kind, &"ActiveWindow");
        {
            let _s = crate::tel_span!("refresh_layout", rebuilt = false).entered();
        }
        if kind == "element" {
            crate::tel_event!("enum_start");
            let e = crate::tel_span!("enumerate", cache = "miss", collected = 3u64).entered();
            {
                let _w = crate::tel_span!(
                    "walk",
                    kind = "browser",
                    foreground = true,
                    process.executable.name = "vivaldi.exe",
                    visited = 10u64
                )
                .entered();
                let _s = crate::tel_span!("search").entered();
            }
            drop(e);
            crate::tel_event!("elements_ready");
            crate::tel_event!("labels_ready");
        }
        {
            let _s = crate::tel_span!("render").entered();
        }
        crate::tel_event!("first_ulw");
        crate::tel_event!("shown");
        super::note_key();
        crate::tel_event!("key_complete");
        if outcome == "cancelled" {
            super::note_cancel("esc");
        }
        p.count(3);
        p.outcome(outcome);
        if kind == "element" {
            p.modifier("none");
            let _s = crate::tel_span!(
                "perform",
                action = "Invoke",
                invoke_path = "Invoke",
                ok = true
            )
            .entered();
            crate::tel_event!("action_done");
        }
    }

    #[test]
    fn records_match_the_golden_example() {
        let _lock = pick::TEST_FG_LOCK.lock();
        clock::calibrate_now();
        pick::record_foreground(0x5151, clock::qpc());
        let lines = layer::tests::capture(|| {
            fake_pick("element", "picked"); // warm-up: gives the next a "last pick"
            fake_pick("element", "picked");
            fake_pick("window", "cancelled");
        });
        assert_eq!(lines.len(), 3);
        let session = machine::session_record("0123456789abcdef", &SessionInfo::default());
        let actual = [&session, &lines[1], &lines[2]];

        let examples: Vec<Value> = EXAMPLE
            .lines()
            .map(|l| serde_json::from_str(l).expect("schema-example.jsonl must be valid JSON"))
            .collect();
        assert_eq!(examples.len(), 3, "session, element pick, window pick");
        let mut errors = Vec::new();
        for (ex, act) in examples.iter().zip(actual) {
            let name = format!(
                "{}/{}",
                ex["record"].as_str().unwrap_or("?"),
                ex["attributes"]["kind"].as_str().unwrap_or("-")
            );
            check(ex, act, &name, &mut errors);
            assert_eq!(ex["schema"], act["schema"], "{name}: schema version");
        }
        assert!(
            errors.is_empty(),
            "schema mismatches:\n{}",
            errors.join("\n")
        );
    }
}
