//! VST2/VST3 plugin discovery.
//!
//! Plugins are collected from the conventional Linux folders (plus
//! `~/vst`, `$VST_PATH` and `$VST3_PATH`) and identified with Carla's
//! `carla-discovery-native` tool, which safely loads each binary in a
//! throwaway process and reports the plugins inside — essential for VST3
//! bundles that contain many plugins behind one file. Results are cached
//! in `~/.cache/gleem-crossfade/vst-scan-v2.json` keyed by path + mtime, so only
//! new or changed binaries are probed on later scans.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use gtk::glib;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VstFormat {
    Vst2,
    Vst3,
}

impl VstFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            VstFormat::Vst2 => "vst2",
            VstFormat::Vst3 => "vst3",
        }
    }
}

/// One loadable plugin (a VST3 bundle yields one entry per contained
/// plugin, distinguished by `label`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VstEntry {
    pub path: String,
    pub format: VstFormat,
    /// Sub-plugin label inside the binary; empty for plain VST2 files.
    pub label: String,
    /// Carla selects sub-plugins in multi-plugin binaries (VST3 bundles,
    /// VST2 shells) by this id, not by label.
    pub unique_id: i64,
    pub name: String,
    pub audio_ins: u32,
    pub audio_outs: u32,
}

#[derive(Default, Serialize, Deserialize)]
struct CacheFile {
    /// binary path -> (mtime seconds, discovered plugins)
    entries: HashMap<String, (i64, Vec<VstEntry>)>,
}

fn cache_path() -> PathBuf {
    // v2: entries carry unique_id.
    glib::user_cache_dir().join("gleem-crossfade").join("vst-scan-v2.json")
}

fn discovery_tool() -> Option<PathBuf> {
    [
        "/usr/lib64/carla/carla-discovery-native",
        "/usr/lib/carla/carla-discovery-native",
        "/usr/lib/x86_64-linux-gnu/carla/carla-discovery-native",
        "/usr/local/lib/carla/carla-discovery-native",
    ]
    .iter()
    .map(PathBuf::from)
    .find(|p| p.exists())
}

fn carla_backend_present() -> bool {
    ["/usr/share/carla/carla_backend.py", "/usr/local/share/carla/carla_backend.py"]
        .iter()
        .any(|p| Path::new(p).exists())
}

/// Whether VST hosting can work at all (used to hint at the missing
/// dependency in the UI).
pub fn available() -> bool {
    discovery_tool().is_some()
        && carla_backend_present()
        && glib::find_program_in_path("python3").is_some()
}

/// The folders searched, in order.
fn plugin_dirs() -> Vec<(PathBuf, VstFormat)> {
    let home = glib::home_dir();
    let mut dirs: Vec<(PathBuf, VstFormat)> = Vec::new();
    let mut push = |p: PathBuf, f: VstFormat| {
        if p.is_dir() && !dirs.iter().any(|(d, df)| *d == p && *df == f) {
            dirs.push((p, f));
        }
    };
    for (env_var, format) in [("VST_PATH", VstFormat::Vst2), ("VST3_PATH", VstFormat::Vst3)] {
        if let Ok(v) = std::env::var(env_var) {
            for part in v.split(':').filter(|s| !s.is_empty()) {
                push(PathBuf::from(part), format);
            }
        }
    }
    for name in ["vst", ".vst", ".lxvst"] {
        push(home.join(name), VstFormat::Vst2);
    }
    push(home.join(".vst3"), VstFormat::Vst3);
    for lib in ["/usr/lib64", "/usr/lib", "/usr/local/lib", "/usr/local/lib64"] {
        push(PathBuf::from(lib).join("vst"), VstFormat::Vst2);
        push(PathBuf::from(lib).join("lxvst"), VstFormat::Vst2);
        push(PathBuf::from(lib).join("vst3"), VstFormat::Vst3);
    }
    dirs
}

/// Candidate binaries: `.so` files for VST2, `.vst3` bundles (or files)
/// for VST3. One directory level deep, following the common layouts.
fn candidates() -> Vec<(String, VstFormat)> {
    let mut out = Vec::new();
    for (dir, format) in plugin_dirs() {
        let Ok(read) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in read.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            let matches = match format {
                VstFormat::Vst2 => name.ends_with(".so"),
                VstFormat::Vst3 => name.ends_with(".vst3"),
            };
            if matches {
                out.push((path.display().to_string(), format));
            } else if path.is_dir() {
                // e.g. vendor subdirectories with .so files inside.
                if let Ok(sub) = fs::read_dir(&path) {
                    for e in sub.flatten() {
                        let n = e.file_name().to_string_lossy().to_string();
                        let ok = match format {
                            VstFormat::Vst2 => n.ends_with(".so"),
                            VstFormat::Vst3 => n.ends_with(".vst3"),
                        };
                        if ok {
                            out.push((e.path().display().to_string(), format));
                        }
                    }
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

fn mtime_of(path: &str) -> i64 {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Run carla-discovery-native on one binary and parse the plugins found.
fn discover(tool: &Path, path: &str, format: VstFormat) -> Vec<VstEntry> {
    let output = Command::new(tool)
        .arg(format.as_str())
        .arg(path)
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let mut plugins = Vec::new();
    let mut current: HashMap<&str, String> = HashMap::new();
    let mut in_block = false;
    for line in text.lines() {
        let Some(rest) = line.trim().strip_prefix("carla-discovery::") else {
            continue;
        };
        let Some((key, value)) = rest.split_once("::") else {
            continue;
        };
        match key {
            "init" => {
                in_block = true;
                current.clear();
            }
            "end" => {
                if in_block {
                    let name = current.get("name").cloned().unwrap_or_default();
                    let label = current.get("label").cloned().unwrap_or_default();
                    let unique_id: i64 = current
                        .get("uniqueId")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0);
                    let ains: u32 = current
                        .get("audio.ins")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0);
                    let aouts: u32 = current
                        .get("audio.outs")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0);
                    plugins.push(VstEntry {
                        path: path.to_string(),
                        format,
                        label,
                        unique_id,
                        name: if name.is_empty() {
                            Path::new(path)
                                .file_stem()
                                .map(|s| s.to_string_lossy().to_string())
                                .unwrap_or_else(|| path.to_string())
                        } else {
                            name
                        },
                        audio_ins: ains,
                        audio_outs: aouts,
                    });
                }
                in_block = false;
            }
            "name" => {
                current.insert("name", value.to_string());
            }
            "label" => {
                current.insert("label", value.to_string());
            }
            "uniqueId" => {
                current.insert("uniqueId", value.to_string());
            }
            "audio.ins" => {
                current.insert("audio.ins", value.to_string());
            }
            "audio.outs" => {
                current.insert("audio.outs", value.to_string());
            }
            _ => {}
        }
    }
    plugins
}

/// Scan all folders. Blocking; fast when the cache is warm (only new or
/// changed binaries are probed).
pub fn scan() -> Vec<VstEntry> {
    let Some(tool) = discovery_tool() else {
        return Vec::new();
    };
    let cpath = cache_path();
    let mut cache: CacheFile = fs::read_to_string(&cpath)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let candidates = candidates();
    let mut dirty = false;
    let mut result: Vec<VstEntry> = Vec::new();
    for (path, format) in &candidates {
        let mtime = mtime_of(path);
        let cached = cache
            .entries
            .get(path)
            .filter(|(t, _)| *t == mtime)
            .map(|(_, e)| e.clone());
        let entries = match cached {
            Some(e) => e,
            None => {
                let e = discover(&tool, path, *format);
                cache.entries.insert(path.clone(), (mtime, e.clone()));
                dirty = true;
                e
            }
        };
        result.extend(entries);
    }
    // Drop cache rows for binaries that no longer exist.
    let live: std::collections::HashSet<&String> =
        candidates.iter().map(|(p, _)| p).collect();
    let before = cache.entries.len();
    cache.entries.retain(|p, _| live.contains(p));
    if dirty || cache.entries.len() != before {
        if let Some(dir) = cpath.parent() {
            let _ = fs::create_dir_all(dir);
        }
        if let Ok(s) = serde_json::to_string(&cache) {
            let _ = fs::write(&cpath, s);
        }
    }

    // Effects only: something with an audio path through it.
    result.retain(|e| {
        (1..=2).contains(&e.audio_ins) && (1..=2).contains(&e.audio_outs)
    });
    result.sort_by_key(|a| a.name.to_lowercase());
    result
}
