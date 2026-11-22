//! Walk dump: one JSON line per UI Automation walk, for checking that a
//! walker change finds the **same elements**, not just that it's faster
//! (roadmap step 2, `scripts/walk_diff.py`).
//!
//! Telemetry builds only, and off unless `JUMPKEY_DUMP_WALK` is set:
//! `1` writes to `%LOCALAPPDATA%\jumpkey\telemetry\walks.jsonl` (which
//! pick-bench sandboxes per build), any other value is taken as the
//! folder to write `walks.jsonl` into.
//!
//! Privacy: each line holds the exe basename, counts, the window rect
//! and each element's role and rect. **No names, titles or URLs.**

use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;

use parking_lot::Mutex;
use serde_json::{json, Value};

use crate::{Bounds, Role};

/// What one walk found, as written to the dump.
pub(crate) struct WalkSummary<'a> {
    pub exe: Option<&'a str>,
    pub is_browser: bool,
    pub foreground: bool,
    pub window: Option<Bounds>,
    pub visited: usize,
    pub max_depth: usize,
    pub hit_element_cap: bool,
    pub elements: &'a [(Role, Bounds)],
}

/// Append one walk to the dump, if the dump is on.
pub(crate) fn record(summary: &WalkSummary<'_>) {
    let Some(file) = dump_file() else {
        return;
    };
    let mut line = summary_json(summary).to_string();
    line.push('\n');
    let mut f = file.lock();
    if let Err(e) = f.write_all(line.as_bytes()) {
        tracing::warn!(error = ?e, "walk dump write failed");
    }
}

fn dump_file() -> Option<&'static Mutex<std::fs::File>> {
    static FILE: OnceLock<Option<Mutex<std::fs::File>>> = OnceLock::new();
    FILE.get_or_init(|| {
        let dir = dump_dir(
            std::env::var_os("JUMPKEY_DUMP_WALK"),
            std::env::var_os("LOCALAPPDATA"),
        )?;
        std::fs::create_dir_all(&dir).ok()?;
        let path = dir.join("walks.jsonl");
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            Ok(f) => {
                tracing::info!(?path, "walk dump on");
                Some(Mutex::new(f))
            }
            Err(e) => {
                tracing::warn!(?path, error = ?e, "walk dump: can't open file");
                None
            }
        }
    })
    .as_ref()
}

/// Where the dump goes for a given `JUMPKEY_DUMP_WALK` value.
fn dump_dir(
    setting: Option<std::ffi::OsString>,
    localappdata: Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    let setting = setting.filter(|s| !s.is_empty())?;
    if setting == "1" {
        Some(
            PathBuf::from(localappdata?)
                .join("jumpkey")
                .join("telemetry"),
        )
    } else {
        Some(PathBuf::from(setting))
    }
}

fn rect(b: &Bounds) -> Value {
    json!([b.x, b.y, b.width, b.height])
}

fn summary_json(s: &WalkSummary<'_>) -> Value {
    let elements: Vec<Value> = s
        .elements
        .iter()
        .map(|(role, b)| json!({ "role": format!("{role:?}"), "rect": rect(b) }))
        .collect();
    json!({
        "exe": s.exe,
        "is_browser": s.is_browser,
        "foreground": s.foreground,
        "window": s.window.as_ref().map(rect),
        "visited": s.visited,
        "max_depth": s.max_depth,
        "hit_element_cap": s.hit_element_cap,
        "collected": s.elements.len(),
        "elements": elements,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(x: i32, y: i32, width: i32, height: i32) -> Bounds {
        Bounds {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn dump_dir_is_off_unless_set() {
        assert_eq!(dump_dir(None, Some("C:\\L".into())), None);
        assert_eq!(dump_dir(Some("".into()), Some("C:\\L".into())), None);
    }

    #[test]
    fn dump_dir_one_means_the_telemetry_folder() {
        assert_eq!(
            dump_dir(Some("1".into()), Some("C:\\L".into())),
            Some(PathBuf::from("C:\\L").join("jumpkey").join("telemetry"))
        );
        assert_eq!(dump_dir(Some("1".into()), None), None);
    }

    #[test]
    fn dump_dir_other_values_are_a_folder() {
        assert_eq!(
            dump_dir(Some("D:\\dumps".into()), None),
            Some(PathBuf::from("D:\\dumps"))
        );
    }

    #[test]
    fn summary_has_roles_and_rects_but_no_names() {
        let elements = [(Role::Button, b(1, 2, 30, 40)), (Role::Link, b(5, 6, 7, 8))];
        let v = summary_json(&WalkSummary {
            exe: Some("vivaldi.exe"),
            is_browser: true,
            foreground: true,
            window: Some(b(0, 0, 1600, 1000)),
            visited: 123,
            max_depth: 9,
            hit_element_cap: false,
            elements: &elements,
        });
        assert_eq!(v["collected"], 2);
        assert_eq!(v["visited"], 123);
        assert_eq!(v["window"], json!([0, 0, 1600, 1000]));
        assert_eq!(
            v["elements"][0],
            json!({"role": "Button", "rect": [1, 2, 30, 40]})
        );
        assert_eq!(v["elements"][1]["role"], "Link");
        let keys: Vec<&String> = v["elements"][0].as_object().unwrap().keys().collect();
        assert_eq!(keys.len(), 2, "only role and rect per element");
    }
}
