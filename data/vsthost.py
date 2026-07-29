#!/usr/bin/env python3
"""Crossfade VST host helper.

Hosts one channel's VST plugins headlessly through Carla's engine library
(no Carla UI), controlled by Crossfade over a JSON-lines protocol:

  stdin  <- {"cmd": "load", "plugins": [{"cfg_id", "path", "format",
             "label", "active", "params": {"<index>": value}}, ...]}
  stdin  <- {"cmd": "set", "cfg_id": N, "param": I, "value": V}
  stdin  <- {"cmd": "active", "cfg_id": N, "on": true}
  stdin  <- {"cmd": "show_ui", "cfg_id": N, "on": true}
  stdin  <- {"cmd": "quit"}

Replies go to the original stdout (dup'ed before Carla can write its own
logs there):

  {"reply": "ready"}
  {"reply": "loaded", "plugins": [{"cfg_id", "ok", "name", "has_ui",
    "error", "params": [{"index", "name", "min", "max", "def", "value",
    "toggled", "integer"}]}]}
  {"reply": "param", "cfg_id": N, "param": I, "value": V}   (edited in the
    plugin's native UI — Crossfade persists it)
  {"reply": "ui", "cfg_id": N, "visible": false}            (window closed)

The engine registers as a JACK client (PipeWire) named after argv[1], in
continuous-rack mode: fixed stereo in/out ports that Crossfade wires with
pw-link. argv[2] is a directory for per-plugin state files (full plugin
state, including non-parameter data, restored on the next load).
"""

import json
import os
import select
import signal
import sys

CLIENT_NAME = sys.argv[1] if len(sys.argv) > 1 else "Crossfade_VST"
STATE_DIR = sys.argv[2] if len(sys.argv) > 2 else ""

# Protocol writes must not interleave with Carla's own stdout logging:
# keep a private dup of stdout and point fd 1 at stderr before the engine
# library gets a chance to print anything.
PROTO = os.fdopen(os.dup(1), "w", buffering=1)
os.dup2(2, 1)

# Die gracefully (state saved in the finally block) when Crossfade
# terminates us on a rack rebuild or quit.
signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))


def send(obj):
    PROTO.write(json.dumps(obj) + "\n")
    PROTO.flush()


def find_first(paths):
    for p in paths:
        if os.path.exists(p):
            return p
    return None


CARLA_SHARE = find_first([
    "/usr/share/carla",
    "/usr/local/share/carla",
])
CARLA_LIB = find_first([
    "/usr/lib64/carla/libcarla_standalone2.so",
    "/usr/lib/carla/libcarla_standalone2.so",
    "/usr/lib/x86_64-linux-gnu/carla/libcarla_standalone2.so",
    "/usr/local/lib/carla/libcarla_standalone2.so",
])
if not CARLA_SHARE or not CARLA_LIB:
    send({"reply": "fatal", "error": "Carla is not installed"})
    sys.exit(1)

sys.path.insert(0, CARLA_SHARE)
from carla_backend import (  # noqa: E402
    CarlaHostDLL,
    BINARY_NATIVE,
    ENGINE_CALLBACK_PARAMETER_VALUE_CHANGED,
    ENGINE_CALLBACK_UI_STATE_CHANGED,
    ENGINE_OPTION_OSC_ENABLED,
    ENGINE_OPTION_PATH_BINARIES,
    ENGINE_OPTION_PROCESS_MODE,
    ENGINE_PROCESS_MODE_CONTINUOUS_RACK,
    PARAMETER_INPUT,
    PARAMETER_IS_BOOLEAN,
    PARAMETER_IS_ENABLED,
    PARAMETER_IS_INTEGER,
    PLUGIN_HAS_CUSTOM_UI,
    PLUGIN_VST2,
    PLUGIN_VST3,
)

host = CarlaHostDLL(CARLA_LIB, False)
host.set_engine_option(ENGINE_OPTION_PROCESS_MODE, ENGINE_PROCESS_MODE_CONTINUOUS_RACK, "")
host.set_engine_option(ENGINE_OPTION_PATH_BINARIES, 0, os.path.dirname(CARLA_LIB))
# One engine runs per channel; without this they all race for the same
# OSC control ports and the losers fail.
host.set_engine_option(ENGINE_OPTION_OSC_ENABLED, 0, "")

# cfg_id (Crossfade's stable id) -> Carla plugin index. Indices never shift
# because the plugin set is fixed per process: structural changes respawn
# the helper.
plugin_ids = {}
cfg_by_pid = {}


def state_file(cfg_id):
    return os.path.join(STATE_DIR, f"p{cfg_id}.carxs") if STATE_DIR else None


def save_state(cfg_id):
    pid = plugin_ids.get(cfg_id)
    path = state_file(cfg_id)
    if pid is None or not path:
        return
    try:
        host.save_plugin_state(pid, path)
    except Exception:
        pass


def engine_callback(handle, action, plugin_id, value1, value2, value3, valuef, value_str):
    cfg_id = cfg_by_pid.get(plugin_id)
    if cfg_id is None:
        return
    if action == ENGINE_CALLBACK_PARAMETER_VALUE_CHANGED and value1 >= 0:
        send({"reply": "param", "cfg_id": cfg_id, "param": value1, "value": valuef})
    elif action == ENGINE_CALLBACK_UI_STATE_CHANGED and value1 == 0:
        # Editor window closed: capture whatever was edited in it.
        save_state(cfg_id)
        send({"reply": "ui", "cfg_id": cfg_id, "visible": False})


def file_callback(ptr, action, is_dir, title, filter_str):
    return ""


host.set_engine_callback(engine_callback)
host.set_file_callback(file_callback)

if not host.engine_init("JACK", CLIENT_NAME):
    send({"reply": "fatal", "error": host.get_last_error() or "engine init failed"})
    sys.exit(1)

send({"reply": "ready"})


def param_dump(pid):
    out = []
    for i in range(host.get_parameter_count(pid)):
        data = host.get_parameter_data(pid, i)
        if data["type"] != PARAMETER_INPUT or not (data["hints"] & PARAMETER_IS_ENABLED):
            continue
        info = host.get_parameter_info(pid, i)
        rng = host.get_parameter_ranges(pid, i)
        out.append({
            "index": i,
            "name": info["name"] or f"Parameter {i}",
            "min": rng["min"],
            "max": rng["max"],
            "def": rng["def"],
            "value": host.get_current_parameter_value(pid, i),
            "toggled": bool(data["hints"] & PARAMETER_IS_BOOLEAN),
            "integer": bool(data["hints"] & PARAMETER_IS_INTEGER),
        })
    return out


def cmd_load(msg):
    results = []
    for p in msg.get("plugins", []):
        cfg_id = p["cfg_id"]
        ptype = PLUGIN_VST3 if p.get("format") == "vst3" else PLUGIN_VST2
        # Carla selects a sub-plugin of a multi-plugin binary (VST3
        # bundles) by the *name* argument; label/uniqueId alone don't.
        name = p.get("name") or None
        label = p.get("label") or None
        unique_id = int(p.get("unique_id") or 0)
        try:
            ok = host.add_plugin(
                BINARY_NATIVE, ptype, p["path"], name, label, unique_id, None, 0
            )
        except Exception:  # defensive: a bad binary must not kill us
            ok = False
        if not ok:
            results.append({
                "cfg_id": cfg_id,
                "ok": False,
                "error": host.get_last_error() or "could not load plugin",
            })
            continue
        pid = host.get_current_plugin_count() - 1
        plugin_ids[cfg_id] = pid
        cfg_by_pid[pid] = cfg_id
        # Full state (including non-parameter data edited in the plugin's
        # own window) first, then explicit parameter values on top.
        path = state_file(cfg_id)
        if path and os.path.exists(path):
            try:
                host.load_plugin_state(pid, path)
            except Exception:
                pass
        for key, value in (p.get("params") or {}).items():
            try:
                host.set_parameter_value(pid, int(key), float(value))
            except Exception:
                pass
        # Backend-added plugins do not start processing on their own.
        host.set_active(pid, bool(p.get("active", True)))
        info = host.get_plugin_info(pid)
        results.append({
            "cfg_id": cfg_id,
            "ok": True,
            "name": info["name"] or os.path.basename(p["path"]),
            "has_ui": bool(info["hints"] & PLUGIN_HAS_CUSTOM_UI),
            "params": param_dump(pid),
        })
    send({"reply": "loaded", "plugins": results})


def handle(msg):
    cmd = msg.get("cmd")
    if cmd == "load":
        cmd_load(msg)
    elif cmd == "set":
        pid = plugin_ids.get(msg.get("cfg_id"))
        if pid is not None:
            host.set_parameter_value(pid, int(msg["param"]), float(msg["value"]))
    elif cmd == "active":
        pid = plugin_ids.get(msg.get("cfg_id"))
        if pid is not None:
            host.set_active(pid, bool(msg["on"]))
    elif cmd == "show_ui":
        pid = plugin_ids.get(msg.get("cfg_id"))
        if pid is not None:
            host.show_custom_ui(pid, bool(msg.get("on", True)))
    elif cmd == "quit":
        raise SystemExit(0)


buf = b""
try:
    while True:
        host.engine_idle()
        ready, _, _ = select.select([0], [], [], 0.03)
        if not ready:
            continue
        chunk = os.read(0, 65536)
        if not chunk:
            break  # Crossfade went away
        buf += chunk
        while b"\n" in buf:
            line, buf = buf.split(b"\n", 1)
            if not line.strip():
                continue
            try:
                handle(json.loads(line))
            except SystemExit:
                raise
            except Exception as e:
                send({"reply": "error", "error": str(e)})
finally:
    for cfg_id in list(plugin_ids):
        save_state(cfg_id)
    try:
        host.engine_close()
    except Exception:
        pass
