//! Scenario definitions (`bench/scenarios.toml`) and timing patterns.
//!
//! A scenario says which app window to benchmark and how to open it in
//! a fixed state. A pattern says how the user behaves around the hotkey
//! press. Both are about what the user does, not how jumpkey works, so
//! they mean the same thing before and after a refactor.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize)]
pub struct ScenarioFile {
    #[serde(default)]
    pub defaults: Defaults,
    #[serde(rename = "scenario", default)]
    pub scenarios: Vec<Scenario>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Defaults {
    /// Window rect `[x, y, width, height]` in physical pixels.
    #[serde(default = "default_rect")]
    pub rect: [i32; 4],
    /// Time to let the app finish loading after it's placed.
    #[serde(default = "default_settle")]
    pub settle_ms: u64,
}

impl Default for Defaults {
    fn default() -> Self {
        Self {
            rect: default_rect(),
            settle_ms: default_settle(),
        }
    }
}

fn default_rect() -> [i32; 4] {
    [80, 80, 1600, 1000]
}

fn default_settle() -> u64 {
    2000
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Scenario {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Candidate executable paths; the first that exists is used.
    /// Placeholders: `{fixtures}`, `{fixtures_url}`, `{repo}`, `{ENV_VAR}`.
    pub exe: Vec<String>,
    #[serde(default)]
    pub args: Vec<String>,
    /// Image name of the process that owns the window (may differ from
    /// `exe`, e.g. `explorer.exe` for a folder window).
    pub window_exe: String,
    pub rect: Option<[i32; 4]>,
    pub settle_ms: Option<u64>,
    /// Close the window afterwards (only if the harness opened it).
    #[serde(default = "yes")]
    pub close: bool,
}

fn yes() -> bool {
    true
}

impl ScenarioFile {
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let f: Self =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        if f.scenarios.is_empty() {
            bail!("{} defines no [[scenario]] entries", path.display());
        }
        Ok(f)
    }

    pub fn get(&self, name: &str) -> Result<&Scenario> {
        self.scenarios
            .iter()
            .find(|s| s.name == name)
            .with_context(|| {
                let names: Vec<_> = self.scenarios.iter().map(|s| s.name.as_str()).collect();
                format!("unknown scenario `{name}`; available: {}", names.join(", "))
            })
    }
}

/// Values substituted into scenario strings.
pub struct Placeholders {
    pub fixtures: PathBuf,
    pub repo: PathBuf,
    /// Root for throwaway app profiles (`{profile}`); one subfolder per
    /// scenario. Set once the run folder exists.
    pub profiles: Option<PathBuf>,
}

impl Placeholders {
    pub fn expand(&self, s: &str) -> String {
        let fixtures = self.fixtures.display().to_string();
        let fixtures_url = format!("file:///{}", fixtures.replace('\\', "/"));
        let mut out = s
            .replace("{fixtures_url}", &fixtures_url)
            .replace("{fixtures}", &fixtures)
            .replace("{repo}", &self.repo.display().to_string());
        // Remaining `{NAME}` → environment variable, left as-is if unset.
        while let Some(start) = out.find('{') {
            let Some(len) = out[start..].find('}') else {
                break;
            };
            let name = &out[start + 1..start + len];
            match std::env::var(name) {
                Ok(v) => out.replace_range(start..=start + len, &v),
                Err(_) => break,
            }
        }
        out
    }
}

impl Scenario {
    /// The first candidate executable that exists on this machine.
    pub fn resolve_exe(&self, ph: &Placeholders) -> Option<PathBuf> {
        self.exe.iter().find_map(|e| {
            let p = PathBuf::from(ph.expand(e));
            if p.is_file() {
                Some(p)
            } else if p.components().count() == 1 {
                which(&p)
            } else {
                None
            }
        })
    }

    /// Arguments with placeholders expanded. `{profile}` becomes a fresh
    /// per-run folder for this scenario, so a browser starts with a clean
    /// profile in its own process tree (no tabs, extensions or history
    /// from the user's real profile).
    pub fn args(&self, ph: &Placeholders) -> Vec<String> {
        let profile = self.profile_dir(ph);
        self.args
            .iter()
            .map(|a| {
                let a = match &profile {
                    Some(p) => a.replace("{profile}", &p.display().to_string()),
                    None => a.clone(),
                };
                ph.expand(&a)
            })
            .collect()
    }

    /// The throwaway profile folder, if this scenario uses `{profile}`.
    pub fn profile_dir(&self, ph: &Placeholders) -> Option<PathBuf> {
        if !self.args.iter().any(|a| a.contains("{profile}")) {
            return None;
        }
        ph.profiles.as_ref().map(|root| root.join(&self.name))
    }
}

/// Look up a bare program name on PATH (e.g. `explorer.exe`, `wt.exe`).
fn which(name: &Path) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

/// How the user behaves around each hotkey press.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Pattern {
    /// Focus another window, focus the target, wait `dwell_ms`, press.
    /// 2000 ms (the default) is the everyday case: switch, read a bit,
    /// press. 10000 ms is "reading the page first", which today is a cold
    /// search but becomes a cache hit if entries live longer (freshness
    /// redesign). 0 and 300 ms are stress cases: the press races the
    /// background cache-warming search of the same window.
    SwitchThenPress { dwell_ms: u64 },
    /// Press again `gap_ms` after the previous overlay closed, without
    /// switching windows (the "Esc, retry" case).
    RepeatPress { gap_ms: u64 },
}

impl Pattern {
    pub fn parse(s: &str) -> Result<Self> {
        let (name, arg) = s.split_once(':').unwrap_or((s, ""));
        let ms = |default: u64| -> Result<u64> {
            if arg.is_empty() {
                Ok(default)
            } else {
                arg.parse()
                    .with_context(|| format!("bad milliseconds in pattern `{s}`"))
            }
        };
        Ok(match name {
            "switch-then-press" | "switch" => Self::SwitchThenPress {
                dwell_ms: ms(2000)?,
            },
            "repeat-press" | "repeat" => Self::RepeatPress { gap_ms: ms(300)? },
            _ => bail!(
                "unknown pattern `{s}` (expected switch-then-press:<ms> or repeat-press:<ms>)"
            ),
        })
    }

    /// The patterns run by `--all`.
    /// `--preset baseline`: the patterns that matter for the planned
    /// cache/walk refactors, in ~30 minutes. 300 ms races the
    /// cache-warming walk after a window switch, 2000 ms is a cold
    /// search (the warmed entry has expired), and repeat-press:300
    /// presses again within the 500 ms cache lifetime.
    pub fn baseline_set() -> Vec<Self> {
        vec![
            Self::SwitchThenPress { dwell_ms: 300 },
            Self::SwitchThenPress { dwell_ms: 2000 },
            Self::RepeatPress { gap_ms: 300 },
        ]
    }

    pub fn standard_set() -> Vec<Self> {
        vec![
            Self::SwitchThenPress { dwell_ms: 0 },
            Self::SwitchThenPress { dwell_ms: 300 },
            Self::SwitchThenPress { dwell_ms: 2000 },
            Self::SwitchThenPress { dwell_ms: 10000 },
            Self::RepeatPress { gap_ms: 300 },
        ]
    }
}

impl std::fmt::Display for Pattern {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SwitchThenPress { dwell_ms } => write!(f, "switch-then-press:{dwell_ms}"),
            Self::RepeatPress { gap_ms } => write!(f, "repeat-press:{gap_ms}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_patterns() {
        assert_eq!(
            Pattern::parse("switch-then-press:2000").unwrap(),
            Pattern::SwitchThenPress { dwell_ms: 2000 }
        );
        assert_eq!(
            Pattern::parse("repeat").unwrap(),
            Pattern::RepeatPress { gap_ms: 300 }
        );
        assert!(Pattern::parse("wiggle:5").is_err());
        assert!(Pattern::parse("repeat:abc").is_err());
        let p = Pattern::SwitchThenPress { dwell_ms: 300 };
        assert_eq!(Pattern::parse(&p.to_string()).unwrap(), p);
    }

    #[test]
    fn expands_placeholders() {
        std::env::set_var("PICK_BENCH_TEST_VAR", "X");
        let ph = Placeholders {
            fixtures: PathBuf::from(r"C:\repo\bench\fixtures"),
            repo: PathBuf::from(r"C:\repo"),
            profiles: None,
        };
        assert_eq!(
            ph.expand("{fixtures_url}/a.html"),
            "file:///C:/repo/bench/fixtures/a.html"
        );
        assert_eq!(
            ph.expand(r"{fixtures}\folder"),
            r"C:\repo\bench\fixtures\folder"
        );
        assert_eq!(ph.expand("{PICK_BENCH_TEST_VAR}/y"), "X/y");
        assert_eq!(
            ph.expand("{PICK_BENCH_UNSET_VAR}"),
            "{PICK_BENCH_UNSET_VAR}"
        );
    }

    #[test]
    fn profile_placeholder_is_per_scenario() {
        let ph = Placeholders {
            fixtures: PathBuf::from(r"C:\f"),
            repo: PathBuf::from(r"C:\r"),
            profiles: Some(PathBuf::from(r"C:\run\profiles")),
        };
        let sc: Scenario = toml::from_str(
            r#"
            name = "dense-list"
            exe = ["x.exe"]
            args = ["--user-data-dir={profile}", "{fixtures_url}/a.html"]
            window_exe = "x.exe"
            "#,
        )
        .unwrap();
        assert_eq!(
            sc.args(&ph),
            vec![
                r"--user-data-dir=C:\run\profiles\dense-list".to_string(),
                "file:///C:/f/a.html".to_string()
            ]
        );
        assert_eq!(
            sc.profile_dir(&ph),
            Some(PathBuf::from(r"C:\run\profiles\dense-list"))
        );
        let no_profile: Scenario =
            toml::from_str("name = \"e\"\nexe = [\"explorer.exe\"]\nwindow_exe = \"explorer.exe\"")
                .unwrap();
        assert_eq!(no_profile.profile_dir(&ph), None);
    }

    #[test]
    fn bundled_scenarios_parse() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../bench/scenarios.toml");
        let f = ScenarioFile::load(&path).unwrap();
        assert!(f.get("dense-list").is_ok());
        for s in &f.scenarios {
            assert!(!s.exe.is_empty(), "{} has no exe", s.name);
        }
    }
}
