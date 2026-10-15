//! The real-time mixing engine: one thread that pulls every channel's input
//! from WASAPI, mixes the Monitor/Stream/VOD buses and delivers them — the
//! monitor bus to a hardware output, the stream and VOD buses to the
//! Crossfade driver's virtual microphones.
//!
//! The mix clock is the performance counter, which is also what the virtual
//! driver's endpoints run on, so the microphones never drift against the
//! mix. Hardware devices run on their own crystals; each one is decoupled by
//! an elastic FIFO that drops or repeats a single frame now and then to
//! track the drift (~1 frame per second at typical crystal tolerances —
//! inaudible, unlike periodic buffer resets).
//!
//! The main thread talks to the engine through `Command`s and receives
//! `EngineEvent`s (meter levels, warnings) through a channel it polls.

use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, IAudioCaptureClient,
    IAudioClient, IAudioRenderClient, IMMDeviceEnumerator, MMDeviceEnumerator, WAVEFORMATEX,
    WAVEFORMATEXTENSIBLE, WAVEFORMATEXTENSIBLE_0,
};
use windows::Win32::Media::KernelStreaming::WAVE_FORMAT_EXTENSIBLE;
use windows::Win32::Media::Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CoUninitialize,
};
use windows::Win32::System::Threading::AvSetMmThreadCharacteristicsW;
use windows::core::{HSTRING, PCWSTR, w};

use super::driver::{DriverRole, MicFeed};
use crate::audio::LevelTarget;

pub const RATE: u32 = 48_000;
const TICK: Duration = Duration::from_millis(5);
/// WASAPI buffer requested for every stream (100 ns units: 100 ms).
const BUFFER_HNS: i64 = 1_000_000;
/// Capture FIFO slack aimed for, in frames (5 ms): headroom for scheduling
/// jitter beyond the 10 ms packet cadence of shared-mode capture.
const INPUT_TARGET: usize = 240;
/// Frames kept queued in the monitor device's own buffer (20 ms).
const OUTPUT_PADDING: u32 = 960;
/// Monitor FIFO slack aimed for (5 ms).
const OUTPUT_TARGET: usize = 240;
/// Never produce more than this per tick, even after a stall (100 ms).
const MAX_BLOCK: usize = 4800;
const LEVEL_INTERVAL: Duration = Duration::from_millis(50);
const RETRY_INTERVAL: Duration = Duration::from_secs(2);

pub type Frame = [f32; 2];

/// Where a channel's audio comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InputSpec {
    /// A capture endpoint (microphone, line in).
    Capture(String),
    /// Loopback recording of a render endpoint — a hardware output's
    /// "monitor", or one of the driver's Crossfade Channel endpoints.
    Loopback(String),
}

pub enum Command {
    SetInput { id: u64, input: Option<InputSpec> },
    /// Linear send gains into [monitor, stream, vod].
    SetGains { id: u64, gains: [f32; 3] },
    Remove(u64),
    /// Render endpoint for the monitor mix (`None` = monitor off).
    SetMonitor(Option<String>),
    SetMonitorGain(f32),
    SetVod(bool),
    Shutdown,
}

pub enum EngineEvent {
    Levels(Vec<(LevelTarget, Vec<f64>)>),
    Warning(String),
}

pub struct Engine {
    tx: Sender<Command>,
    rx: Receiver<EngineEvent>,
    thread: Option<JoinHandle<()>>,
}

impl Engine {
    pub fn start() -> Self {
        let (tx, cmd_rx) = mpsc::channel();
        let (ev_tx, rx) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("crossfade-engine".into())
            .spawn(move || run(cmd_rx, ev_tx))
            .expect("spawn audio engine thread");
        Self {
            tx,
            rx,
            thread: Some(thread),
        }
    }

    pub fn send(&self, cmd: Command) {
        let _ = self.tx.send(cmd);
    }

    pub fn events(&self) -> Vec<EngineEvent> {
        self.rx.try_iter().collect()
    }

    /// Stop the engine and wait for its streams to be released.
    pub fn stop(&mut self) {
        let _ = self.tx.send(Command::Shutdown);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.stop();
    }
}

// ---- Elastic FIFO ---------------------------------------------------------------

/// A FIFO between two clocks. The reader asks for exactly what it needs.
///
/// WASAPI moves audio in bursts (one device period, usually 10 ms), so the
/// fill level saw-tooths and its average says little. What matters is the
/// *slack*: the lowest fill seen after a read over a window. Slack that
/// keeps growing means the writer is faster (drop a frame), slack that
/// shrinks towards zero means the reader is faster (repeat a frame). One
/// correction per window tracks crystal drift (tens of ppm, ~1 frame/s).
struct Elastic {
    buf: VecDeque<Frame>,
    /// Slack aimed for, in frames.
    target: usize,
    /// Waiting for `target` frames before the first (or post-underrun) read.
    primed: bool,
    window_min: usize,
    window_pulls: u32,
}

/// Reads per drift decision (~0.5 s at the engine tick).
const WINDOW_PULLS: u32 = 100;

impl Elastic {
    fn new(target: usize) -> Self {
        Self {
            buf: VecDeque::with_capacity(target * 8),
            target,
            primed: false,
            window_min: usize::MAX,
            window_pulls: 0,
        }
    }

    fn clear(&mut self) {
        self.buf.clear();
        self.primed = false;
        self.window_min = usize::MAX;
        self.window_pulls = 0;
    }

    fn push(&mut self, frames: impl IntoIterator<Item = Frame>) {
        self.buf.extend(frames);
        // A reader that stopped (device gone, consumer stalled) must not
        // let the queue grow without bound.
        if self.buf.len() > MAX_BLOCK * 2 {
            let excess = self.buf.len() - self.target;
            self.buf.drain(..excess);
        }
    }

    /// Fill `out` with exactly `n` frames.
    fn pull(&mut self, n: usize, out: &mut Vec<Frame>) {
        out.clear();
        if !self.primed {
            if self.buf.len() >= self.target + n {
                self.primed = true;
                self.window_min = usize::MAX;
                self.window_pulls = 0;
            } else {
                out.resize(n, [0.0; 2]);
                return;
            }
        }
        let avail = n.min(self.buf.len());
        out.extend(self.buf.drain(..avail));
        if out.len() < n {
            // Underrun: pad with silence and re-prime.
            out.resize(n, [0.0; 2]);
            self.primed = false;
            return;
        }
        self.window_min = self.window_min.min(self.buf.len());
        self.window_pulls += 1;
        if self.window_pulls < WINDOW_PULLS {
            return;
        }
        let slack = self.window_min;
        self.window_min = usize::MAX;
        self.window_pulls = 0;
        if slack > self.target * 6 {
            // Far off (startup burst, a stall that queued up audio): cut
            // back in one go rather than over minutes.
            self.buf.drain(..slack - self.target);
        } else if slack > self.target * 2 {
            // Writer is fast: skip one frame.
            self.buf.pop_front();
        } else if slack < self.target / 2
            && let Some(&last) = out.last()
        {
            // Reader is fast: repeat one frame.
            self.buf.push_front(last);
        }
    }
}

// ---- WASAPI streams ---------------------------------------------------------------

fn float_format(channels: u16, mask: u32) -> WAVEFORMATEXTENSIBLE {
    let block = channels * 4;
    WAVEFORMATEXTENSIBLE {
        Format: WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_EXTENSIBLE as u16,
            nChannels: channels,
            nSamplesPerSec: RATE,
            nAvgBytesPerSec: RATE * u32::from(block),
            nBlockAlign: block,
            wBitsPerSample: 32,
            cbSize: (std::mem::size_of::<WAVEFORMATEXTENSIBLE>()
                - std::mem::size_of::<WAVEFORMATEX>()) as u16,
        },
        Samples: WAVEFORMATEXTENSIBLE_0 {
            wValidBitsPerSample: 32,
        },
        dwChannelMask: mask,
        SubFormat: KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
    }
}

/// Activate and initialize a shared-mode client on an endpoint in our
/// float/48 kHz format, keeping the device's channel layout (the engine
/// up/down-mixes to stereo itself, so mono mics stay centered).
unsafe fn open_client(
    enumerator: &IMMDeviceEnumerator,
    id: &str,
    loopback: bool,
) -> windows::core::Result<(IAudioClient, usize)> {
    // SAFETY: the caller's thread is COM-initialized; the mix format
    // pointer is freed after its fields are copied.
    unsafe {
        let wide = HSTRING::from(id);
        let device = enumerator.GetDevice(PCWSTR(wide.as_ptr()))?;
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
        let mix = client.GetMixFormat()?;
        let channels = (*mix).nChannels.max(1);
        let mask = if (*mix).wFormatTag == WAVE_FORMAT_EXTENSIBLE as u16 {
            (*(mix as *const WAVEFORMATEXTENSIBLE)).dwChannelMask
        } else {
            0
        };
        CoTaskMemFree(Some(mix as *const _));
        let format = float_format(channels, mask);
        let mut flags =
            AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
        if loopback {
            flags |= AUDCLNT_STREAMFLAGS_LOOPBACK;
        }
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            flags,
            BUFFER_HNS,
            0,
            &format.Format,
            None,
        )?;
        Ok((client, channels as usize))
    }
}

struct CaptureStream {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    channels: usize,
}

impl CaptureStream {
    fn open(enumerator: &IMMDeviceEnumerator, spec: &InputSpec) -> windows::core::Result<Self> {
        let (id, loopback) = match spec {
            InputSpec::Capture(id) => (id, false),
            InputSpec::Loopback(id) => (id, true),
        };
        // SAFETY: engine thread is COM-initialized.
        unsafe {
            let (client, channels) = open_client(enumerator, id, loopback)?;
            let capture: IAudioCaptureClient = client.GetService()?;
            client.Start()?;
            Ok(Self {
                client,
                capture,
                channels,
            })
        }
    }

    /// Move every pending packet into the FIFO as stereo frames.
    fn drain(&self, fifo: &mut Elastic) -> windows::core::Result<()> {
        // SAFETY: buffers are only touched between GetBuffer/ReleaseBuffer.
        unsafe {
            loop {
                if self.capture.GetNextPacketSize()? == 0 {
                    return Ok(());
                }
                let mut data = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                self.capture
                    .GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
                let n = frames as usize;
                if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                    fifo.push(std::iter::repeat_n([0.0; 2], n));
                } else {
                    let samples = std::slice::from_raw_parts(data as *const f32, n * self.channels);
                    fifo.push(samples.chunks_exact(self.channels).map(to_stereo));
                }
                self.capture.ReleaseBuffer(frames)?;
            }
        }
    }
}

impl Drop for CaptureStream {
    fn drop(&mut self) {
        // SAFETY: stopping a started client.
        unsafe {
            let _ = self.client.Stop();
        }
    }
}

fn to_stereo(frame: &[f32]) -> Frame {
    match frame.len() {
        1 => [frame[0], frame[0]],
        _ => [frame[0], frame[1]],
    }
}

struct RenderStream {
    client: IAudioClient,
    render: IAudioRenderClient,
    channels: usize,
    buffer_frames: u32,
}

impl RenderStream {
    fn open(enumerator: &IMMDeviceEnumerator, id: &str) -> windows::core::Result<Self> {
        // SAFETY: engine thread is COM-initialized.
        unsafe {
            let (client, channels) = open_client(enumerator, id, false)?;
            let render: IAudioRenderClient = client.GetService()?;
            let buffer_frames = client.GetBufferSize()?;
            client.Start()?;
            Ok(Self {
                client,
                render,
                channels,
                buffer_frames,
            })
        }
    }

    /// How many frames the device wants to stay `OUTPUT_PADDING` ahead.
    fn wanted(&self) -> windows::core::Result<usize> {
        // SAFETY: plain getter.
        let padding = unsafe { self.client.GetCurrentPadding()? };
        let space = self.buffer_frames.saturating_sub(padding);
        Ok(OUTPUT_PADDING.saturating_sub(padding).min(space) as usize)
    }

    fn write(&self, frames: &[Frame]) -> windows::core::Result<()> {
        if frames.is_empty() {
            return Ok(());
        }
        let n = frames.len();
        // SAFETY: we write exactly `n * channels` samples into the buffer
        // WASAPI handed out, then release it.
        unsafe {
            let data = self.render.GetBuffer(n as u32)?;
            let out = std::slice::from_raw_parts_mut(data as *mut f32, n * self.channels);
            for (dst, src) in out.chunks_exact_mut(self.channels).zip(frames) {
                match self.channels {
                    1 => dst[0] = (src[0] + src[1]) * 0.5,
                    _ => {
                        dst[0] = src[0];
                        dst[1] = src[1];
                        dst[2..].fill(0.0);
                    }
                }
            }
            self.render.ReleaseBuffer(n as u32, 0)?;
        }
        Ok(())
    }
}

impl Drop for RenderStream {
    fn drop(&mut self) {
        // SAFETY: stopping a started client.
        unsafe {
            let _ = self.client.Stop();
        }
    }
}

// ---- Engine state -------------------------------------------------------------------

struct Input {
    spec: Option<InputSpec>,
    stream: Option<CaptureStream>,
    retry_at: Instant,
    /// Last error reported for this input, so retries stay quiet.
    failed: bool,
    fifo: Elastic,
    /// Current and requested send gains; ramped per block (no zipper noise).
    gains: [f32; 3],
    target: [f32; 3],
    peak: [f32; 2],
}

impl Input {
    fn new() -> Self {
        Self {
            spec: None,
            stream: None,
            retry_at: Instant::now(),
            failed: false,
            fifo: Elastic::new(INPUT_TARGET),
            gains: [0.0; 3],
            target: [0.0; 3],
            peak: [0.0; 2],
        }
    }
}

struct Monitor {
    device: Option<String>,
    stream: Option<RenderStream>,
    retry_at: Instant,
    failed: bool,
    fifo: Elastic,
    gain: f32,
    gain_target: f32,
}

struct Mic {
    role: DriverRole,
    feed: Option<MicFeed>,
    retry_at: Instant,
}

impl Mic {
    fn new(role: DriverRole) -> Self {
        Self {
            role,
            feed: None,
            retry_at: Instant::now(),
        }
    }

    fn push(&mut self, frames: &[Frame], now: Instant) {
        if self.feed.is_none() && now >= self.retry_at {
            self.feed = MicFeed::open(self.role);
            self.retry_at = now + RETRY_INTERVAL;
        }
        if let Some(feed) = &mut self.feed
            && !feed.push(frames.as_flattened())
        {
            self.feed = None;
        }
    }
}

struct State {
    enumerator: IMMDeviceEnumerator,
    events: Sender<EngineEvent>,
    inputs: HashMap<u64, Input>,
    monitor: Monitor,
    vod: bool,
    stream_mic: Mic,
    vod_mic: Mic,
    bus_peak: [[f32; 2]; 3],
}

impl State {
    fn warn(&self, msg: String) {
        let _ = self.events.send(EngineEvent::Warning(msg));
    }

    fn apply(&mut self, cmd: Command) {
        match cmd {
            Command::SetInput { id, input } => {
                let entry = self.inputs.entry(id).or_insert_with(Input::new);
                if entry.spec != input {
                    entry.spec = input;
                    entry.stream = None;
                    entry.failed = false;
                    entry.retry_at = Instant::now();
                    entry.fifo.clear();
                }
            }
            Command::SetGains { id, gains } => {
                self.inputs.entry(id).or_insert_with(Input::new).target = gains;
            }
            Command::Remove(id) => {
                self.inputs.remove(&id);
            }
            Command::SetMonitor(device) => {
                if self.monitor.device != device {
                    self.monitor.device = device;
                    self.monitor.stream = None;
                    self.monitor.failed = false;
                    self.monitor.retry_at = Instant::now();
                    self.monitor.fifo.clear();
                }
            }
            Command::SetMonitorGain(g) => self.monitor.gain_target = g,
            Command::SetVod(on) => self.vod = on,
            Command::Shutdown => {}
        }
    }

    /// (Re)open streams that are configured but not running.
    fn reconnect(&mut self, now: Instant) {
        let mut warnings = Vec::new();
        for input in self.inputs.values_mut() {
            let Some(spec) = &input.spec else {
                continue;
            };
            if input.stream.is_some() || now < input.retry_at {
                continue;
            }
            input.retry_at = now + RETRY_INTERVAL;
            match CaptureStream::open(&self.enumerator, spec) {
                Ok(s) => {
                    input.stream = Some(s);
                    input.failed = false;
                    input.fifo.clear();
                }
                Err(e) => {
                    if !input.failed {
                        warnings.push(format!("could not open input {spec:?}: {e}"));
                    }
                    input.failed = true;
                }
            }
        }
        let m = &mut self.monitor;
        if let Some(device) = &m.device
            && m.stream.is_none()
            && now >= m.retry_at
        {
            m.retry_at = now + RETRY_INTERVAL;
            match RenderStream::open(&self.enumerator, device) {
                Ok(s) => {
                    m.stream = Some(s);
                    m.failed = false;
                    m.fifo.clear();
                }
                Err(e) => {
                    if !m.failed {
                        warnings.push(format!("could not open the monitor output: {e}"));
                    }
                    m.failed = true;
                }
            }
        }
        for w in warnings {
            self.warn(w);
        }
    }

    /// Produce `n` frames of every bus and deliver them.
    fn process(&mut self, n: usize, now: Instant, scratch: &mut Scratch) {
        for bus in &mut scratch.buses {
            bus.clear();
            bus.resize(n, [0.0; 2]);
        }
        let vod = self.vod;
        for input in self.inputs.values_mut() {
            if let Some(stream) = &input.stream
                && stream.drain(&mut input.fifo).is_err()
            {
                // Device unplugged or invalidated: reopen later.
                input.stream = None;
                input.fifo.clear();
                input.retry_at = now + RETRY_INTERVAL;
            }
            input.fifo.pull(n, &mut scratch.block);
            for f in &scratch.block {
                input.peak[0] = input.peak[0].max(f[0].abs());
                input.peak[1] = input.peak[1].max(f[1].abs());
            }
            let buses = if vod { 3 } else { 2 };
            for (b, bus) in scratch.buses.iter_mut().enumerate().take(buses) {
                let (from, to) = (input.gains[b], input.target[b]);
                if from == 0.0 && to == 0.0 {
                    continue;
                }
                let step = (to - from) / n as f32;
                for (i, (dst, src)) in bus.iter_mut().zip(&scratch.block).enumerate() {
                    let g = from + step * i as f32;
                    dst[0] += src[0] * g;
                    dst[1] += src[1] * g;
                }
            }
            input.gains = input.target;
        }

        for (b, bus) in scratch.buses.iter().enumerate() {
            for f in bus {
                self.bus_peak[b][0] = self.bus_peak[b][0].max(f[0].abs());
                self.bus_peak[b][1] = self.bus_peak[b][1].max(f[1].abs());
            }
        }

        // Monitor: master gain, then out through the device's FIFO.
        let m = &mut self.monitor;
        if m.stream.is_some() {
            let (from, to) = (m.gain, m.gain_target);
            let step = (to - from) / n as f32;
            m.fifo.push(scratch.buses[0].iter().enumerate().map(|(i, f)| {
                let g = from + step * i as f32;
                [f[0] * g, f[1] * g]
            }));
            m.gain = to;
            let result = m.stream.as_ref().map(|s| -> windows::core::Result<()> {
                let want = s.wanted()?;
                if want > 0 {
                    m.fifo.pull(want, &mut scratch.block);
                    s.write(&scratch.block)?;
                }
                Ok(())
            });
            if let Some(Err(_)) = result {
                m.stream = None;
                m.retry_at = now + RETRY_INTERVAL;
            }
        } else {
            m.gain = m.gain_target;
        }

        self.stream_mic.push(&scratch.buses[1], now);
        if vod {
            self.vod_mic.push(&scratch.buses[2], now);
        }
    }

    fn report_levels(&mut self) {
        let mut levels = Vec::with_capacity(self.inputs.len() + 3);
        for (id, input) in &mut self.inputs {
            let mono = input.stream.as_ref().is_some_and(|s| s.channels == 1);
            let v = if mono {
                vec![f64::from(input.peak[0])]
            } else {
                vec![f64::from(input.peak[0]), f64::from(input.peak[1])]
            };
            levels.push((LevelTarget::Channel(*id), v));
            input.peak = [0.0; 2];
        }
        let targets = [LevelTarget::MonitorMix, LevelTarget::StreamMix, LevelTarget::VodMix];
        for (b, target) in targets.iter().enumerate() {
            if b == 2 && !self.vod {
                continue;
            }
            let p = self.bus_peak[b];
            levels.push((*target, vec![f64::from(p[0]), f64::from(p[1])]));
        }
        self.bus_peak = [[0.0; 2]; 3];
        let _ = self.events.send(EngineEvent::Levels(levels));
    }
}

#[derive(Default)]
struct Scratch {
    buses: [Vec<Frame>; 3],
    block: Vec<Frame>,
}

fn run(commands: Receiver<Command>, events: Sender<EngineEvent>) {
    // SAFETY: per-thread COM and MMCSS registration.
    let enumerator: IMMDeviceEnumerator = unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let mut task = 0u32;
        let _ = AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task);
        match CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) {
            Ok(e) => e,
            Err(e) => {
                let _ = events.send(EngineEvent::Warning(format!(
                    "audio engine could not start: {e}"
                )));
                CoUninitialize();
                return;
            }
        }
    };
    let mut state = State {
        enumerator,
        events,
        inputs: HashMap::new(),
        monitor: Monitor {
            device: None,
            stream: None,
            retry_at: Instant::now(),
            failed: false,
            fifo: Elastic::new(OUTPUT_TARGET),
            gain: 0.0,
            gain_target: 0.0,
        },
        vod: false,
        stream_mic: Mic::new(DriverRole::StreamMic),
        vod_mic: Mic::new(DriverRole::VodMic),
        bus_peak: [[0.0; 2]; 3],
    };
    let mut scratch = Scratch::default();
    let start = Instant::now();
    let mut produced: u64 = 0;
    let mut last_levels = start;

    'outer: loop {
        loop {
            match commands.try_recv() {
                Ok(Command::Shutdown) | Err(TryRecvError::Disconnected) => break 'outer,
                Ok(cmd) => state.apply(cmd),
                Err(TryRecvError::Empty) => break,
            }
        }
        let now = Instant::now();
        state.reconnect(now);

        let due = (now - start).as_nanos() as u64 * u64::from(RATE) / 1_000_000_000;
        let mut n = due.saturating_sub(produced) as usize;
        if n > MAX_BLOCK {
            // We were descheduled for a long time; skip ahead instead of
            // bursting stale audio.
            produced = due - MAX_BLOCK as u64;
            n = MAX_BLOCK;
        }
        if n > 0 {
            state.process(n, now, &mut scratch);
            produced += n as u64;
        }

        if now - last_levels >= LEVEL_INTERVAL {
            last_levels = now;
            state.report_levels();
        }
        std::thread::sleep(TICK);
    }

    drop(state);
    // SAFETY: balanced with the CoInitializeEx above.
    unsafe { CoUninitialize() };
}
