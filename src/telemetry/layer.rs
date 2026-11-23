//! The `tracing` layer that turns telemetry spans into JSON lines.
//!
//! Spans are grouped into one trace per **root span** (a span named in
//! [`ROOTS`] with no tracked parent). Children find their trace through
//! their parent in the span registry, not through a thread-local, so a
//! walk on a rayon worker (created with an explicit parent) lands in
//! the same trace as the hotkey handler that started it. Spans outside
//! any root (e.g. walks on the prewarm thread) are ignored.
//!
//! The registry keeps a parent open until its children close, so the
//! root closes last; at that point the trace is serialized and written
//! as one line. Traces whose root never closes are written with
//! `incomplete: true` at shutdown, or when they're older than
//! [`STALE_AFTER_SECS`] (a pick legitimately stays open while the
//! overlay waits for keys, so the cutoff is generous).
//!
//! CPU per span: `QueryThreadCycleTime` on enter and exit, summed over
//! every enter/exit pair. Inclusive: a parent's cycles include its
//! children's (they run while it's entered).

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use parking_lot::Mutex;
use serde_json::{json, Map, Value};
use tracing::field::{Field, Visit};
use tracing::{span, Event, Level, Subscriber};
use tracing_subscriber::filter::{Filtered, Targets};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

use super::clock::{cycles_to_us, qpc, qpc_freq, qpc_us, thread_cycles, thread_id};
use super::{SCHEMA, TARGET};

/// Span names that start a trace.
pub(crate) const ROOTS: &[&str] = &["pick", "scroll"];

/// Open traces older than this are written as incomplete.
const STALE_AFTER_SECS: i64 = 600;

/// String-valued fields that may be written. Everything else that
/// isn't a number or bool is dropped and counted in `dropped_fields`:
/// the guard against titles, element names or URLs reaching the file.
const STRING_FIELDS: &[&str] = &[
    "kind",
    "scope",
    "outcome",
    "cancel_by",
    "first_from",
    "exit_by",
    "modifier",
    "cache",
    "action",
    "invoke_path",
    "hotkey_source",
    "process.executable.name",
    "milestone",
];

/// Whitelisted strings are cut to this many characters.
const MAX_STRING: usize = 64;

/// One recorded field value.
#[derive(Debug, Clone, PartialEq)]
enum Val {
    U(u64),
    I(i64),
    F(f64),
    B(bool),
    S(String),
}

impl Val {
    fn to_json(&self) -> Value {
        match self {
            Val::U(v) => json!(v),
            Val::I(v) => json!(v),
            Val::F(v) => json!(v),
            Val::B(v) => json!(v),
            Val::S(v) => json!(v),
        }
    }
}

/// Field values of one span or event, after the whitelist.
#[derive(Debug, Default)]
struct Fields {
    vals: Vec<(&'static str, Val)>,
    dropped: u32,
}

impl Fields {
    fn set(&mut self, name: &'static str, v: Val) {
        match self.vals.iter_mut().find(|(k, _)| *k == name) {
            Some(slot) => slot.1 = v,
            None => self.vals.push((name, v)),
        }
    }

    fn get(&self, name: &str) -> Option<&Val> {
        self.vals.iter().find(|(k, _)| *k == name).map(|(_, v)| v)
    }

    fn set_str(&mut self, name: &'static str, s: &str) {
        if STRING_FIELDS.contains(&name) {
            self.set(name, Val::S(s.chars().take(MAX_STRING).collect()));
        } else {
            self.dropped += 1;
        }
    }

    fn to_json(&self) -> Value {
        Value::Object(
            self.vals
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_json()))
                .collect(),
        )
    }
}

impl Visit for Fields {
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.set(field.name(), Val::U(value));
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.set(field.name(), Val::I(value));
    }
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.set(field.name(), Val::F(value));
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.set(field.name(), Val::B(value));
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        self.set_str(field.name(), value);
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        // `?x` / `%x` fields. Only whitelisted names are formatted, so
        // a `?element` can't leak a name through its Debug output.
        if STRING_FIELDS.contains(&field.name()) {
            let s = format!("{value:?}");
            self.set_str(field.name(), s.trim_matches('"'));
        } else {
            self.dropped += 1;
        }
    }
}

/// Per-span state, stored in the registry's span extensions.
struct SpanData {
    trace: u64,
    no: u32,
    parent_no: u32,
    name: &'static str,
    start_qpc: i64,
    thread: u32,
    cycles: u64,
    entered_at: Option<u64>,
    fields: Fields,
    is_root: bool,
}

/// A trace whose root is still open.
struct TraceBuf {
    root_name: &'static str,
    start_qpc: i64,
    start_unix_nano: u64,
    next_no: u32,
    spans: Vec<Value>,
    events: Vec<Value>,
    dropped: u32,
}

/// State shared by the layer, [`shutdown`] and the session writer.
pub(crate) struct Shared {
    writer: Mutex<Box<dyn Write + Send>>,
    open: Mutex<HashMap<u64, TraceBuf>>,
    next_trace: AtomicU64,
    seq: AtomicU64,
    session: String,
}

impl Shared {
    fn new(writer: Box<dyn Write + Send>) -> Self {
        Self {
            writer: Mutex::new(writer),
            open: Mutex::new(HashMap::new()),
            next_trace: AtomicU64::new(1),
            seq: AtomicU64::new(0),
            session: new_session_id(),
        }
    }

    pub(crate) fn session(&self) -> &str {
        &self.session
    }

    /// Write one JSON record as a line.
    pub(crate) fn write_line(&self, v: &Value) {
        if let Ok(mut line) = serde_json::to_string(v) {
            line.push('\n');
            let _ = self.writer.lock().write_all(line.as_bytes());
        }
    }

    fn flush(&self) {
        let _ = self.writer.lock().flush();
    }

    /// Serialize a finished (or abandoned) trace and write it.
    fn finish(&self, trace: u64, buf: TraceBuf, root: Option<&SpanData>, end_qpc: i64) {
        let build_start = qpc();
        let TraceBuf {
            root_name,
            start_qpc,
            start_unix_nano,
            mut spans,
            events,
            dropped,
            ..
        } = buf;
        spans.sort_by_key(|s| s["id"].as_u64().unwrap_or(0));
        let mut rec = Map::new();
        rec.insert("record".into(), json!(root_name));
        rec.insert("schema".into(), json!(SCHEMA));
        rec.insert("session".into(), json!(self.session));
        rec.insert("trace_id".into(), json!(trace));
        rec.insert(
            "seq".into(),
            json!(self.seq.fetch_add(1, Ordering::Relaxed)),
        );
        rec.insert("start_unix_nano".into(), json!(start_unix_nano));
        rec.insert("start_qpc".into(), json!(start_qpc));
        rec.insert("dur_us".into(), json!(qpc_us(start_qpc, end_qpc)));
        let (thread, cycles, attrs, root_dropped) = match root {
            Some(r) => (r.thread, r.cycles, r.fields.to_json(), r.fields.dropped),
            None => (0, 0, json!({}), 0),
        };
        rec.insert("thread".into(), json!(thread));
        rec.insert("cpu_cycles".into(), json!(cycles));
        rec.insert("cpu_us".into(), json!(cycles_to_us(cycles)));
        rec.insert("attributes".into(), attrs);
        rec.insert("events".into(), Value::Array(events));
        rec.insert("spans".into(), Value::Array(spans));
        rec.insert("dropped_fields".into(), json!(dropped + root_dropped));
        rec.insert("incomplete".into(), json!(root.is_none()));
        rec.insert("record_build_us".into(), json!(qpc_us(build_start, qpc())));
        let rec = Value::Object(rec);
        self.write_line(&rec);
        LAST_SUMMARY.with(|s| *s.borrow_mut() = Some(summary(&rec)));
    }

    /// Write every open trace as incomplete (shutdown).
    fn flush_open(&self) {
        let open: Vec<(u64, TraceBuf)> = self.open.lock().drain().collect();
        let now = qpc();
        for (trace, buf) in open {
            self.finish(trace, buf, None, now);
        }
        self.flush();
    }
}

static GLOBAL: OnceLock<Arc<Shared>> = OnceLock::new();

/// The app-wide shared state, once [`layer`] has run.
pub(crate) fn global() -> Option<&'static Arc<Shared>> {
    GLOBAL.get()
}

thread_local! {
    /// One-line summary of the last trace finished on this thread.
    static LAST_SUMMARY: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Take the summary of the trace that just finished on this thread.
pub(crate) fn take_summary() -> Option<String> {
    LAST_SUMMARY.with(|s| s.borrow_mut().take())
}

/// The telemetry layer. Build it with [`layer`].
pub struct TelemetryLayer {
    shared: Arc<Shared>,
}

impl TelemetryLayer {
    /// A layer writing to `writer` instead of the daily files (tests).
    #[cfg(test)]
    pub(crate) fn with_writer(writer: impl Write + Send + 'static) -> Self {
        Self {
            shared: Arc::new(Shared::new(Box::new(writer))),
        }
    }

    fn new_trace(&self, name: &'static str, now: i64) -> u64 {
        let trace = self.shared.next_trace.fetch_add(1, Ordering::Relaxed);
        let start_unix_nano = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let stale_ticks = STALE_AFTER_SECS * qpc_freq();
        let stale: Vec<(u64, TraceBuf)> = {
            let mut open = self.shared.open.lock();
            let stale_ids: Vec<u64> = open
                .iter()
                .filter(|(_, b)| now - b.start_qpc > stale_ticks)
                .map(|(id, _)| *id)
                .collect();
            let stale = stale_ids
                .into_iter()
                .filter_map(|id| open.remove(&id).map(|b| (id, b)))
                .collect();
            open.insert(
                trace,
                TraceBuf {
                    root_name: name,
                    start_qpc: now,
                    start_unix_nano,
                    next_no: 1,
                    spans: Vec::new(),
                    events: Vec::new(),
                    dropped: 0,
                },
            );
            stale
        };
        for (id, buf) in stale {
            self.shared.finish(id, buf, None, now);
        }
        trace
    }
}

impl<S> Layer<S> for TelemetryLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &span::Attributes<'_>, id: &span::Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        let name = attrs.metadata().name();
        let now = qpc();
        let parent = span
            .parent()
            .and_then(|p| p.extensions().get::<SpanData>().map(|d| (d.trace, d.no)));
        let (trace, no, parent_no, is_root) = match parent {
            Some((trace, parent_no)) => {
                let mut open = self.shared.open.lock();
                // The trace was already written as stale: ignore.
                let Some(buf) = open.get_mut(&trace) else {
                    return;
                };
                let no = buf.next_no;
                buf.next_no += 1;
                (trace, no, parent_no, false)
            }
            None if ROOTS.contains(&name) => (self.new_trace(name, now), 0, 0, true),
            None => return,
        };
        let mut fields = Fields::default();
        attrs.record(&mut fields);
        span.extensions_mut().insert(SpanData {
            trace,
            no,
            parent_no,
            name,
            start_qpc: now,
            thread: thread_id(),
            cycles: 0,
            entered_at: None,
            fields,
            is_root,
        });
    }

    fn on_record(&self, id: &span::Id, values: &span::Record<'_>, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        let mut ext = span.extensions_mut();
        if let Some(data) = ext.get_mut::<SpanData>() {
            values.record(&mut data.fields);
        }
    }

    fn on_enter(&self, id: &span::Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        let mut ext = span.extensions_mut();
        if let Some(data) = ext.get_mut::<SpanData>() {
            data.entered_at = Some(thread_cycles());
        }
    }

    fn on_exit(&self, id: &span::Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        let mut ext = span.extensions_mut();
        if let Some(data) = ext.get_mut::<SpanData>() {
            if let Some(start) = data.entered_at.take() {
                data.cycles += thread_cycles().saturating_sub(start);
            }
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let Some(span) = ctx.event_span(event) else {
            return;
        };
        let (trace, span_no) = {
            let ext = span.extensions();
            let Some(d) = ext.get::<SpanData>() else {
                return;
            };
            (d.trace, d.no)
        };
        let mut f = Fields::default();
        event.record(&mut f);
        let Some(Val::S(name)) = f.get("milestone").cloned() else {
            return;
        };
        let at = match f.get("at_qpc") {
            Some(Val::I(v)) => *v,
            Some(Val::U(v)) => *v as i64,
            _ => qpc(),
        };
        let mut open = self.shared.open.lock();
        if let Some(buf) = open.get_mut(&trace) {
            buf.events.push(json!({
                "name": name,
                "t_us": qpc_us(buf.start_qpc, at),
                "span": span_no,
            }));
            buf.dropped += f.dropped;
        }
    }

    fn on_close(&self, id: span::Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(&id) else { return };
        let Some(data) = span.extensions_mut().remove::<SpanData>() else {
            return;
        };
        let now = qpc();
        let mut open = self.shared.open.lock();
        if data.is_root {
            let Some(buf) = open.remove(&data.trace) else {
                return;
            };
            drop(open);
            self.shared.finish(data.trace, buf, Some(&data), now);
        } else if let Some(buf) = open.get_mut(&data.trace) {
            buf.dropped += data.fields.dropped;
            buf.spans.push(json!({
                "id": data.no,
                "parent": data.parent_no,
                "name": data.name,
                "start_us": qpc_us(buf.start_qpc, data.start_qpc),
                "dur_us": qpc_us(data.start_qpc, now),
                "thread": data.thread,
                "cpu_cycles": data.cycles,
                "cpu_us": cycles_to_us(data.cycles),
                "attributes": data.fields.to_json(),
            }));
        }
    }
}

/// Keeps the background file writer alive; flushes open traces and
/// pending lines when dropped. Hold it until the process exits.
pub struct TelemetryGuard {
    _worker: tracing_appender::non_blocking::WorkerGuard,
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        shutdown();
        // `_worker` drops after this, flushing the writer thread.
    }
}

/// Build the telemetry layer writing daily files to
/// `%LOCALAPPDATA%\jumpkey\telemetry\` (14 days kept). `None` if that
/// folder can't be created. Also starts TSC calibration.
#[allow(clippy::type_complexity)]
pub fn layer<S>() -> Option<(Filtered<TelemetryLayer, Targets, S>, TelemetryGuard)>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    use tracing_appender::rolling::{RollingFileAppender, Rotation};

    let dir = std::path::PathBuf::from(std::env::var_os("LOCALAPPDATA")?)
        .join("jumpkey")
        .join("telemetry");
    std::fs::create_dir_all(&dir).ok()?;
    let appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("telemetry")
        .filename_suffix("jsonl")
        .max_log_files(14)
        .build(&dir)
        .ok()?;
    let (writer, worker) = tracing_appender::non_blocking(appender);
    let shared = Arc::new(Shared::new(Box::new(writer)));
    let _ = GLOBAL.set(shared.clone());
    super::clock::calibrate_in_background();
    let filter = Targets::new().with_target(TARGET, Level::TRACE);
    Some((
        TelemetryLayer { shared }.with_filter(filter),
        TelemetryGuard { _worker: worker },
    ))
}

/// Write open traces as incomplete and flush. Called on exit (also by
/// [`TelemetryGuard`]'s drop).
pub fn shutdown() {
    if let Some(shared) = GLOBAL.get() {
        shared.flush_open();
    }
}

/// Random-enough session id: hash of time, pid and a per-process random
/// seed, as 16 hex digits.
fn new_session_id() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hash, Hasher};
    let mut h = RandomState::new().build_hasher();
    std::time::SystemTime::now().hash(&mut h);
    std::process::id().hash(&mut h);
    format!("{:016x}", h.finish())
}

/// `pick element 812.4 ms (picked): enumerate 640.1, labels 0.3, … shown at 701.2 ms`
fn summary(rec: &Value) -> String {
    let ms = |us: &Value| us.as_f64().unwrap_or(0.0) / 1000.0;
    let attrs = &rec["attributes"];
    let mut s = format!(
        "telemetry: {} {} {:.1} ms ({})",
        rec["record"].as_str().unwrap_or("?"),
        attrs["kind"].as_str().unwrap_or("?"),
        ms(&rec["dur_us"]),
        attrs["outcome"].as_str().unwrap_or("incomplete"),
    );
    let top: Vec<String> = rec["spans"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|sp| sp["parent"].as_u64() == Some(0))
        .map(|sp| {
            format!(
                "{} {:.1}",
                sp["name"].as_str().unwrap_or("?"),
                ms(&sp["dur_us"])
            )
        })
        .collect();
    if !top.is_empty() {
        s.push_str(": ");
        s.push_str(&top.join(", "));
    }
    if let Some(shown) = rec["events"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|e| e["name"] == "shown")
    {
        s.push_str(&format!("; shown at {:.1} ms", ms(&shown["t_us"])));
    }
    s
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use tracing_subscriber::layer::SubscriberExt;

    /// Shared in-memory writer.
    #[derive(Clone, Default)]
    pub(crate) struct Buf(pub Arc<Mutex<Vec<u8>>>);

    impl Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Buf {
        pub(crate) fn lines(&self) -> Vec<Value> {
            String::from_utf8(self.0.lock().clone())
                .unwrap()
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect()
        }
    }

    /// Run `f` with a subscriber holding only a telemetry layer that
    /// writes to the returned buffer.
    pub(crate) fn capture(f: impl FnOnce()) -> Vec<Value> {
        let buf = Buf::default();
        let layer = TelemetryLayer::with_writer(buf.clone())
            .with_filter(Targets::new().with_target(TARGET, Level::TRACE));
        let sub = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(sub, f);
        buf.lines()
    }

    #[test]
    fn one_line_per_root_with_children_and_milestones() {
        let lines = capture(|| {
            let root = tracing::info_span!(target: TARGET, "pick", kind = "element", count = tracing::field::Empty).entered();
            {
                let _c =
                    tracing::info_span!(target: TARGET, "enumerate", collected = 3u64).entered();
                tracing::info!(target: TARGET, milestone = "elements_ready");
            }
            root.record("count", 3u64);
        });
        assert_eq!(lines.len(), 1);
        let rec = &lines[0];
        assert_eq!(rec["record"], "pick");
        assert_eq!(rec["incomplete"], false);
        assert_eq!(rec["attributes"]["kind"], "element");
        assert_eq!(rec["attributes"]["count"], 3);
        assert_eq!(rec["spans"][0]["name"], "enumerate");
        assert_eq!(rec["spans"][0]["parent"], 0);
        assert_eq!(rec["spans"][0]["attributes"]["collected"], 3);
        assert_eq!(rec["events"][0]["name"], "elements_ready");
        assert_eq!(rec["events"][0]["span"], 1);
        assert_eq!(rec["dropped_fields"], 0);
    }

    #[test]
    fn non_whitelisted_strings_are_dropped_and_counted() {
        let lines = capture(|| {
            let _root = tracing::info_span!(target: TARGET, "pick",
                kind = "element",
                title = "Secret Document - Notepad",
                name = ?"OK button",
                url = %"https://example.com/private")
            .entered();
        });
        let rec = &lines[0];
        let text = rec.to_string();
        assert!(!text.contains("Secret"));
        assert!(!text.contains("OK button"));
        assert!(!text.contains("example.com"));
        assert_eq!(rec["dropped_fields"], 3);
        assert_eq!(rec["attributes"]["kind"], "element");
    }

    #[test]
    fn spans_outside_a_root_are_ignored() {
        let lines = capture(|| {
            let _w = tracing::info_span!(target: TARGET, "walk").entered();
            tracing::info!(target: TARGET, milestone = "shown");
        });
        assert!(lines.is_empty());
    }

    #[test]
    fn explicit_parent_on_another_thread_joins_the_trace() {
        let lines = capture(|| {
            let _root = tracing::info_span!(target: TARGET, "pick").entered();
            let parent = tracing::Span::current();
            let dispatch = tracing::dispatcher::get_default(|d| d.clone());
            std::thread::spawn(move || {
                tracing::dispatcher::with_default(&dispatch, || {
                    let _w = tracing::info_span!(target: TARGET, parent: &parent, "walk").entered();
                });
            })
            .join()
            .unwrap();
        });
        let rec = &lines[0];
        let walk = &rec["spans"][0];
        assert_eq!(walk["name"], "walk");
        assert_ne!(walk["thread"], rec["thread"]);
    }

    #[test]
    fn at_qpc_backdates_a_milestone() {
        let lines = capture(|| {
            let earlier = qpc() - qpc_freq() / 100; // 10 ms ago
            let _root = tracing::info_span!(target: TARGET, "pick").entered();
            tracing::info!(target: TARGET, milestone = "hotkey", at_qpc = earlier);
        });
        let t = lines[0]["events"][0]["t_us"].as_i64().unwrap();
        assert!(t <= -9_000, "t_us = {t}");
    }

    #[test]
    fn stale_open_trace_is_written_incomplete() {
        let buf = Buf::default();
        let layer = TelemetryLayer::with_writer(buf.clone());
        let shared = layer.shared.clone();
        // A trace that started 11 minutes ago and never closed.
        shared.open.lock().insert(
            99,
            TraceBuf {
                root_name: "pick",
                start_qpc: qpc() - 660 * qpc_freq(),
                start_unix_nano: 0,
                next_no: 1,
                spans: Vec::new(),
                events: Vec::new(),
                dropped: 0,
            },
        );
        let sub = tracing_subscriber::registry()
            .with(layer.with_filter(Targets::new().with_target(TARGET, Level::TRACE)));
        tracing::subscriber::with_default(sub, || {
            let _root = tracing::info_span!(target: TARGET, "pick").entered();
        });
        let lines = buf.lines();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["trace_id"], 99);
        assert_eq!(lines[0]["incomplete"], true);
        assert_eq!(lines[1]["incomplete"], false);
        // And shutdown flushes whatever is still open.
        shared.open.lock().insert(
            7,
            TraceBuf {
                root_name: "pick",
                start_qpc: qpc(),
                start_unix_nano: 0,
                next_no: 1,
                spans: Vec::new(),
                events: Vec::new(),
                dropped: 0,
            },
        );
        shared.flush_open();
        let lines = buf.lines();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[2]["trace_id"], 7);
    }

    #[test]
    fn cpu_cycles_are_counted_while_entered() {
        let lines = capture(|| {
            let _root = tracing::info_span!(target: TARGET, "pick").entered();
            let _busy = tracing::info_span!(target: TARGET, "busy").entered();
            let mut x = 0u64;
            for i in 0..2_000_000u64 {
                x = x.wrapping_add(i ^ (i >> 3));
            }
            std::hint::black_box(x);
        });
        let rec = &lines[0];
        let busy = rec["spans"][0]["cpu_cycles"].as_u64().unwrap();
        let root = rec["cpu_cycles"].as_u64().unwrap();
        assert!(busy > 0);
        assert!(root >= busy, "inclusive: root {root} < child {busy}");
    }

    #[test]
    fn summary_lists_top_level_spans() {
        let rec = json!({
            "record": "pick", "dur_us": 812400,
            "attributes": {"kind": "element", "outcome": "picked"},
            "spans": [
                {"name": "enumerate", "parent": 0, "dur_us": 640100},
                {"name": "walk", "parent": 1, "dur_us": 600000},
            ],
            "events": [{"name": "shown", "t_us": 701200}],
        });
        assert_eq!(
            summary(&rec),
            "telemetry: pick element 812.4 ms (picked): enumerate 640.1; shown at 701.2 ms"
        );
    }
}
