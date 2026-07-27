//! MIDI controller input/output through the ALSA sequencer (which PipeWire
//! bridges, so hardware controllers show up regardless of the audio stack).
//!
//! A single duplex sequencer client "OpenWave" auto-connects every hardware
//! controller: readable ports are subscribed into "Control In", writable
//! ports are remembered for LED feedback via "Feedback Out". Hotplug is
//! handled by subscribing to the kernel's announce port; new clients are
//! wired as they appear. The sequencer's poll fd is watched on the GTK main
//! loop, so — like the PulseAudio side — everything stays single-threaded.
//!
//! Controllers are identified by their client *name* (the USB product
//! string), which is stable across replugs; client ids are not and are only
//! used as runtime map keys.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_int;
use std::rc::Rc;

use alsa::poll::Descriptors;
use alsa::seq::{
    Addr, ClientIter, EvCtrl, EvNote, Event, EventType, PortCap, PortIter, PortSubscribe,
    PortType, Seq,
};
use gtk::glib;

#[derive(Clone, Debug)]
pub enum MidiEvent {
    /// A control-change message (CC) from a connected controller.
    Control {
        device: String,
        channel: u8,
        number: u8,
        value: u8,
    },
    /// A note-on with velocity > 0 (pad/button press). Note-offs and
    /// zero-velocity note-ons are filtered out at this layer.
    NoteOn {
        device: String,
        channel: u8,
        number: u8,
    },
    /// A controller appeared or disappeared.
    DevicesChanged,
}

struct DeviceInfo {
    name: String,
    /// Writable ports of this client, for LED feedback.
    feedback: Vec<Addr>,
}

struct MidiInner {
    seq: Option<Rc<Seq>>,
    client_id: i32,
    in_port: i32,
    out_port: i32,
    /// Connected controllers by sequencer client id.
    clients: HashMap<i32, DeviceInfo>,
    handler: Option<Rc<dyn Fn(MidiEvent)>>,
}

/// Owned copy of an incoming event, extracted before the input buffer is
/// reused and before any `RefCell` borrow is taken.
enum RawEvent {
    Control { client: i32, channel: u8, number: u8, value: u8 },
    NoteOn { client: i32, channel: u8, number: u8 },
}

#[derive(Clone)]
pub struct MidiManager {
    inner: Rc<RefCell<MidiInner>>,
}

impl Default for MidiManager {
    fn default() -> Self {
        Self::new()
    }
}

impl MidiManager {
    pub fn new() -> Self {
        Self {
            inner: Rc::new(RefCell::new(MidiInner {
                seq: None,
                client_id: -1,
                in_port: -1,
                out_port: -1,
                clients: HashMap::new(),
                handler: None,
            })),
        }
    }

    pub fn set_event_handler(&self, f: impl Fn(MidiEvent) + 'static) {
        self.inner.borrow_mut().handler = Some(Rc::new(f));
    }

    /// Whether the ALSA sequencer could be opened. When false (no
    /// /dev/snd/seq, container, …) MIDI support is simply absent.
    pub fn available(&self) -> bool {
        self.inner.borrow().seq.is_some()
    }

    /// Names of the currently connected controllers.
    pub fn devices(&self) -> Vec<String> {
        let inner = self.inner.borrow();
        let mut names: Vec<String> = inner.clients.values().map(|d| d.name.clone()).collect();
        names.sort();
        names.dedup();
        names
    }

    /// Open the sequencer, connect existing controllers and start watching
    /// for input. Safe to call once at startup; failures only disable MIDI.
    pub fn start(&self) {
        if self.inner.borrow().seq.is_some() {
            return;
        }
        let (seq, client_id, in_port, out_port) = match Self::open_seq() {
            Ok(v) => v,
            Err(e) => {
                eprintln!("openwave: MIDI disabled (ALSA sequencer unavailable): {e}");
                return;
            }
        };
        let fds = match (&*seq, Some(alsa::Direction::Capture)).get() {
            Ok(fds) => fds,
            Err(e) => {
                eprintln!("openwave: MIDI disabled (no sequencer poll fd): {e}");
                return;
            }
        };
        {
            let mut inner = self.inner.borrow_mut();
            inner.client_id = client_id;
            inner.in_port = in_port;
            inner.out_port = out_port;
            Self::scan(&seq, &mut inner);
            inner.seq = Some(seq);
        }
        for fd in fds {
            let rc = self.inner.clone();
            watch_fd(fd.fd, move || Self::drain(&rc));
        }
    }

    fn open_seq() -> alsa::Result<(Rc<Seq>, i32, i32, i32)> {
        let seq = Seq::open(None, None, true)?;
        seq.set_client_name(c"OpenWave")?;
        let client_id = seq.client_id()?;
        let caps_in = PortCap::WRITE | PortCap::SUBS_WRITE;
        let caps_out = PortCap::READ | PortCap::SUBS_READ;
        let kind = PortType::MIDI_GENERIC | PortType::APPLICATION;
        let in_port = seq.create_simple_port(c"Control In", caps_in, kind)?;
        let out_port = seq.create_simple_port(c"Feedback Out", caps_out, kind)?;
        // Hotplug: the kernel announces client/port changes on System:Announce.
        let sub = PortSubscribe::empty()?;
        sub.set_sender(Addr::system_announce());
        sub.set_dest(Addr {
            client: client_id,
            port: in_port,
        });
        seq.subscribe_port(&sub)?;
        Ok((Rc::new(seq), client_id, in_port, out_port))
    }

    /// (Re)connect every controller and rebuild the client map. Idempotent:
    /// re-subscribing an existing connection just fails with EBUSY.
    fn scan(seq: &Seq, inner: &mut MidiInner) {
        let mut clients: HashMap<i32, DeviceInfo> = HashMap::new();
        for client in ClientIter::new(seq) {
            let id = client.get_client();
            if id == 0 || id == inner.client_id {
                continue;
            }
            let name = client.get_name().unwrap_or_default().to_string();
            // Not controllers: the kernel's loopback client and the
            // sequencer bridges PipeWire creates for its own graph.
            if name == "Midi Through" || name.starts_with("PipeWire-") {
                continue;
            }
            let mut has_input = false;
            let mut feedback = Vec::new();
            for port in PortIter::new(seq, id) {
                let caps = port.get_capability();
                let addr = port.addr();
                if caps.contains(PortCap::READ | PortCap::SUBS_READ) {
                    has_input = true;
                    if let Ok(sub) = PortSubscribe::empty() {
                        sub.set_sender(addr);
                        sub.set_dest(Addr {
                            client: inner.client_id,
                            port: inner.in_port,
                        });
                        let _ = seq.subscribe_port(&sub);
                    }
                }
                if caps.contains(PortCap::WRITE | PortCap::SUBS_WRITE) {
                    feedback.push(addr);
                    if let Ok(sub) = PortSubscribe::empty() {
                        sub.set_sender(Addr {
                            client: inner.client_id,
                            port: inner.out_port,
                        });
                        sub.set_dest(addr);
                        let _ = seq.subscribe_port(&sub);
                    }
                }
            }
            if has_input || !feedback.is_empty() {
                clients.insert(id, DeviceInfo { name, feedback });
            }
        }
        inner.clients = clients;
    }

    /// Pull everything queued on the sequencer fd and dispatch it. Events
    /// are copied out first so no `RefCell` borrow is held while the
    /// handler runs (it typically calls back into this manager).
    fn drain(rc: &Rc<RefCell<MidiInner>>) {
        let Some(seq) = rc.borrow().seq.clone() else {
            return;
        };
        let mut raw: Vec<RawEvent> = Vec::new();
        let mut announce = false;
        {
            let mut input = seq.input();
            loop {
                match input.event_input_pending(true) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let Ok(ev) = input.event_input() else {
                    break;
                };
                match ev.get_type() {
                    EventType::Controller => {
                        if let Some(d) = ev.get_data::<EvCtrl>() {
                            raw.push(RawEvent::Control {
                                client: ev.get_source().client,
                                channel: d.channel,
                                number: d.param.min(127) as u8,
                                value: d.value.clamp(0, 127) as u8,
                            });
                        }
                    }
                    EventType::Noteon => {
                        if let Some(d) = ev.get_data::<EvNote>()
                            && d.velocity > 0
                        {
                            raw.push(RawEvent::NoteOn {
                                client: ev.get_source().client,
                                channel: d.channel,
                                number: d.note,
                            });
                        }
                    }
                    EventType::ClientStart
                    | EventType::ClientExit
                    | EventType::ClientChange
                    | EventType::PortStart
                    | EventType::PortExit
                    | EventType::PortChange => announce = true,
                    _ => {}
                }
            }
        }
        let mut out: Vec<MidiEvent> = Vec::new();
        {
            let mut inner = rc.borrow_mut();
            if announce {
                Self::scan(&seq, &mut inner);
                out.push(MidiEvent::DevicesChanged);
            }
            for r in raw {
                // Resolve the sender to a controller name; clients that
                // direct-address our port without any subscribable port of
                // their own (scripting tools, `aplaymidi`, …) are not in
                // the map and are looked up on the fly.
                let device = |client: i32, inner: &MidiInner| {
                    inner
                        .clients
                        .get(&client)
                        .map(|d| d.name.clone())
                        .or_else(|| {
                            seq.get_any_client_info(client)
                                .ok()
                                .and_then(|ci| ci.get_name().ok().map(str::to_string))
                        })
                };
                match r {
                    RawEvent::Control {
                        client,
                        channel,
                        number,
                        value,
                    } => {
                        if let Some(device) = device(client, &inner) {
                            out.push(MidiEvent::Control {
                                device,
                                channel,
                                number,
                                value,
                            });
                        }
                    }
                    RawEvent::NoteOn {
                        client,
                        channel,
                        number,
                    } => {
                        if let Some(device) = device(client, &inner) {
                            out.push(MidiEvent::NoteOn {
                                device,
                                channel,
                                number,
                            });
                        }
                    }
                }
            }
        }
        let handler = rc.borrow().handler.clone();
        if let Some(h) = handler {
            for ev in out {
                h(ev);
            }
        }
    }

    /// Send a note-on to every writable port of the named controller —
    /// how pad LEDs are set on class-compliant surfaces (the velocity
    /// selects the color on e.g. an APC mini). Addressed directly, so a
    /// second connected controller never receives another device's state.
    pub fn send_note(&self, device: &str, channel: u8, note: u8, velocity: u8) {
        let channel = led_channel(device, channel);
        let inner = self.inner.borrow();
        let Some(seq) = inner.seq.as_ref() else {
            return;
        };
        let mut sent = false;
        for info in inner.clients.values().filter(|d| d.name == device) {
            for addr in &info.feedback {
                let mut ev = Event::new(
                    EventType::Noteon,
                    &EvNote {
                        channel,
                        note,
                        velocity,
                        off_velocity: 0,
                        duration: 0,
                    },
                );
                ev.set_source(inner.out_port);
                ev.set_dest(*addr);
                ev.set_direct();
                if seq.event_output(&mut ev).is_ok() {
                    sent = true;
                }
            }
        }
        if sent {
            let _ = seq.drain_output();
        }
    }
}

/// Velocity → 0xRRGGBB palette shared by the common RGB pad surfaces
/// (Novation Launchpads; Akai adopted the same table for the APC mini mk2).
/// Mono-LED controllers simply light up on any non-zero velocity, so using
/// this table universally degrades gracefully.
const PAD_PALETTE: [u32; 128] = [
    0x000000, 0x1E1E1E, 0x7F7F7F, 0xFFFFFF, 0xFF4C4C, 0xFF0000, 0x590000, 0x190000,
    0xFFBD6C, 0xFF5400, 0x591D00, 0x271B00, 0xFFFF4C, 0xFFFF00, 0x595900, 0x191900,
    0x88FF4C, 0x54FF00, 0x1D5900, 0x142B00, 0x4CFF4C, 0x00FF00, 0x005900, 0x001900,
    0x4CFF5E, 0x00FF19, 0x00590D, 0x001902, 0x4CFF88, 0x00FF55, 0x00591D, 0x001F12,
    0x4CFFB7, 0x00FF99, 0x005935, 0x001912, 0x4CC3FF, 0x00A9FF, 0x004152, 0x001019,
    0x4C88FF, 0x0055FF, 0x001D59, 0x000819, 0x4C4CFF, 0x0000FF, 0x000059, 0x000019,
    0x874CFF, 0x5400FF, 0x190064, 0x0F0030, 0xFF4CFF, 0xFF00FF, 0x590059, 0x190019,
    0xFF4C87, 0xFF0054, 0x59001D, 0x220013, 0xFF1500, 0x993500, 0x795100, 0x436400,
    0x033900, 0x005735, 0x00547F, 0x0000FF, 0x00454F, 0x2500CC, 0x7F7F7F, 0x202020,
    0xFF0000, 0xBDFF2D, 0xAFED06, 0x64FF09, 0x108B00, 0x00FF87, 0x00A9FF, 0x002AFF,
    0x3F00FF, 0x7A00FF, 0xB21A7D, 0x402100, 0xFF4A00, 0x88E106, 0x72FF15, 0x00FF00,
    0x3BFF26, 0x59FF71, 0x38FFCC, 0x5B8AFF, 0x3151C6, 0x877FE9, 0xD31DFF, 0xFF005D,
    0xFF7F00, 0xB9B000, 0x90FF00, 0x835D07, 0x392B00, 0x144C10, 0x0D5038, 0x15152A,
    0x16205A, 0x693C1C, 0xA8000A, 0xDE513D, 0xD86A1C, 0xFFE126, 0x9EE12F, 0x67B50F,
    0x1E1E30, 0xDCFF6B, 0x80FFBD, 0x9A99FF, 0x8E66FF, 0x404040, 0x757575, 0xE0FFFF,
    0xA00000, 0x350000, 0x1AD000, 0x074200, 0xB9B000, 0x3F3100, 0xB35F00, 0x4B1502,
];

/// Velocity whose palette entry is nearest (RGB distance) to the requested
/// color — how a channel color is shown on a pad bound as its color pad.
pub fn color_velocity(r: u8, g: u8, b: u8) -> u8 {
    let mut best = 0u8;
    let mut best_d = i32::MAX;
    for (i, &c) in PAD_PALETTE.iter().enumerate() {
        let dr = ((c >> 16) & 0xFF) as i32 - r as i32;
        let dg = ((c >> 8) & 0xFF) as i32 - g as i32;
        let db = (c & 0xFF) as i32 - b as i32;
        let d = dr * dr + dg * dg + db * db;
        if d < best_d {
            best_d = d;
            best = i as u8;
        }
    }
    best
}

/// Per-device LED quirks. Pads are normally lit by echoing the note on the
/// channel the binding listens on, but on the APC mini mk2 the note-on
/// channel selects pad *brightness* (0 = 10% … 6 = 100%, 7+ = pulse/blink),
/// so echoing on the learned channel 0 comes out barely visible — use full
/// brightness instead.
fn led_channel(device: &str, bound: u8) -> u8 {
    if device.eq_ignore_ascii_case("APC mini mk2") {
        6
    } else {
        bound
    }
}

/// Watch a file descriptor for readability on the GLib main loop. glib 0.22
/// dropped the `unix_fd_add` bindings, so this goes through the still-bound
/// GIOChannel watch FFI; the watch holds its own channel reference and stays
/// installed for the life of the process (the sequencer fd never changes).
fn watch_fd(fd: c_int, callback: impl Fn() + 'static) {
    unsafe extern "C" fn trampoline(
        _chan: *mut glib::ffi::GIOChannel,
        _cond: glib::ffi::GIOCondition,
        data: glib::ffi::gpointer,
    ) -> glib::ffi::gboolean {
        let f = unsafe { &*(data as *const Box<dyn Fn()>) };
        f();
        glib::ffi::GTRUE
    }
    unsafe extern "C" fn destroy(data: glib::ffi::gpointer) {
        drop(unsafe { Box::from_raw(data as *mut Box<dyn Fn()>) });
    }
    let data: *mut Box<dyn Fn()> = Box::into_raw(Box::new(Box::new(callback)));
    unsafe {
        let chan = glib::ffi::g_io_channel_unix_new(fd);
        glib::ffi::g_io_add_watch_full(
            chan,
            glib::ffi::G_PRIORITY_DEFAULT,
            glib::ffi::G_IO_IN,
            Some(trampoline),
            data as glib::ffi::gpointer,
            Some(destroy),
        );
        glib::ffi::g_io_channel_unref(chan);
    }
}
