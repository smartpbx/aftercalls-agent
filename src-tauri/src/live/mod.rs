//! Live-transcript relay — Phase 1, Tier-1.
//!
//! Streams a **lossy copy** of the recorder's mic + system audio to the
//! backend over a WebSocket, receives draft You/Them segments, and forwards
//! them to the Svelte UI as `live-segment` / `live-session` Tauri events.
//!
//! This is a *disposable draft*. The post-call batch pipeline
//! (`pipeline.rs` → `transcribe` → `summarize`) remains the source of truth;
//! nothing here feeds the AI summary. The relay must never add latency to or
//! drop samples from the WAV writer — only this live tap is allowed to lose
//! audio (bounded, drop-oldest). On any connect/stream failure it degrades
//! silently (a `session`-`error` event → soft "live unavailable" in the UI)
//! and never crashes the recording.
//!
//! Ladder note: the `LocalStt` trait is the seam for future Tier-0 on-device
//! engines. Phase 1 ships exactly one impl, [`Tier1Relay`], which forwards
//! PCM to the backend rather than transcribing locally. The fallback seam is
//! invisible to the UI — all tiers emit the same `LiveSegment` shape.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::{mpsc, watch, Notify};

use rubato::Resampler;
use tokio_tungstenite::tungstenite::Message;

/// Target rate the backend transcriber expects: 16 kHz mono.
const TARGET_RATE: usize = 16_000;
/// Channel tags on the binary wire frames. byte[0] of every audio frame.
pub const CHANNEL_MIC: u8 = 0x00;
pub const CHANNEL_SYSTEM: u8 = 0x01;
/// earshot's WebRTC VAD scores exactly-256-sample (16 ms @ 16 kHz) frames.
const VAD_FRAME: usize = 256;
/// Device audio buffered per channel before a resample+send pass (~200 ms).
const FRAME_MS: usize = 200;
/// Bounded live-tap ring depth. Drop-OLDEST past this. A few hundred ms of
/// device callbacks; live loss is acceptable, WAV loss is not.
const RING_CAP: usize = 64;

/// Which capture lane a chunk came from. mic → "You", system → "Them".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    Mic,
    System,
}

impl Channel {
    fn from_tag(tag: u8) -> Channel {
        if tag == CHANNEL_SYSTEM {
            Channel::System
        } else {
            Channel::Mic
        }
    }
    fn tag(self) -> u8 {
        match self {
            Channel::Mic => CHANNEL_MIC,
            Channel::System => CHANNEL_SYSTEM,
        }
    }
}

/// Health of a `LocalStt` engine. Drives ladder fallback in a later phase;
/// Phase 1 only needs "relaying" vs "unavailable".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
// `Unavailable` + `from_u8` are the ladder-fallback seam: a later phase's
// health watchdog reads them to switch tiers. Unused by the Tier-1-only
// Phase 1.
#[allow(dead_code)]
pub enum SttHealth {
    Starting = 0,
    Live = 1,
    Unavailable = 2,
}

impl SttHealth {
    #[allow(dead_code)]
    fn from_u8(v: u8) -> SttHealth {
        match v {
            1 => SttHealth::Live,
            2 => SttHealth::Unavailable,
            _ => SttHealth::Starting,
        }
    }
}

/// Uniform draft-segment shape shared by every tier. Phase 1 receives these
/// as raw JSON from the backend and forwards them verbatim, but the type is
/// the poll seam for a future on-device engine (`LocalStt::poll_segment`).
/// Mirrors `agent/packages/shared/types.ts` `LiveSegment`.
///
/// Constructed by a future on-device engine via `LocalStt::poll_segment`; the
/// Phase-1 relay forwards the backend's raw segment JSON instead, so this type
/// is a seam placeholder for now.
#[allow(dead_code)]
#[derive(Clone, Debug, Serialize)]
pub struct LiveSegment {
    pub channel: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speaker: Option<String>,
    pub start_ms: i64,
    pub end_ms: i64,
    pub text: String,
    pub provisional: bool,
}

/// The engine seam. A Tier-0 on-device transcriber would produce segments
/// locally (`poll_segment`); the Phase-1 [`Tier1Relay`] instead forwards PCM
/// upstream and lets the backend produce segments out-of-band (delivered as
/// `live-segment` events straight from the WS reader), so its `poll_segment`
/// is always `None`.
///
/// `poll_segment` + `health` are the ladder seam: a later on-device tier and
/// the fallback watchdog call them. Unused in the Tier-1-only Phase 1.
#[allow(dead_code)]
pub trait LocalStt: Send {
    /// Feed one chunk of 16 kHz mono i16 PCM for a channel.
    fn feed(&mut self, channel: Channel, pcm_16k: &[i16]);
    /// Poll a locally-produced segment, if the engine transcribes on-device.
    fn poll_segment(&mut self) -> Option<LiveSegment> {
        None
    }
    /// Current health of the engine.
    fn health(&self) -> SttHealth;
}

/// Phase-1 engine: encodes each PCM chunk as a `[tag][pcm-le]` binary frame
/// and hands it to the WS writer task via an unbounded channel. Segments come
/// back over the same socket and are emitted by the reader task.
pub struct Tier1Relay {
    out: mpsc::UnboundedSender<Vec<u8>>,
    // Read by `health()` (part of the ladder seam) — updated in Phase 1 but
    // only consumed by a later-phase fallback watchdog.
    #[allow(dead_code)]
    health: Arc<AtomicU8>,
}

impl LocalStt for Tier1Relay {
    fn feed(&mut self, channel: Channel, pcm_16k: &[i16]) {
        let mut buf = Vec::with_capacity(1 + pcm_16k.len() * 2);
        buf.push(channel.tag());
        for s in pcm_16k {
            buf.extend_from_slice(&s.to_le_bytes());
        }
        // Best-effort: if the writer task is gone the session is ending.
        let _ = self.out.send(buf);
    }
    fn health(&self) -> SttHealth {
        SttHealth::from_u8(self.health.load(Ordering::Relaxed))
    }
}

/// One device-native audio chunk copied off a cpal stream. Interleaved f32,
/// native rate + channel count; the relay downmixes + resamples.
struct LiveFrame {
    tag: u8,
    samples: Vec<f32>,
    sample_rate: u32,
    channels: u16,
}

struct RingInner {
    q: Mutex<VecDeque<LiveFrame>>,
    notify: Notify,
}

/// Bounded, drop-oldest producer handed to the recorder. Cloneable so the
/// mic + system cpal callbacks share one ring. Pushing is lock-brief and
/// uses a lock *separate* from the WAV writer's, so it can never stall the
/// lossless WAV path.
#[derive(Clone)]
pub struct LiveTap {
    inner: Arc<RingInner>,
}

impl LiveTap {
    /// Copy a device callback buffer into the live ring. `samples` is
    /// interleaved native audio already converted to f32. Called from the
    /// cpal audio thread — stays cheap and never blocks.
    pub fn push(&self, channel_tag: u8, samples: Vec<f32>, sample_rate: u32, channels: u16) {
        {
            let mut q = self.inner.q.lock().unwrap();
            if q.len() >= RING_CAP {
                q.pop_front(); // drop OLDEST
            }
            q.push_back(LiveFrame {
                tag: channel_tag,
                samples,
                sample_rate,
                channels,
            });
        }
        self.inner.notify.notify_one();
    }
}

struct ActiveLive {
    stop_tx: watch::Sender<bool>,
}

/// Tauri-managed live-relay controller. `begin()` at record-start returns a
/// `LiveTap` for the recorder and spawns the async driver; `end()` at
/// record-stop signals a clean shutdown (flush `stop` frame, close socket).
pub struct LiveRelay {
    active: Mutex<Option<ActiveLive>>,
}

impl LiveRelay {
    pub fn new() -> Self {
        Self {
            active: Mutex::new(None),
        }
    }

    /// Begin a live session keyed on `session_uuid`. Returns the tap the
    /// recorder feeds. The relay connects on the async runtime; the caller
    /// never blocks and a connect failure only degrades the live draft.
    ///
    /// `contact_hint` (#653) is the Zoho contact id the user pre-picked in
    /// the co-pilot panel, forwarded on the WS `start` frame so the backend
    /// can resolve the counterpart off the audio hot path. `None` when the
    /// user didn't pick (or copilot is off) — additive + non-breaking.
    pub fn begin(
        &self,
        app: AppHandle,
        api_base: String,
        session_uuid: String,
        contact_hint: Option<String>,
    ) -> LiveTap {
        let inner = Arc::new(RingInner {
            q: Mutex::new(VecDeque::new()),
            notify: Notify::new(),
        });
        let tap = LiveTap {
            inner: inner.clone(),
        };
        let (stop_tx, stop_rx) = watch::channel(false);
        {
            let mut guard = self.active.lock().unwrap();
            // Signal any stale predecessor to stop before replacing it.
            if let Some(prev) = guard.take() {
                let _ = prev.stop_tx.send(true);
            }
            *guard = Some(ActiveLive { stop_tx });
        }
        tauri::async_runtime::spawn(run_relay(
            app,
            api_base,
            session_uuid,
            contact_hint,
            inner,
            stop_rx,
        ));
        tap
    }

    /// Signal the active session (if any) to flush + close. No-op when idle.
    pub fn end(&self) {
        if let Some(active) = self.active.lock().unwrap().take() {
            let _ = active.stop_tx.send(true);
        }
    }
}

impl Default for LiveRelay {
    fn default() -> Self {
        Self::new()
    }
}

async fn run_relay(
    app: AppHandle,
    api_base: String,
    session_uuid: String,
    contact_hint: Option<String>,
    ring: Arc<RingInner>,
    stop_rx: watch::Receiver<bool>,
) {
    match run_relay_inner(&app, &api_base, &session_uuid, contact_hint, ring, stop_rx).await {
        Ok(()) => emit_session(&app, "ended", None),
        Err(e) => {
            // Vendor-opaque + detail-free to the UI; full context to stderr.
            eprintln!("aftercalls: live relay unavailable: {e:#}");
            emit_session(&app, "error", None);
        }
    }
}

async fn run_relay_inner(
    app: &AppHandle,
    api_base: &str,
    session_uuid: &str,
    contact_hint: Option<String>,
    ring: Arc<RingInner>,
    mut stop_rx: watch::Receiver<bool>,
) -> Result<()> {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::http::HeaderValue;

    // Fresh bearer via the same refresh-aware path the HTTP client uses.
    let cfg = crate::config::Config::load().context("load config for live relay")?;
    let backend = cfg
        .backend
        .ok_or_else(|| anyhow!("no backend configured"))?;
    let bearer = crate::portal::build_auth_header(&backend).await?;

    let ws_url = ws_url_from_base(api_base);
    let mut req = ws_url
        .as_str()
        .into_client_request()
        .context("build live ws request")?;
    req.headers_mut().insert(
        "Authorization",
        HeaderValue::from_str(&bearer).context("bearer header")?,
    );

    let (ws, _resp) = tokio_tungstenite::connect_async(req)
        .await
        .context("open live ws")?;
    let (mut write, mut read) = ws.split();

    // start frame. #653 — attach the pre-picked Zoho contact id when the
    // user chose one in the co-pilot panel; omitted otherwise so the
    // backend's `Option<String>` field defaults to `None` (older agents
    // never send it — additive, non-breaking).
    let mut start = serde_json::json!({
        "type": "start",
        "session_uuid": session_uuid,
        "sample_rate": TARGET_RATE,
    });
    if let Some(hint) = &contact_hint {
        start["contact_hint"] = serde_json::Value::String(hint.clone());
    }
    write
        .send(Message::text(start.to_string()))
        .await
        .context("send start frame")?;

    let health = Arc::new(AtomicU8::new(SttHealth::Starting as u8));
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let mut stt: Box<dyn LocalStt> = Box::new(Tier1Relay {
        out: out_tx,
        health: health.clone(),
    });

    // Reader task: backend segment/session JSON → Tauri events.
    let app_r = app.clone();
    let mut stop_r = stop_rx.clone();
    let reader = tauri::async_runtime::spawn(async move {
        loop {
            if *stop_r.borrow() {
                break;
            }
            tokio::select! {
                _ = stop_r.changed() => break,
                msg = read.next() => match msg {
                    Some(Ok(Message::Text(t))) => forward_incoming(&app_r, t.as_str()),
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {}
                    Some(Err(_)) => break,
                },
            }
        }
    });

    // Writer task: outbound audio frames + clean stop → WS.
    let mut stop_w = stop_rx.clone();
    let writer = tauri::async_runtime::spawn(async move {
        loop {
            if *stop_w.borrow() {
                break;
            }
            tokio::select! {
                _ = stop_w.changed() => break,
                frame = out_rx.recv() => match frame {
                    Some(bytes) => {
                        if write.send(Message::binary(bytes)).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                },
            }
        }
        // Best-effort clean close: tell the backend the session ended.
        let _ = write
            .send(Message::text(serde_json::json!({ "type": "stop" }).to_string()))
            .await;
        let _ = write.close().await;
    });

    health.store(SttHealth::Live as u8, Ordering::Relaxed);

    // Pump: drain the recorder ring → downmix/resample/VAD → stt.feed().
    let mut pipes = Pipelines::default();
    loop {
        if *stop_rx.borrow() {
            break;
        }
        tokio::select! {
            _ = stop_rx.changed() => break,
            _ = ring.notify.notified() => {
                loop {
                    let frame = { ring.q.lock().unwrap().pop_front() };
                    let Some(frame) = frame else { break };
                    if let Some((ch, pcm)) = pipes.process(frame) {
                        stt.feed(ch, &pcm);
                    }
                }
            }
        }
    }

    // Stop signalled — writer/reader observe the same watch and wind down.
    let _ = reader.await;
    let _ = writer.await;
    Ok(())
}

/// Per-channel resample + VAD state, created lazily as each lane's device
/// format is first observed.
#[derive(Default)]
struct Pipelines {
    mic: Option<ChannelPipe>,
    system: Option<ChannelPipe>,
}

impl Pipelines {
    fn process(&mut self, frame: LiveFrame) -> Option<(Channel, Vec<i16>)> {
        let channel = Channel::from_tag(frame.tag);
        let slot = match channel {
            Channel::Mic => &mut self.mic,
            Channel::System => &mut self.system,
        };
        if slot.is_none() {
            match ChannelPipe::new(frame.sample_rate, frame.channels) {
                Ok(p) => *slot = Some(p),
                Err(e) => {
                    eprintln!("aftercalls: live resampler init failed: {e:#}");
                    return None;
                }
            }
        }
        let pipe = slot.as_mut().unwrap();
        pipe.push_and_maybe_emit(&frame.samples)
            .map(|pcm| (channel, pcm))
    }
}

struct ChannelPipe {
    resampler: rubato::Async<f32>,
    vad: earshot::Detector,
    src_rate: u32,
    src_channels: u16,
    /// Accumulated mono device samples awaiting the next ~200 ms resample.
    accum: Vec<f32>,
}

impl ChannelPipe {
    fn new(src_rate: u32, src_channels: u16) -> Result<Self> {
        let rate = src_rate.max(1);
        let ratio = TARGET_RATE as f64 / rate as f64;
        let resampler = rubato::Async::<f32>::new_poly(
            ratio,
            1.0,
            rubato::PolynomialDegree::Cubic,
            1024,
            1,
            rubato::FixedAsync::Input,
        )
        .map_err(|e| anyhow!("build resampler: {e}"))?;
        Ok(Self {
            resampler,
            vad: earshot::Detector::default(),
            src_rate: rate,
            src_channels: src_channels.max(1),
            accum: Vec::new(),
        })
    }

    /// Downmix to mono, buffer ~200 ms, then resample to 16 kHz i16.
    /// Returns the PCM chunk to send — SILENCE INCLUDED, because the STT
    /// provider's turn detection needs to hear the pauses between utterances
    /// to end a turn — or `None` when more audio is still needed.
    fn push_and_maybe_emit(&mut self, interleaved: &[f32]) -> Option<Vec<i16>> {
        let ch = self.src_channels as usize;
        let frames = interleaved.len() / ch;
        self.accum.reserve(frames);
        for i in 0..frames {
            let mut sum = 0.0f32;
            for c in 0..ch {
                sum += interleaved[i * ch + c];
            }
            self.accum.push(sum / ch as f32);
        }

        let threshold = (self.src_rate as usize * FRAME_MS) / 1000;
        if self.accum.len() < threshold {
            return None;
        }

        let input: Vec<f32> = std::mem::take(&mut self.accum);
        let adapter = rubato::audioadapter_buffers::direct::InterleavedSlice::new(
            &input[..],
            1,
            input.len(),
        )
        .ok()?;
        let out = self
            .resampler
            .process_all(&adapter, input.len(), None)
            .ok()?;
        let data: Vec<f32> = out.take_data();
        if data.is_empty() {
            return None;
        }

        let pcm: Vec<i16> = data
            .iter()
            .map(|&x| (x.clamp(-1.0, 1.0) * 32767.0) as i16)
            .collect();

        // Always send the chunk, INCLUDING silence. The realtime STT provider's
        // turn detection relies on hearing the pauses between utterances to end
        // a turn; VAD-gating the silence produced one endless turn that only
        // finalized at hang-up — no live line breaks, finals only at the end.
        // Keep feeding the detector so its window state stays continuous (score
        // unused for now; reserved for a future "who's talking" hint).
        let mut i = 0;
        while i + VAD_FRAME <= pcm.len() {
            let _ = self.vad.predict_i16(&pcm[i..i + VAD_FRAME]);
            i += VAD_FRAME;
        }
        Some(pcm)
    }
}

/// `https://…` → `wss://…/v1/live/ws` (and `http` → `ws`). Leaves an
/// already-`ws(s)` base untouched.
fn ws_url_from_base(base: &str) -> String {
    let b = base.trim_end_matches('/');
    let b = if let Some(rest) = b.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = b.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        b.to_string()
    };
    format!("{b}/v1/live/ws")
}

/// Forward a backend text frame to the webview. `segment` → `live-segment`,
/// `session` → `live-session`, `coaching` → `live-coaching`, `live_cue` →
/// `live-cue`, `checklist` → `live-checklist`; the raw JSON rides through so the
/// UI type stays the single wire-shape authority. Unknown types are ignored
/// (older agents drop the #654 `coaching` + #659-P2 `live_cue` + #659-P3
/// `checklist` frames — additive, non-breaking).
fn forward_incoming(app: &AppHandle, text: &str) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
        return;
    };
    match v.get("type").and_then(|t| t.as_str()) {
        Some("segment") => {
            let _ = app.emit("live-segment", v);
        }
        Some("session") => {
            // #659 P4 — mirror the session status into the overlay cold-start
            // cache so a freshly-opened overlay hydrates the live/ended state.
            if let Some(cache) = app.try_state::<crate::LiveSnapshotCache>() {
                if let Some(s) = v.get("status").and_then(|s| s.as_str()) {
                    cache.set_status(s);
                }
            }
            let _ = app.emit("live-session", v);
        }
        // #654 — live coaching snapshot for the Coaching lane.
        Some("coaching") => {
            // #659 P4 — stash the latest FULL coaching snapshot so a
            // freshly-opened overlay renders the top cue + sentiment instantly
            // instead of waiting up to ~20s for the next frame.
            if let Some(cache) = app.try_state::<crate::LiveSnapshotCache>() {
                cache.set_coaching(v.clone());
            }
            let _ = app.emit("live-coaching", v);
        }
        // #659 (P2) — a FAST-lane cue (battlecard / deal-risk) for the
        // IntelligenceLane's live-cue section.
        Some("live_cue") => {
            let _ = app.emit("live-cue", v);
        }
        // #659 (P3) — auto-checking agenda checklist snapshot for the
        // IntelligenceLane's checklist section.
        Some("checklist") => {
            let _ = app.emit("live-checklist", v);
        }
        _ => {}
    }
}

fn emit_session(app: &AppHandle, status: &str, message: Option<&str>) {
    // #659 P4 — mirror the terminal status ("ended" / "error") into the
    // overlay cold-start cache so an overlay opened during the stop grace
    // period shows the frozen final state, not a stale "live".
    if let Some(cache) = app.try_state::<crate::LiveSnapshotCache>() {
        cache.set_status(status);
    }
    let payload = serde_json::json!({
        "type": "session",
        "status": status,
        "message": message,
    });
    let _ = app.emit("live-session", payload);
}
