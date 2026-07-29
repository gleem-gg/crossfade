//! Per-channel effect processing outside the PulseAudio API.
//!
//! Two kinds of helper processes are managed here, both children of the app
//! (and tied to its lifetime via PR_SET_PDEATHSIG):
//!
//! * The LV2 chain: a `pipewire -c <generated conf>` process hosting
//!   `libpipewire-module-filter-chain`. It exposes a sink
//!   (`Crossfade_Ch<id>_FX`) and a source (`Crossfade_Ch<id>_FXOut`) that the
//!   regular loopback plumbing in `audio.rs` routes through. Control-port
//!   changes are applied live with `pw-cli set-param <node> Props …`.
//!
//! * The VST rack: a headless helper (`data/vsthost.py`, embedded and run
//!   with python3) hosting VST2/VST3 plugins through Carla's engine
//!   library — no Carla UI anywhere; Crossfade's own dialog edits the
//!   parameters over a JSON-lines pipe. The helper registers as a JACK
//!   client named `Crossfade_Ch<id>_VST` and is wired in front of the chain
//!   sink with `pw-link` against a dedicated null sink's monitor, because
//!   JACK clients are invisible to the PulseAudio API. Structural changes
//!   (add/remove/reorder/enable) respawn the helper with the new plugin
//!   set; parameter changes are sent live.

use std::cell::Cell;
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::os::fd::IntoRawFd;
use std::path::PathBuf;
use std::process::{ChildStdin, Command, Stdio};
use std::rc::Rc;
use std::time::Instant;

use gtk::{gio, glib};

use crate::config::ChannelConfig;
use crate::lv2::Catalog;

const VSTHOST_SCRIPT: &str = include_str!("../data/vsthost.py");

pub fn chain_sink_name(id: u64) -> String {
    format!("Crossfade_Ch{id}_FX")
}

pub fn chain_source_name(id: u64) -> String {
    format!("Crossfade_Ch{id}_FXOut")
}

pub fn vstin_sink_name(id: u64) -> String {
    format!("Crossfade_Ch{id}_VSTIn")
}

pub fn vst_node_name(id: u64) -> String {
    format!("Crossfade_Ch{id}_VST")
}

/// Node labels inside the filter graph for one effect ("fx<id>", or one per
/// stereo lane when the plugin is mono and instantiated twice).
pub fn effect_labels(effect_id: u64, mono: bool) -> Vec<String> {
    if mono {
        vec![format!("fx{effect_id}_l"), format!("fx{effect_id}_r")]
    } else {
        vec![format!("fx{effect_id}")]
    }
}

#[derive(Clone, Debug)]
pub enum FxEvent {
    /// The chain process for a channel exited without being asked to.
    ChainDied(u64),
    /// The VST host helper for a channel exited (crash) or never became
    /// linkable.
    VstHostDied(u64),
    /// A protocol line arrived from a channel's VST host helper; route it
    /// back into `FxManager::handle_vst_reply`.
    VstReply(u64, String),
}

/// One parameter of a loaded VST plugin, as reported by the helper.
#[derive(Clone, Debug)]
pub struct VstParamInfo {
    pub index: u32,
    pub name: String,
    pub min: f64,
    pub max: f64,
    pub value: f64,
    pub toggled: bool,
    pub integer: bool,
}

#[derive(Clone, Debug)]
pub enum VstState {
    Loading,
    Loaded,
    Failed(String),
}

/// Runtime info for one configured VST plugin (UI reads this to render
/// parameter rows).
#[derive(Clone, Debug)]
pub struct VstRuntime {
    pub cfg_id: u64,
    pub state: VstState,
    /// The plugin ships its own editor window (openable via show_vst_ui).
    pub has_ui: bool,
    pub params: Vec<VstParamInfo>,
}

/// What a helper protocol line amounted to.
#[derive(Default)]
pub struct VstReplyOutcome {
    /// Plugin load results arrived; the effects dialog should refresh.
    pub structure_changed: bool,
    /// Parameter edits made in a plugin's native UI: (cfg_id, index, value).
    /// The caller persists these into the channel config.
    pub params: Vec<(u64, u32, f64)>,
}

struct Slot {
    pid: i32,
    alive: Rc<Cell<bool>>,
    expect_exit: Rc<Cell<bool>>,
    /// Set when the process died shortly after spawning (bad plugin, bad
    /// conf) so callers stop waiting for its nodes.
    failed: Rc<Cell<bool>>,
}

impl Slot {
    fn running(&self) -> bool {
        self.alive.get() && !self.failed.get()
    }

    fn kill(&self) {
        if self.alive.get() {
            self.expect_exit.set(true);
            unsafe {
                // Negative pid: the whole process group (Carla forks).
                libc::kill(-self.pid, libc::SIGTERM);
            }
        }
    }
}

struct ChainSlot {
    slot: Slot,
    conf: String,
    /// Consecutive quick deaths with this exact conf; at 2 we stop
    /// respawning so a plugin that crashes on load can't cause a loop.
    fails: u32,
}

struct VstSlot {
    slot: Slot,
    stdin: Option<ChildStdin>,
    /// Signature of the enabled plugin set the helper was started with;
    /// a different set means respawn.
    desc: String,
    runtime: Vec<VstRuntime>,
    /// Consecutive quick deaths with this plugin set; at 2 we stop
    /// respawning so a crashing plugin can't cause a loop.
    fails: u32,
    /// Cancellation flag for the link-poll timeout. The poll source removes
    /// itself (returns Break) when this is set — never remove a stored
    /// SourceId here, a source that already broke would panic on remove.
    link_cancel: Option<Rc<Cell<bool>>>,
    /// Same pattern for the stdout-reader poll.
    reader_cancel: Rc<Cell<bool>>,
}

impl Drop for VstSlot {
    fn drop(&mut self) {
        if let Some(c) = self.link_cancel.take() {
            c.set(true);
        }
        self.reader_cancel.set(true);
    }
}

#[derive(Default)]
pub struct FxManager {
    chains: HashMap<u64, ChainSlot>,
    vsts: HashMap<u64, VstSlot>,
    handler: Option<Rc<dyn Fn(FxEvent)>>,
}

fn fx_dir() -> PathBuf {
    glib::user_config_dir().join("gleem-crossfade").join("fx")
}

/// Per-channel directory for full VST plugin state (chunk data captured
/// when a plugin's own editor window is closed, restored on load).
fn vst_state_dir(id: u64) -> PathBuf {
    glib::user_config_dir()
        .join("gleem-crossfade")
        .join("vst-state")
        .join(format!("ch{id}"))
}

fn sanitize_token(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        .collect()
}

fn sanitize_uri(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_whitespace() && !matches!(c, '"' | '\'' | '\\'))
        .collect()
}

/// Spawn a child in its own process group that dies with us.
fn spawn_child(argv: &[String], envs: &[(String, String)], log: &str) -> Option<i32> {
    use std::os::unix::process::CommandExt;
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let out = fs::File::create(glib::user_runtime_dir().join(log)).ok();
    cmd.stdin(Stdio::null());
    match out {
        Some(f) => {
            if let Ok(f2) = f.try_clone() {
                cmd.stdout(Stdio::from(f));
                cmd.stderr(Stdio::from(f2));
            } else {
                cmd.stdout(Stdio::from(f));
                cmd.stderr(Stdio::null());
            }
        }
        None => {
            cmd.stdout(Stdio::null());
            cmd.stderr(Stdio::null());
        }
    }
    cmd.process_group(0);
    unsafe {
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
    cmd.spawn().ok().map(|c| c.id() as i32)
}

impl FxManager {
    pub fn set_event_handler(&mut self, f: impl Fn(FxEvent) + 'static) {
        self.handler = Some(Rc::new(f));
    }

    fn watch(&self, pid: i32, slot: &Slot, event: FxEvent, min_uptime_failed: bool) {
        let alive = slot.alive.clone();
        let expect = slot.expect_exit.clone();
        let failed = slot.failed.clone();
        let handler = self.handler.clone();
        let started = Instant::now();
        glib::child_watch_add_local(glib::Pid(pid), move |_, _| {
            alive.set(false);
            if expect.get() {
                return;
            }
            if min_uptime_failed && started.elapsed().as_secs() < 5 {
                failed.set(true);
            }
            if let Some(h) = &handler {
                h(event.clone());
            }
        });
    }

    // ---- LV2 chain ---------------------------------------------------------

    /// Make sure the chain process for this channel runs `conf` (respawning
    /// on content changes), or kill it when `conf` is None.
    pub fn ensure_chain(&mut self, id: u64, conf: Option<String>) {
        let Some(conf) = conf else {
            if let Some(c) = self.chains.remove(&id) {
                c.slot.kill();
            }
            return;
        };
        let mut fails = 0;
        if let Some(c) = self.chains.get(&id)
            && c.conf == conf {
                if c.slot.running() {
                    return;
                }
                if c.slot.failed.get() {
                    fails = c.fails + 1;
                    if fails >= 2 {
                        return; // known-bad conf; keep the dead slot as a marker
                    }
                }
            }
        if let Some(c) = self.chains.remove(&id) {
            c.slot.kill();
        }
        let dir = fx_dir();
        let _ = fs::create_dir_all(&dir);
        let path = dir.join(format!("ch{id}.conf"));
        if fs::write(&path, &conf).is_err() {
            return;
        }
        let slot = Slot {
            pid: 0,
            alive: Rc::new(Cell::new(true)),
            expect_exit: Rc::new(Cell::new(false)),
            failed: Rc::new(Cell::new(false)),
        };
        let Some(pid) = spawn_child(
            &[
                "pipewire".to_string(),
                "-c".to_string(),
                path.display().to_string(),
            ],
            &[],
            &format!("gleem-crossfade-fx-ch{id}.log"),
        ) else {
            return;
        };
        let slot = Slot { pid, ..slot };
        self.watch(pid, &slot, FxEvent::ChainDied(id), true);
        self.chains.insert(id, ChainSlot { slot, conf, fails });
    }

    pub fn chain_running(&self, id: u64) -> bool {
        self.chains.get(&id).is_some_and(|c| c.slot.running())
    }

    /// The chain died right after spawning; callers waiting for its nodes
    /// should wire the channel without it.
    pub fn chain_failed(&self, id: u64) -> bool {
        self.chains.get(&id).is_none_or(|c| !c.slot.running())
    }

    /// Apply a control-port value to the live chain of a channel.
    pub fn set_controls(&self, channel: u64, labels: &[String], symbol: &str, value: f64) {
        if !self.chain_running(channel) {
            return;
        }
        let node = chain_sink_name(channel);
        let symbol = sanitize_token(symbol);
        let entries: String = labels
            .iter()
            .map(|l| format!("\"{}:{}\" {:.6} ", sanitize_token(l), symbol, value))
            .collect();
        let cmd = format!(
            "pw-cli set-param {node} Props '{{ params = [ {entries}] }}'"
        );
        let _ = glib::spawn_command_line_async(cmd);
    }

    // ---- VST rack (headless helper) -----------------------------------------

    pub fn vst_running(&self, id: u64) -> bool {
        self.vsts.get(&id).is_some_and(|c| c.slot.running())
    }

    /// Runtime plugin/parameter info for the UI.
    pub fn vst_runtime(&self, id: u64) -> Vec<VstRuntime> {
        self.vsts
            .get(&id)
            .map(|s| s.runtime.clone())
            .unwrap_or_default()
    }

    pub fn kill_vst(&mut self, id: u64) {
        if let Some(c) = self.vsts.remove(&id) {
            c.slot.kill();
        }
    }

    /// Make sure a helper hosting exactly the channel's enabled VST plugins
    /// runs (respawning when the set changed); returns whether one runs.
    pub fn ensure_vst_host(&mut self, id: u64, cfg: &ChannelConfig) -> bool {
        let enabled = cfg.enabled_vsts();
        if enabled.is_empty() {
            self.kill_vst(id);
            return false;
        }
        let desc: String = enabled
            .iter()
            .map(|p| {
                format!(
                    "{}\x1f{}\x1f{}\x1f{}\x1f{}",
                    p.id,
                    p.path,
                    p.label,
                    p.unique_id,
                    p.format.as_str()
                )
            })
            .collect::<Vec<_>>()
            .join("\x1e");
        let mut fails = 0;
        if let Some(s) = self.vsts.get(&id)
            && s.desc == desc {
                if s.slot.running() {
                    return true;
                }
                if s.slot.failed.get() {
                    fails = s.fails + 1;
                    if fails >= 2 {
                        return false; // known-bad set; wire without the rack
                    }
                }
            }
        self.kill_vst(id);

        let dir = fx_dir();
        let _ = fs::create_dir_all(&dir);
        let script = dir.join("vsthost.py");
        if fs::write(&script, VSTHOST_SCRIPT).is_err() {
            return false;
        }
        let Some(python) = glib::find_program_in_path("python3") else {
            return false;
        };
        let state_dir = vst_state_dir(id);
        let _ = fs::create_dir_all(&state_dir);
        let slot = Slot {
            pid: 0,
            alive: Rc::new(Cell::new(true)),
            expect_exit: Rc::new(Cell::new(false)),
            failed: Rc::new(Cell::new(false)),
        };
        let Some((pid, mut stdin, stdout_fd)) = spawn_child_piped(
            &[
                python.display().to_string(),
                script.display().to_string(),
                vst_node_name(id),
                state_dir.display().to_string(),
            ],
            &format!("gleem-crossfade-vst-ch{id}.log"),
        ) else {
            return false;
        };
        let slot = Slot { pid, ..slot };
        self.watch(pid, &slot, FxEvent::VstHostDied(id), true);

        // Forward every protocol line back through the event handler (which
        // re-enters handle_vst_reply outside of any Inner borrow).
        let reader_cancel = Rc::new(Cell::new(false));
        if let Some(handler) = self.handler.clone() {
            watch_lines(stdout_fd, reader_cancel.clone(), move |line| {
                handler(FxEvent::VstReply(id, line))
            });
        }

        let plugins: Vec<serde_json::Value> = enabled
            .iter()
            .map(|p| {
                serde_json::json!({
                    "cfg_id": p.id,
                    "path": p.path,
                    "format": p.format.as_str(),
                    "name": p.name,
                    "label": p.label,
                    "unique_id": p.unique_id,
                    "active": true,
                    "params": p.params,
                })
            })
            .collect();
        let msg = serde_json::json!({ "cmd": "load", "plugins": plugins });
        let _ = writeln!(stdin, "{msg}");
        let runtime = enabled
            .iter()
            .map(|p| VstRuntime {
                cfg_id: p.id,
                state: VstState::Loading,
                has_ui: false,
                params: Vec::new(),
            })
            .collect();
        self.vsts.insert(
            id,
            VstSlot {
                slot,
                stdin: Some(stdin),
                desc,
                runtime,
                fails,
                link_cancel: None,
                reader_cancel,
            },
        );
        true
    }

    /// Push a parameter value into the running rack.
    pub fn set_vst_param(&mut self, id: u64, cfg_id: u64, index: u32, value: f64) {
        let Some(slot) = self.vsts.get_mut(&id) else {
            return;
        };
        if let Some(stdin) = slot.stdin.as_mut() {
            let msg = serde_json::json!({
                "cmd": "set", "cfg_id": cfg_id, "param": index, "value": value,
            });
            let _ = writeln!(stdin, "{msg}");
        }
        // Mirror into the runtime cache so a dialog rebuild shows the value.
        if let Some(rt) = slot.runtime.iter_mut().find(|r| r.cfg_id == cfg_id)
            && let Some(p) = rt.params.iter_mut().find(|p| p.index == index) {
                p.value = value;
            }
    }

    /// Process a protocol line from a helper.
    pub fn handle_vst_reply(&mut self, id: u64, line: &str) -> VstReplyOutcome {
        let mut outcome = VstReplyOutcome::default();
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            return outcome;
        };
        let Some(slot) = self.vsts.get_mut(&id) else {
            return outcome;
        };
        match v["reply"].as_str() {
            Some("loaded") => {
                for item in v["plugins"].as_array().into_iter().flatten() {
                    let Some(cfg_id) = item["cfg_id"].as_u64() else {
                        continue;
                    };
                    let Some(rt) = slot.runtime.iter_mut().find(|r| r.cfg_id == cfg_id)
                    else {
                        continue;
                    };
                    if item["ok"].as_bool() == Some(true) {
                        rt.state = VstState::Loaded;
                        rt.has_ui = item["has_ui"].as_bool().unwrap_or(false);
                        rt.params = item["params"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|p| {
                                Some(VstParamInfo {
                                    index: p["index"].as_u64()? as u32,
                                    name: p["name"]
                                        .as_str()
                                        .unwrap_or("Parameter")
                                        .to_string(),
                                    min: p["min"].as_f64().unwrap_or(0.0),
                                    max: p["max"].as_f64().unwrap_or(1.0),
                                    value: p["value"].as_f64().unwrap_or(0.0),
                                    toggled: p["toggled"].as_bool().unwrap_or(false),
                                    integer: p["integer"].as_bool().unwrap_or(false),
                                })
                            })
                            .collect();
                    } else {
                        rt.state = VstState::Failed(
                            item["error"]
                                .as_str()
                                .unwrap_or("could not load plugin")
                                .to_string(),
                        );
                    }
                }
                outcome.structure_changed = true;
            }
            Some("param") => {
                // Edited in the plugin's native window; mirror + persist.
                if let (Some(cfg_id), Some(index), Some(value)) = (
                    v["cfg_id"].as_u64(),
                    v["param"].as_u64(),
                    v["value"].as_f64(),
                ) {
                    let index = index as u32;
                    if let Some(rt) =
                        slot.runtime.iter_mut().find(|r| r.cfg_id == cfg_id)
                        && let Some(p) = rt.params.iter_mut().find(|p| p.index == index)
                        {
                            p.value = value;
                        }
                    outcome.params.push((cfg_id, index, value));
                }
            }
            _ => {}
        }
        outcome
    }

    /// Open (or close) a plugin's own editor window inside the helper.
    pub fn show_vst_ui(&mut self, id: u64, cfg_id: u64, on: bool) {
        let Some(slot) = self.vsts.get_mut(&id) else {
            return;
        };
        if let Some(stdin) = slot.stdin.as_mut() {
            let msg = serde_json::json!({ "cmd": "show_ui", "cfg_id": cfg_id, "on": on });
            let _ = writeln!(stdin, "{msg}");
        }
    }

    /// Keep trying to wire VSTIn.monitor → VST host → chain sink until the
    /// helper's JACK ports exist. `pw-link` exits with "File exists" once a
    /// link is up, which counts as success; "No such object" means the node
    /// is not there yet.
    pub fn start_vst_links(&mut self, id: u64) {
        let Some(c) = self.vsts.get_mut(&id) else {
            return;
        };
        if let Some(old) = c.link_cancel.take() {
            old.set(true);
        }
        let cancel = Rc::new(Cell::new(false));
        c.link_cancel = Some(cancel.clone());
        let vstin = vstin_sink_name(id);
        let vst = vst_node_name(id);
        let sink = chain_sink_name(id);
        let script = format!(
            "pw-link '{vstin}:monitor_FL' '{vst}:audio-in1' 2>&1; \
             pw-link '{vstin}:monitor_FR' '{vst}:audio-in2' 2>&1; \
             pw-link '{vst}:audio-out1' '{sink}:playback_FL' 2>&1; \
             pw-link '{vst}:audio-out2' '{sink}:playback_FR' 2>&1; \
             exit 0"
        );
        let alive = c.slot.alive.clone();
        let handler = self.handler.clone();
        let tries = Rc::new(Cell::new(0u32));
        let done = Rc::new(Cell::new(false));
        glib::timeout_add_local(std::time::Duration::from_millis(800), move || {
            if cancel.get() || done.get() || !alive.get() {
                return glib::ControlFlow::Break;
            }
            tries.set(tries.get() + 1);
            if tries.get() > 40 {
                // The node never became linkable; give up so the channel
                // can be rewired without the rack.
                if let Some(h) = &handler {
                    h(FxEvent::VstHostDied(id));
                }
                return glib::ControlFlow::Break;
            }
            let proc = gio::Subprocess::newv(
                &["sh".as_ref(), "-c".as_ref(), script.as_ref()],
                gio::SubprocessFlags::STDOUT_PIPE | gio::SubprocessFlags::STDERR_MERGE,
            );
            if let Ok(proc) = proc {
                let done = done.clone();
                proc.communicate_utf8_async(
                    None,
                    None::<&gio::Cancellable>,
                    move |res| {
                        if let Ok((Some(out), _)) = res
                            && !out.contains("No such object") {
                                done.set(true);
                            }
                    },
                );
            }
            glib::ControlFlow::Continue
        });
    }

    // ---- Lifecycle ---------------------------------------------------------

    pub fn remove_channel(&mut self, id: u64) {
        self.ensure_chain(id, None);
        self.kill_vst(id);
        let _ = fs::remove_file(fx_dir().join(format!("ch{id}.conf")));
        let _ = fs::remove_dir_all(vst_state_dir(id));
    }

    pub fn shutdown_all(&mut self) {
        for (_, c) in self.chains.drain() {
            c.slot.kill();
        }
        for (_, c) in self.vsts.drain() {
            c.slot.kill();
        }
    }
}

/// Like `spawn_child` but with piped stdin/stdout for the JSON protocol
/// (stderr still goes to the log file).
fn spawn_child_piped(argv: &[String], log: &str) -> Option<(i32, ChildStdin, i32)> {
    use std::os::unix::process::CommandExt;
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    match fs::File::create(glib::user_runtime_dir().join(log)) {
        Ok(f) => {
            cmd.stderr(Stdio::from(f));
        }
        Err(_) => {
            cmd.stderr(Stdio::null());
        }
    }
    cmd.process_group(0);
    unsafe {
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
    let mut child = cmd.spawn().ok()?;
    let stdin = child.stdin.take()?;
    let stdout = child.stdout.take()?;
    Some((child.id() as i32, stdin, stdout.into_raw_fd()))
}

/// Deliver every line arriving on `fd` to `on_line`, polled from a main-loop
/// timer (this glib version exposes no fd-watch API). Owns the fd; closes it
/// and removes itself at EOF or when `cancel` is set.
fn watch_lines(fd: i32, cancel: Rc<Cell<bool>>, on_line: impl Fn(String) + 'static) {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
    let buf = std::cell::RefCell::new(Vec::<u8>::new());
    glib::timeout_add_local(std::time::Duration::from_millis(100), move || {
        if cancel.get() {
            unsafe {
                libc::close(fd);
            }
            return glib::ControlFlow::Break;
        }
        let mut tmp = [0u8; 4096];
        let eof = loop {
            let n =
                unsafe { libc::read(fd, tmp.as_mut_ptr() as *mut libc::c_void, tmp.len()) };
            if n > 0 {
                buf.borrow_mut().extend_from_slice(&tmp[..n as usize]);
            } else {
                break n == 0;
            }
        };
        loop {
            let line = {
                let mut b = buf.borrow_mut();
                let Some(pos) = b.iter().position(|&c| c == b'\n') else {
                    break;
                };
                let raw: Vec<u8> = b.drain(..=pos).collect();
                String::from_utf8(raw).ok()
            };
            if let Some(line) = line {
                let line = line.trim().to_string();
                if !line.is_empty() {
                    on_line(line);
                }
            }
        }
        if eof {
            unsafe {
                libc::close(fd);
            }
            return glib::ControlFlow::Break;
        }
        glib::ControlFlow::Continue
    });
}

/// Build the `pipewire -c` configuration hosting this channel's filter
/// chain. Returns None when the channel needs no chain at all.
pub fn chain_conf(id: u64, cfg: &ChannelConfig, catalog: Option<&Catalog>) -> Option<String> {
    if !cfg.fx_active() {
        return None;
    }

    // (declaration lines, [left in, right in], [left out, right out])
    let mut nodes = String::new();
    let mut stages: Vec<([String; 2], [String; 2])> = Vec::new();
    for e in cfg.enabled_effects() {
        let Some(info) = catalog.and_then(|c| c.find(&e.uri)) else {
            // Plugin not installed (anymore): skip it, keep the chain alive.
            continue;
        };
        let uri = sanitize_uri(&e.uri);
        let controls: String = e
            .controls
            .iter()
            .filter(|(s, _)| info.controls.iter().any(|c| c.symbol.as_str() == s.as_str()))
            .map(|(s, v)| format!("\"{}\" = {:.6} ", sanitize_token(s), v))
            .collect();
        let control_block = if controls.is_empty() {
            String::new()
        } else {
            format!(" control = {{ {controls}}}")
        };
        if info.is_mono() {
            let (l, r) = (format!("fx{}_l", e.id), format!("fx{}_r", e.id));
            for n in [&l, &r] {
                nodes.push_str(&format!(
                    "                    {{ type = lv2 name = \"{n}\" plugin = \"{uri}\"{control_block} }}\n"
                ));
            }
            stages.push((
                [
                    format!("{l}:{}", sanitize_token(&info.audio_in[0])),
                    format!("{r}:{}", sanitize_token(&info.audio_in[0])),
                ],
                [
                    format!("{l}:{}", sanitize_token(&info.audio_out[0])),
                    format!("{r}:{}", sanitize_token(&info.audio_out[0])),
                ],
            ));
        } else {
            let n = format!("fx{}", e.id);
            nodes.push_str(&format!(
                "                    {{ type = lv2 name = \"{n}\" plugin = \"{uri}\"{control_block} }}\n"
            ));
            stages.push((
                [
                    format!("{n}:{}", sanitize_token(&info.audio_in[0])),
                    format!("{n}:{}", sanitize_token(&info.audio_in[1])),
                ],
                [
                    format!("{n}:{}", sanitize_token(&info.audio_out[0])),
                    format!("{n}:{}", sanitize_token(&info.audio_out[1])),
                ],
            ));
        }
    }
    if stages.is_empty() {
        // Carla-only (or all plugins missing): pass audio through unchanged.
        for n in ["copy_l", "copy_r"] {
            nodes.push_str(&format!(
                "                    {{ type = builtin name = {n} label = copy }}\n"
            ));
        }
        stages.push((
            ["copy_l:In".to_string(), "copy_r:In".to_string()],
            ["copy_l:Out".to_string(), "copy_r:Out".to_string()],
        ));
    }

    let mut links = String::new();
    for w in stages.windows(2) {
        for ch in 0..2 {
            links.push_str(&format!(
                "                    {{ output = \"{}\" input = \"{}\" }}\n",
                w[0].1[ch], w[1].0[ch]
            ));
        }
    }
    let first = &stages.first().unwrap().0;
    let last = &stages.last().unwrap().1;
    let sink = chain_sink_name(id);
    let source = chain_source_name(id);

    Some(format!(
        r#"# Generated by Crossfade; do not edit (rewritten on every change).
context.properties = {{
    log.level = 2
}}

context.spa-libs = {{
    audio.convert.* = audioconvert/libspa-audioconvert
    support.*       = support/libspa-support
}}

context.modules = [
    {{ name = libpipewire-module-rt
        args = {{ nice.level = -11 }}
        flags = [ ifexists nofail ]
    }}
    {{ name = libpipewire-module-protocol-native }}
    {{ name = libpipewire-module-client-node }}
    {{ name = libpipewire-module-adapter }}

    {{ name = libpipewire-module-filter-chain
        args = {{
            node.description = "Crossfade Channel {id} Effects"
            media.name       = "Crossfade Channel {id} Effects"
            filter.graph = {{
                nodes = [
{nodes}                ]
                links = [
{links}                ]
                inputs  = [ "{in_l}" "{in_r}" ]
                outputs = [ "{out_l}" "{out_r}" ]
            }}
            audio.channels = 2
            audio.position = [ FL FR ]
            capture.props = {{
                node.name           = "{sink}"
                node.description    = "Crossfade Ch {id} FX (internal)"
                media.class         = Audio/Sink
                state.restore-props = false
            }}
            playback.props = {{
                node.name           = "{source}"
                node.description    = "Crossfade Ch {id} FX Out (internal)"
                media.class         = Audio/Source
                state.restore-props = false
            }}
        }}
    }}
]
"#,
        in_l = first[0],
        in_r = first[1],
        out_l = last[0],
        out_r = last[1],
    ))
}
