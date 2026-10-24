//! WinMM backend (Windows).
//!
//! Every MIDI input device is opened and started; its callback (a WinMM
//! system thread) forwards short messages through a channel that the GTK
//! main loop drains, so handlers run on the main thread like on Linux.
//! Output devices with the same name are opened for LED feedback.
//!
//! WinMM has no hotplug notification, so the device list is polled every
//! couple of seconds and everything is reopened when it changes. Note that
//! classic WinMM devices are single-client: a controller Crossfade has open
//! is unavailable to other applications.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use gtk::glib;
use windows::Win32::Media::Audio::{
    CALLBACK_FUNCTION, HMIDIIN, HMIDIOUT, MIDIINCAPSW, MIDIOUTCAPSW, midiInClose, midiInGetDevCapsW,
    midiInGetNumDevs, midiInOpen, midiInReset, midiInStart, midiInStop, midiOutClose,
    midiOutGetDevCapsW, midiOutGetNumDevs, midiOutOpen, midiOutShortMsg,
};

use super::{MidiEvent, led_channel};

const MIM_DATA: u32 = 0x3C3;
const MMSYSERR_NOERROR: u32 = 0;
const DRAIN_INTERVAL: Duration = Duration::from_millis(10);
const HOTPLUG_INTERVAL: Duration = Duration::from_secs(2);

/// Callback context handed to WinMM; owned by the open input and freed
/// after the device is closed (no callbacks can follow `midiInClose`).
struct Ctx {
    tx: Sender<(String, u32)>,
    name: String,
}

struct Input {
    handle: HMIDIIN,
    ctx: *mut Ctx,
}

impl Drop for Input {
    fn drop(&mut self) {
        // SAFETY: stop/reset/close a handle we opened; after close WinMM no
        // longer calls back, so the context can be freed.
        unsafe {
            let _ = midiInStop(self.handle);
            let _ = midiInReset(self.handle);
            let _ = midiInClose(self.handle);
            drop(Box::from_raw(self.ctx));
        }
    }
}

struct Output {
    name: String,
    handle: HMIDIOUT,
}

impl Drop for Output {
    fn drop(&mut self) {
        // SAFETY: closing a handle we opened.
        unsafe {
            let _ = midiOutClose(self.handle);
        }
    }
}

struct MidiInner {
    started: bool,
    inputs: Vec<(String, Input)>,
    outputs: Vec<Output>,
    tx: Sender<(String, u32)>,
    rx: Receiver<(String, u32)>,
    /// Device lists as of the last (re)open, to detect hotplug.
    signature: (Vec<String>, Vec<String>),
    handler: Option<Rc<dyn Fn(MidiEvent)>>,
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

unsafe extern "system" fn on_input(
    _handle: HMIDIIN,
    msg: u32,
    instance: usize,
    param1: usize,
    _param2: usize,
) {
    if msg != MIM_DATA || instance == 0 {
        return;
    }
    // SAFETY: `instance` is the live `Ctx` registered with this handle.
    let ctx = unsafe { &*(instance as *const Ctx) };
    let _ = ctx.tx.send((ctx.name.clone(), param1 as u32));
}

fn wide_name(raw: &[u16]) -> String {
    let len = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
    String::from_utf16_lossy(&raw[..len])
}

fn input_names() -> Vec<String> {
    // SAFETY: capability queries with correctly sized structs.
    unsafe {
        (0..midiInGetNumDevs())
            .map(|i| {
                let mut caps = MIDIINCAPSW::default();
                midiInGetDevCapsW(i as usize, &mut caps, std::mem::size_of::<MIDIINCAPSW>() as u32);
                let name = caps.szPname;
                wide_name(&name)
            })
            .collect()
    }
}

fn output_names() -> Vec<String> {
    // SAFETY: capability queries with correctly sized structs.
    unsafe {
        (0..midiOutGetNumDevs())
            .map(|i| {
                let mut caps = MIDIOUTCAPSW::default();
                midiOutGetDevCapsW(i as usize, &mut caps, std::mem::size_of::<MIDIOUTCAPSW>() as u32);
                let name = caps.szPname;
                wide_name(&name)
            })
            .collect()
    }
}

impl MidiManager {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            inner: Rc::new(RefCell::new(MidiInner {
                started: false,
                inputs: Vec::new(),
                outputs: Vec::new(),
                tx,
                rx,
                signature: (Vec::new(), Vec::new()),
                handler: None,
            })),
        }
    }

    pub fn set_event_handler(&self, f: impl Fn(MidiEvent) + 'static) {
        self.inner.borrow_mut().handler = Some(Rc::new(f));
    }

    /// WinMM is always present; MIDI is "available" once started.
    pub fn available(&self) -> bool {
        self.inner.borrow().started
    }

    /// Names of the currently connected controllers.
    pub fn devices(&self) -> Vec<String> {
        let inner = self.inner.borrow();
        let mut names: Vec<String> = inner
            .inputs
            .iter()
            .map(|(n, _)| n.clone())
            .chain(inner.outputs.iter().map(|o| o.name.clone()))
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// Open every controller and start draining input on the main loop.
    pub fn start(&self) {
        if self.inner.borrow().started {
            return;
        }
        self.inner.borrow_mut().started = true;
        self.reopen();
        {
            let rc = self.inner.clone();
            glib::timeout_add_local(DRAIN_INTERVAL, move || {
                Self::drain(&rc);
                glib::ControlFlow::Continue
            });
        }
        {
            let this = self.clone();
            glib::timeout_add_local(HOTPLUG_INTERVAL, move || {
                let current = (input_names(), output_names());
                if current != this.inner.borrow().signature {
                    this.reopen();
                    let handler = this.inner.borrow().handler.clone();
                    if let Some(h) = handler {
                        h(MidiEvent::DevicesChanged);
                    }
                }
                glib::ControlFlow::Continue
            });
        }
    }

    fn reopen(&self) {
        let mut inner = self.inner.borrow_mut();
        inner.inputs.clear();
        inner.outputs.clear();
        let ins = input_names();
        let outs = output_names();
        for (i, name) in ins.iter().enumerate() {
            let ctx = Box::into_raw(Box::new(Ctx {
                tx: inner.tx.clone(),
                name: name.clone(),
            }));
            let mut handle = HMIDIIN::default();
            // SAFETY: the callback and its context stay valid until the
            // `Input` guard closes the handle.
            let opened = unsafe {
                midiInOpen(
                    &mut handle,
                    i as u32,
                    Some(on_input as *const () as usize),
                    Some(ctx as usize),
                    CALLBACK_FUNCTION,
                ) == MMSYSERR_NOERROR
            };
            if !opened {
                // Typically held by another application.
                // SAFETY: never handed to WinMM successfully.
                drop(unsafe { Box::from_raw(ctx) });
                eprintln!("gleem-crossfade: could not open MIDI input \"{name}\" (in use?)");
                continue;
            }
            let input = Input { handle, ctx };
            // SAFETY: start a handle we just opened.
            unsafe {
                let _ = midiInStart(handle);
            }
            inner.inputs.push((name.clone(), input));
        }
        for (i, name) in outs.iter().enumerate() {
            let mut handle = HMIDIOUT::default();
            // SAFETY: plain open without callback.
            let opened = unsafe {
                midiOutOpen(&mut handle, i as u32, None, None, Default::default())
                    == MMSYSERR_NOERROR
            };
            if opened {
                inner.outputs.push(Output {
                    name: name.clone(),
                    handle,
                });
            }
        }
        inner.signature = (ins, outs);
    }

    fn drain(rc: &Rc<RefCell<MidiInner>>) {
        let raw: Vec<(String, u32)> = rc.borrow().rx.try_iter().collect();
        if raw.is_empty() {
            return;
        }
        let Some(handler) = rc.borrow().handler.clone() else {
            return;
        };
        for (device, msg) in raw {
            let status = (msg & 0xFF) as u8;
            let data1 = ((msg >> 8) & 0x7F) as u8;
            let data2 = ((msg >> 16) & 0x7F) as u8;
            let channel = status & 0x0F;
            let ev = match status & 0xF0 {
                0xB0 => MidiEvent::Control {
                    device: device.clone(),
                    channel,
                    number: data1,
                    value: data2,
                },
                0x90 if data2 > 0 => MidiEvent::NoteOn {
                    device: device.clone(),
                    channel,
                    number: data1,
                },
                _ => continue,
            };
            handler(ev);
        }
    }

    /// Send a note-on to the named controller's output(s) — how pad LEDs are
    /// set on class-compliant surfaces.
    pub fn send_note(&self, device: &str, channel: u8, note: u8, velocity: u8) {
        let channel = led_channel(device, channel) & 0x0F;
        let msg = 0x90 | u32::from(channel) | (u32::from(note & 0x7F) << 8)
            | (u32::from(velocity & 0x7F) << 16);
        let inner = self.inner.borrow();
        for out in inner.outputs.iter().filter(|o| o.name == device) {
            // SAFETY: short message to an open output.
            unsafe {
                let _ = midiOutShortMsg(out.handle, msg);
            }
        }
    }
}
