//! Labels file: one JSON line per shown pick, listing each label and
//! the rectangle it was drawn for. pick-bench's `check` mode reads it to
//! know which label to type for a given fixture element (bench contract
//! item 7, `bench/README.md`).
//!
//! Telemetry builds only, and off unless `JUMPKEY_BENCH_LABELS` names a
//! file. The line is written and flushed **before** the overlay window
//! is shown, so a reader that waits for the show event always finds it.
//!
//! Privacy: labels and rectangles only. **No names, titles or URLs.**

use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use parking_lot::Mutex;
use serde_json::{json, Value};

use crate::Bounds;

/// Append one pick's labels, if the labels file is on.
pub(crate) fn record<'a>(labels: impl Iterator<Item = (&'a str, &'a Bounds)>) {
    let Some(file) = labels_file() else {
        return;
    };
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let mut line = line_json(seq, labels).to_string();
    line.push('\n');
    let mut f = file.lock();
    if let Err(e) = f.write_all(line.as_bytes()).and_then(|()| f.flush()) {
        tracing::warn!(error = ?e, "labels file write failed");
    }
}

fn labels_file() -> Option<&'static Mutex<std::fs::File>> {
    static FILE: OnceLock<Option<Mutex<std::fs::File>>> = OnceLock::new();
    FILE.get_or_init(|| {
        let path = std::env::var_os("JUMPKEY_BENCH_LABELS").filter(|s| !s.is_empty())?;
        let path = std::path::PathBuf::from(path);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).ok()?;
        }
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            Ok(f) => {
                tracing::info!(?path, "labels file on");
                Some(Mutex::new(f))
            }
            Err(e) => {
                tracing::warn!(?path, error = ?e, "labels file: can't open");
                None
            }
        }
    })
    .as_ref()
}

fn line_json<'a>(seq: u64, labels: impl Iterator<Item = (&'a str, &'a Bounds)>) -> Value {
    let labels: Vec<Value> = labels
        .map(|(label, b)| json!({ "label": label, "rect": [b.x, b.y, b.width, b.height] }))
        .collect();
    json!({ "seq": seq, "labels": labels })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_has_labels_and_rects_only() {
        let a = Bounds {
            x: 1,
            y: 2,
            width: 30,
            height: 40,
        };
        let b = Bounds {
            x: 5,
            y: 6,
            width: 7,
            height: 8,
        };
        let v = line_json(3, [("as", &a), ("d", &b)].into_iter());
        assert_eq!(v["seq"], 3);
        assert_eq!(
            v["labels"][0],
            json!({"label": "as", "rect": [1, 2, 30, 40]})
        );
        assert_eq!(v["labels"][1]["label"], "d");
        let keys: Vec<&String> = v["labels"][0].as_object().unwrap().keys().collect();
        assert_eq!(keys.len(), 2, "only label and rect per entry");
    }
}
