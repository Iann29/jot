//! Voice transcription via the Soniox real-time WebSocket API.
//!
//! Architecture:
//!
//!   [main thread / GLib]                [tokio thread]
//!         │  cmd_tx  ─────────────────────► cmd_rx
//!         │                              ─── tokio runtime ───
//!         │                              │  WebSocket (TLS)   │
//!         │                              │      │  ▲          │
//!         │                              │      ▼  │          │
//!         │                              │   Soniox cloud     │
//!         │  evt_rx  ◄───────────────────  evt_tx          ── │
//!         │                              ── audio_rx ◄────────┐
//!                                                             │
//!                                            [cpal thread]    │
//!                                            cpal::Stream ──► audio_tx
//!
//! cpal's `Stream` is `!Send`, so it lives on its own dedicated thread; an
//! `mpsc::Sender` carries the encoded audio frames into the tokio task,
//! which forwards them to Soniox over the WebSocket. Transcript events
//! and errors come back through `async_channel::Sender` so the GLib main
//! loop can `recv()` them with `glib::spawn_future_local`.

use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use async_channel::{Receiver, Sender};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio_tungstenite::tungstenite::Message;

const WS_ENDPOINT: &str = "wss://stt-rt.soniox.com/transcribe-websocket";
const SONIOX_MODEL: &str = "stt-rt-v4";
const TARGET_SAMPLE_RATE: u32 = 16_000;

/// Events the GLib main loop consumes.
#[derive(Debug)]
pub enum TranscriptEvent {
    /// WebSocket open + config accepted.
    Connected,
    /// One full token batch from the server. Per Soniox spec, each batch
    /// contains the *new* finals (append) plus the *complete current* set
    /// of non-finals (replaces previous tentatives entirely).
    Tokens(Vec<Token>),
    /// Server told us `finished: true` — session ended cleanly.
    Finished,
    /// Anything went wrong (network, audio, server error). Session is over.
    Error(String),
}

#[derive(Debug, Clone)]
pub struct Token {
    pub text: String,
    pub is_final: bool,
    #[allow(dead_code)]
    pub language: Option<String>,
}

/// Commands the GLib side sends to the tokio task.
#[derive(Debug)]
pub enum TranscriptCmd {
    /// Politely end the audio stream and wait for the server's last tokens.
    Stop,
}

/// Handle returned from `start()`. Dropping it does NOT stop transcription —
/// send `TranscriptCmd::Stop` first so the server flushes pending tokens.
pub struct TranscriptionHandle {
    pub cmd_tx: Sender<TranscriptCmd>,
}

/// Start a real-time transcription session. The returned `Receiver` yields
/// `TranscriptEvent`s on the calling thread's executor; in our app that's
/// the GLib main loop via `glib::spawn_future_local`.
pub fn start(api_key: String) -> (TranscriptionHandle, Receiver<TranscriptEvent>) {
    let (cmd_tx, cmd_rx) = async_channel::unbounded::<TranscriptCmd>();
    let (evt_tx, evt_rx) = async_channel::unbounded::<TranscriptEvent>();

    let evt_tx_for_thread = evt_tx.clone();
    std::thread::Builder::new()
        .name("jot-transcribe".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = evt_tx_for_thread
                        .send_blocking(TranscriptEvent::Error(format!("tokio rt: {e}")));
                    return;
                }
            };
            rt.block_on(async move {
                if let Err(e) = run_session(api_key, evt_tx_for_thread.clone(), cmd_rx).await {
                    let _ = evt_tx_for_thread
                        .send(TranscriptEvent::Error(format!("{e:#}")))
                        .await;
                }
            });
        })
        .expect("spawn jot-transcribe thread");

    (TranscriptionHandle { cmd_tx }, evt_rx)
}

#[derive(Debug, Deserialize)]
struct ServerToken {
    text: String,
    #[serde(default)]
    is_final: bool,
    #[serde(default)]
    language: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ServerMessage {
    #[serde(default)]
    tokens: Vec<ServerToken>,
    #[serde(default)]
    finished: bool,
    // Soniox sends `error_code` as either a string (e.g. "INVALID_API_KEY")
    // or an integer HTTP-style status (e.g. 401). Accept both with an
    // untagged enum and stringify on consumption.
    #[serde(default)]
    error_code: Option<ErrorCode>,
    #[serde(default)]
    error_message: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ErrorCode {
    String(String),
    Number(i64),
}

impl std::fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ErrorCode::String(s) => f.write_str(s),
            ErrorCode::Number(n) => write!(f, "{n}"),
        }
    }
}

async fn run_session(
    api_key: String,
    evt_tx: Sender<TranscriptEvent>,
    cmd_rx: Receiver<TranscriptCmd>,
) -> Result<()> {
    // 1. Audio capture — `cpal::Stream` is !Send, so it lives on its own
    //    dedicated thread. We get back the actual sample rate the device
    //    accepted (Soniox needs to know).
    let (audio_tx, mut audio_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    let (stop_audio_tx, stop_audio_rx) = std::sync::mpsc::channel::<()>();
    let audio_ready = Arc::new((std::sync::Mutex::new(None), std::sync::Condvar::new()));
    let audio_ready_for_thread = audio_ready.clone();
    std::thread::Builder::new()
        .name("jot-audio".into())
        .spawn(move || {
            run_audio_capture(audio_tx, stop_audio_rx, audio_ready_for_thread);
        })
        .context("spawn audio thread")?;

    // Wait for the audio thread to publish the sample rate it picked.
    let sample_rate = {
        let (mtx, cvar) = &*audio_ready;
        let mut guard = mtx.lock().unwrap();
        while guard.is_none() {
            guard = cvar.wait(guard).unwrap();
        }
        guard.take().unwrap()
    };
    let sample_rate = match sample_rate {
        Ok(rate) => rate,
        Err(e) => {
            let _ = stop_audio_tx.send(());
            return Err(e);
        }
    };
    tracing::info!("audio capture running at {sample_rate} Hz mono i16");

    // 2. Open WebSocket
    let (ws, _resp) = tokio_tungstenite::connect_async(WS_ENDPOINT)
        .await
        .context("connect to Soniox")?;
    let (mut sink, mut stream) = ws.split();

    // 3. Send config message
    let config = serde_json::json!({
        "api_key": api_key,
        "model": SONIOX_MODEL,
        "audio_format": "pcm_s16le",
        "sample_rate": sample_rate,
        "num_channels": 1,
        "enable_language_identification": true,
    });
    sink.send(Message::Text(config.to_string().into()))
        .await
        .context("send Soniox config")?;
    let _ = evt_tx.send(TranscriptEvent::Connected).await;

    // 4. Main loop: forward audio, react to commands, decode server messages.
    let mut closing = false;
    let result: Result<()> = loop {
        tokio::select! {
            biased;

            // Stop command — tell Soniox we're done sending audio.
            cmd = cmd_rx.recv() => {
                match cmd {
                    Ok(TranscriptCmd::Stop) | Err(_) => {
                        if !closing {
                            closing = true;
                            let _ = stop_audio_tx.send(());
                            // An empty text frame is Soniox's "no more audio" sentinel.
                            if let Err(e) = sink.send(Message::Text(String::new().into())).await {
                                break Err(e.into());
                            }
                        }
                    }
                }
            }

            // New audio chunk to forward.
            chunk = audio_rx.recv(), if !closing => {
                match chunk {
                    Some(bytes) => {
                        if let Err(e) = sink.send(Message::Binary(bytes.into())).await {
                            break Err(e.into());
                        }
                    }
                    None => {
                        // Audio thread ended — treat as stop.
                        if !closing {
                            closing = true;
                            let _ = sink.send(Message::Text(String::new().into())).await;
                        }
                    }
                }
            }

            // Server response.
            msg = stream.next() => {
                let msg = match msg {
                    Some(Ok(m)) => m,
                    Some(Err(e)) => break Err(e.into()),
                    None => break Ok(()),
                };
                let text = match msg {
                    Message::Text(t) => t.to_string(),
                    Message::Close(_) => break Ok(()),
                    _ => continue,
                };
                let parsed: ServerMessage = match serde_json::from_str(&text) {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::warn!("malformed Soniox message: {e}: {text}");
                        continue;
                    }
                };
                if let Some(code) = parsed.error_code {
                    let detail = parsed.error_message.unwrap_or_default();
                    break Err(anyhow!("Soniox {code}: {detail}"));
                }
                if !parsed.tokens.is_empty() {
                    let batch: Vec<Token> = parsed
                        .tokens
                        .into_iter()
                        .map(|t| Token {
                            text: t.text,
                            is_final: t.is_final,
                            language: t.language,
                        })
                        .collect();
                    let _ = evt_tx.send(TranscriptEvent::Tokens(batch)).await;
                }
                if parsed.finished {
                    let _ = evt_tx.send(TranscriptEvent::Finished).await;
                    break Ok(());
                }
            }
        }
    };

    let _ = stop_audio_tx.send(());
    result
}

/// Pick an input config that's as close to 16 kHz mono i16 as the device
/// supports. Falls back to whatever the device offers natively, in which
/// case we resample-by-truth (no resampling — we just tell Soniox the real
/// rate) and downmix to mono.
fn run_audio_capture(
    audio_tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    stop_rx: std::sync::mpsc::Receiver<()>,
    ready: Arc<(std::sync::Mutex<Option<Result<u32>>>, std::sync::Condvar)>,
) {
    let publish = |result: Result<u32>| {
        let (mtx, cvar) = &*ready;
        let mut guard = mtx.lock().unwrap();
        *guard = Some(result);
        cvar.notify_all();
    };

    let host = cpal::default_host();
    let device = match host.default_input_device() {
        Some(d) => d,
        None => {
            publish(Err(anyhow!("no default audio input device")));
            return;
        }
    };

    let device_name = device.name().unwrap_or_else(|_| "<unknown>".into());

    // Try to find a 16 kHz mono i16 config first.
    let preferred = device
        .supported_input_configs()
        .ok()
        .and_then(|iter| {
            iter.filter(|c| {
                c.channels() == 1
                    && c.sample_format() == cpal::SampleFormat::I16
                    && c.min_sample_rate().0 <= TARGET_SAMPLE_RATE
                    && c.max_sample_rate().0 >= TARGET_SAMPLE_RATE
            })
            .next()
            .map(|c| c.with_sample_rate(cpal::SampleRate(TARGET_SAMPLE_RATE)))
        });

    let supported = match preferred {
        Some(c) => c,
        None => match device.default_input_config() {
            Ok(c) => c,
            Err(e) => {
                publish(Err(anyhow!("default input config: {e}")));
                return;
            }
        },
    };

    let sample_rate = supported.sample_rate().0;
    let channels = supported.channels() as usize;
    let sample_format = supported.sample_format();
    let stream_cfg: cpal::StreamConfig = supported.into();
    tracing::info!(
        "audio device='{device_name}' rate={sample_rate} ch={channels} fmt={sample_format:?}"
    );

    let err_fn = |e: cpal::StreamError| tracing::error!("audio stream error: {e}");

    let stream_result = match sample_format {
        cpal::SampleFormat::I16 => device.build_input_stream::<i16, _, _>(
            &stream_cfg,
            {
                let tx = audio_tx.clone();
                move |data, _info: &cpal::InputCallbackInfo| {
                    let bytes = encode_i16_mono(data, channels);
                    let _ = tx.try_send(bytes);
                }
            },
            err_fn,
            None,
        ),
        cpal::SampleFormat::F32 => device.build_input_stream::<f32, _, _>(
            &stream_cfg,
            {
                let tx = audio_tx.clone();
                move |data, _info: &cpal::InputCallbackInfo| {
                    let bytes = encode_f32_mono(data, channels);
                    let _ = tx.try_send(bytes);
                }
            },
            err_fn,
            None,
        ),
        cpal::SampleFormat::U16 => device.build_input_stream::<u16, _, _>(
            &stream_cfg,
            {
                let tx = audio_tx.clone();
                move |data, _info: &cpal::InputCallbackInfo| {
                    let bytes = encode_u16_mono(data, channels);
                    let _ = tx.try_send(bytes);
                }
            },
            err_fn,
            None,
        ),
        other => {
            publish(Err(anyhow!("unsupported sample format: {other:?}")));
            return;
        }
    };

    let stream = match stream_result {
        Ok(s) => s,
        Err(e) => {
            publish(Err(anyhow!("build input stream: {e}")));
            return;
        }
    };
    if let Err(e) = stream.play() {
        publish(Err(anyhow!("stream.play(): {e}")));
        return;
    }

    publish(Ok(sample_rate));

    // Block until told to stop. Dropping `stream` halts the cpal thread.
    let _ = stop_rx.recv();
    drop(stream);
}

fn encode_i16_mono(samples: &[i16], channels: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() / channels.max(1) * 2);
    if channels <= 1 {
        for s in samples {
            out.extend_from_slice(&s.to_le_bytes());
        }
    } else {
        for chunk in samples.chunks_exact(channels) {
            let avg: i32 = chunk.iter().map(|&s| s as i32).sum::<i32>() / channels as i32;
            let s = avg.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
            out.extend_from_slice(&s.to_le_bytes());
        }
    }
    out
}

fn encode_f32_mono(samples: &[f32], channels: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() / channels.max(1) * 2);
    if channels <= 1 {
        for &f in samples {
            let s = (f.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
            out.extend_from_slice(&s.to_le_bytes());
        }
    } else {
        for chunk in samples.chunks_exact(channels) {
            let avg: f32 = chunk.iter().sum::<f32>() / channels as f32;
            let s = (avg.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
            out.extend_from_slice(&s.to_le_bytes());
        }
    }
    out
}

fn encode_u16_mono(samples: &[u16], channels: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() / channels.max(1) * 2);
    let convert = |u: u16| -> i16 { (u as i32 - 32_768) as i16 };
    if channels <= 1 {
        for &u in samples {
            out.extend_from_slice(&convert(u).to_le_bytes());
        }
    } else {
        for chunk in samples.chunks_exact(channels) {
            let avg: i32 = chunk.iter().map(|&u| u as i32 - 32_768).sum::<i32>() / channels as i32;
            let s = avg.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
            out.extend_from_slice(&s.to_le_bytes());
        }
    }
    out
}
