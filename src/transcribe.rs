//! Voice transcription via Groq's Whisper-Large-v3-Turbo.
//!
//! Whisper has no streaming variant: it's a batch encoder-decoder. To get
//! a "live" feel we cut the captured audio into utterance-shaped chunks
//! at natural pauses (RMS-based VAD) and POST each chunk to Groq in
//! parallel. Groq runs Whisper at ~216× real-time on its LPU hardware,
//! so a 5-second utterance is transcribed in ~25 ms — the perceived
//! latency is dominated by the silence the VAD waits for, not the
//! network or the model.
//!
//! Architecture:
//!
//!   [main thread / GLib]                [worker thread]
//!         │  cmd_tx  ─────────────────►  cmd_rx
//!         │                              │
//!         │                              │  ┌──── [cpal thread] ──┐
//!         │                              │  │ stream → audio_tx ──┼──► chunker
//!         │                              │  └─────────────────────┘   │
//!         │                              │                            │
//!         │                              │  ┌── reqwest blocking POST ┘
//!         │                              │  │  (one per chunk, fire-and-forget)
//!         │                              ▼  ▼
//!         │  evt_rx  ◄────────────────── evt_tx (Recording, Chunk{seq,text}, …)
//!
//! Chunks are tagged with a monotonically increasing sequence number so
//! the GLib side can keep transcripts in spoken order even if the HTTP
//! responses arrive out of order.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use async_channel::{Receiver, Sender};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

const GROQ_ENDPOINT: &str = "https://api.groq.com/openai/v1/audio/transcriptions";
const GROQ_MODEL: &str = "whisper-large-v3-turbo";
const TARGET_SAMPLE_RATE: u32 = 16_000;
const MIN_BILLED_SECONDS: f32 = 10.0;

// VAD knobs — RMS amplitude over a 20 ms window. Tuned for typical desk
// mics; loud rooms may need a higher floor.
const VAD_WINDOW_MS: usize = 20;
const VAD_SILENCE_RMS: f32 = 0.012;
/// Pause that closes a chunk. A typical sentence-ending pause is 250-500 ms;
/// 500 ms is conservative — drop to 350 if it feels laggy.
const VAD_TRAILING_SILENCE_MS: u32 = 500;
/// Don't split a chunk shorter than this — produces ungrammatical fragments.
const MIN_CHUNK_MS: u32 = 1_400;
/// Hard cap. Force a flush if the user keeps talking past this.
const MAX_CHUNK_MS: u32 = 25_000;
/// Drop chunks that contain effectively no voiced audio.
const MIN_VOICED_FRACTION: f32 = 0.04;
/// How long we wait for the capture thread to report its sample rate before
/// giving up. cpal's WASAPI host can `panic!` on an unexpected COM failure and
/// has other unwrap paths; without a bound, such a thread never publishes and
/// the session thread parks forever — no `Finished`, UI stuck in "recording".
const AUDIO_START_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug)]
pub enum TranscriptEvent {
    /// Capture started; UI can flip to "recording".
    Recording,
    /// A chunk has been closed and dispatched. Useful to show "transcribing…"
    /// dots if the user wants to know we're chewing on something.
    ChunkPending {
        seq: u64,
    },
    /// A transcript came back. `seq` matches the `ChunkPending`.
    Chunk {
        seq: u64,
        text: String,
    },
    /// Recording stopped, no more chunks coming after this. Includes any
    /// final partial chunk we flushed at stop time.
    Finished,
    Error(String),
}

#[derive(Debug)]
pub enum TranscriptCmd {
    Stop,
}

pub struct TranscriptionHandle {
    pub cmd_tx: Sender<TranscriptCmd>,
}

pub fn start(
    api_key: String,
    language: Option<String>,
) -> (TranscriptionHandle, Receiver<TranscriptEvent>) {
    let (cmd_tx, cmd_rx) = async_channel::unbounded::<TranscriptCmd>();
    let (evt_tx, evt_rx) = async_channel::unbounded::<TranscriptEvent>();

    let evt_tx_for_thread = evt_tx.clone();
    std::thread::Builder::new()
        .name("jot-transcribe".into())
        .spawn(move || {
            if let Err(e) = run_session(api_key, language, evt_tx_for_thread.clone(), cmd_rx) {
                let _ = evt_tx_for_thread.send_blocking(TranscriptEvent::Error(format!("{e:#}")));
            }
            let _ = evt_tx_for_thread.send_blocking(TranscriptEvent::Finished);
        })
        .expect("spawn jot-transcribe thread");

    (TranscriptionHandle { cmd_tx }, evt_rx)
}

fn run_session(
    api_key: String,
    language: Option<String>,
    evt_tx: Sender<TranscriptEvent>,
    cmd_rx: Receiver<TranscriptCmd>,
) -> Result<()> {
    // Audio capture lives on its own thread (cpal::Stream is !Send).
    let (audio_tx, audio_rx) = std::sync::mpsc::channel::<Vec<i16>>();
    let (stop_audio_tx, stop_audio_rx) = std::sync::mpsc::channel::<()>();
    let ready = Arc::new((std::sync::Mutex::new(None), std::sync::Condvar::new()));
    let ready_for_thread = ready.clone();

    let evt_tx_for_audio = evt_tx.clone();
    std::thread::Builder::new()
        .name("jot-audio".into())
        .spawn(move || {
            run_audio_capture(audio_tx, stop_audio_rx, ready_for_thread, evt_tx_for_audio)
        })
        .context("spawn audio thread")?;

    let sample_rate = {
        let (mtx, cvar) = &*ready;
        let mut g = mtx.lock().unwrap();
        let deadline = Instant::now() + AUDIO_START_TIMEOUT;
        while g.is_none() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let (next, res) = cvar.wait_timeout(g, remaining).unwrap();
            g = next;
            if res.timed_out() && g.is_none() {
                break;
            }
        }
        match g.take() {
            Some(r) => r,
            None => Err(anyhow!(
                "audio capture did not start within {}s",
                AUDIO_START_TIMEOUT.as_secs()
            )),
        }
    }
    .inspect_err(|_e| {
        let _ = stop_audio_tx.send(());
    })?;

    let _ = evt_tx.send_blocking(TranscriptEvent::Recording);

    // Reuse a single blocking client so TLS handshakes amortise.
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(45))
        .build()
        .context("reqwest client")?;
    let client = Arc::new(client);

    let mut chunker = Chunker::new(sample_rate);
    let mut next_seq: u64 = 0;
    let mut got_stop = false;

    loop {
        // Drain whatever audio is waiting first.
        loop {
            match audio_rx.try_recv() {
                Ok(samples) => chunker.feed(&samples),
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    got_stop = true;
                    break;
                }
            }
        }

        // Fire any chunks the VAD has finished.
        while let Some(chunk) = chunker.take_ready_chunk(got_stop) {
            let seq = next_seq;
            next_seq += 1;
            dispatch_chunk(&client, &api_key, language.clone(), chunk, seq, &evt_tx);
        }

        // Stop command?
        if !got_stop {
            match cmd_rx.try_recv() {
                Ok(TranscriptCmd::Stop) => {
                    got_stop = true;
                }
                Err(async_channel::TryRecvError::Empty) => {}
                Err(async_channel::TryRecvError::Closed) => {
                    got_stop = true;
                }
            }
        }

        if got_stop {
            // Tell audio capture to stop, then drain any straggling samples.
            let _ = stop_audio_tx.send(());
            while let Ok(samples) = audio_rx.try_recv() {
                chunker.feed(&samples);
            }
            // Final flush of whatever's left in the buffer.
            while let Some(chunk) = chunker.take_ready_chunk(true) {
                let seq = next_seq;
                next_seq += 1;
                dispatch_chunk(&client, &api_key, language.clone(), chunk, seq, &evt_tx);
            }
            break;
        }

        std::thread::sleep(Duration::from_millis(15));
    }

    Ok(())
}

fn dispatch_chunk(
    client: &Arc<reqwest::blocking::Client>,
    api_key: &str,
    language: Option<String>,
    chunk: AudioChunk,
    seq: u64,
    evt_tx: &Sender<TranscriptEvent>,
) {
    let _ = evt_tx.send_blocking(TranscriptEvent::ChunkPending { seq });

    let client = client.clone();
    let api_key = api_key.to_string();
    let evt_tx = evt_tx.clone();
    std::thread::Builder::new()
        .name(format!("jot-stt-{seq}"))
        .spawn(move || {
            let started = Instant::now();
            let evt = match transcribe_chunk(&client, &api_key, language.as_deref(), &chunk) {
                Ok(text) => {
                    tracing::info!(
                        "chunk {seq}: {:.2}s audio → {} chars in {} ms",
                        chunk.seconds(),
                        text.chars().count(),
                        started.elapsed().as_millis()
                    );
                    TranscriptEvent::Chunk { seq, text }
                }
                Err(e) => {
                    tracing::warn!("chunk {seq} transcription failed: {e:#}");
                    TranscriptEvent::Chunk {
                        seq,
                        text: String::new(),
                    }
                }
            };
            let _ = evt_tx.send_blocking(evt);
        })
        .expect("spawn stt request thread");
}

fn transcribe_chunk(
    client: &reqwest::blocking::Client,
    api_key: &str,
    language: Option<&str>,
    chunk: &AudioChunk,
) -> Result<String> {
    let wav = encode_wav(&chunk.samples, chunk.sample_rate);
    let part = reqwest::blocking::multipart::Part::bytes(wav)
        .file_name("audio.wav")
        .mime_str("audio/wav")?;

    let mut form = reqwest::blocking::multipart::Form::new()
        .text("model", GROQ_MODEL)
        .text("response_format", "json")
        .text("temperature", "0")
        .part("file", part);
    if let Some(lang) = language.filter(|l| !l.is_empty()) {
        form = form.text("language", lang.to_string());
    }

    let resp = client
        .post(GROQ_ENDPOINT)
        .bearer_auth(api_key)
        .multipart(form)
        .send()
        .context("groq POST")?;

    let status = resp.status();
    let body = resp.text().context("read groq response")?;
    if !status.is_success() {
        return Err(anyhow!("groq {status}: {body}"));
    }

    let parsed: serde_json::Value =
        serde_json::from_str(&body).with_context(|| format!("parse groq response: {body}"))?;
    let text = parsed
        .get("text")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_string();
    Ok(text)
}

// ─────────────────────── VAD-driven chunker ──────────────────────────

struct AudioChunk {
    samples: Vec<i16>,
    sample_rate: u32,
}

impl AudioChunk {
    fn seconds(&self) -> f32 {
        self.samples.len() as f32 / self.sample_rate as f32
    }
}

struct Chunker {
    sample_rate: u32,
    buf: Vec<i16>,
    /// Trailing silence counter — samples consecutively below VAD threshold.
    trailing_silence_samples: u32,
    /// Total voiced samples in the current buffer (RMS above threshold).
    voiced_samples: u32,
    /// Pre-computed thresholds in samples.
    silence_close_samples: u32,
    min_chunk_samples: u32,
    max_chunk_samples: u32,
    window_samples: usize,
}

impl Chunker {
    fn new(sample_rate: u32) -> Self {
        let ms_to_samples = |ms: u32| (sample_rate as u64 * ms as u64 / 1000) as u32;
        Self {
            sample_rate,
            buf: Vec::with_capacity((sample_rate * 30) as usize),
            trailing_silence_samples: 0,
            voiced_samples: 0,
            silence_close_samples: ms_to_samples(VAD_TRAILING_SILENCE_MS),
            min_chunk_samples: ms_to_samples(MIN_CHUNK_MS),
            max_chunk_samples: ms_to_samples(MAX_CHUNK_MS),
            window_samples: (sample_rate as usize * VAD_WINDOW_MS / 1000).max(64),
        }
    }

    fn feed(&mut self, samples: &[i16]) {
        // Update VAD state by looking at the new chunk in 20 ms windows.
        for window in samples.chunks(self.window_samples) {
            let rms = rms(window);
            if rms >= VAD_SILENCE_RMS {
                self.trailing_silence_samples = 0;
                self.voiced_samples = self.voiced_samples.saturating_add(window.len() as u32);
            } else {
                self.trailing_silence_samples = self
                    .trailing_silence_samples
                    .saturating_add(window.len() as u32);
            }
        }
        self.buf.extend_from_slice(samples);
    }

    fn take_ready_chunk(&mut self, force: bool) -> Option<AudioChunk> {
        let buf_len = self.buf.len() as u32;
        if buf_len == 0 {
            return None;
        }

        let hit_silence_split = self.trailing_silence_samples >= self.silence_close_samples
            && buf_len >= self.min_chunk_samples;
        let hit_size_cap = buf_len >= self.max_chunk_samples;
        let force_flush = force && buf_len >= self.sample_rate / 2; // ≥ 0.5 s

        if !(hit_silence_split || hit_size_cap || force_flush) {
            return None;
        }

        // Discard chunks that are mostly silence — those are clicks, hum,
        // mic-handling noise. Whisper hallucinates words on those.
        let voiced_fraction = self.voiced_samples as f32 / buf_len.max(1) as f32;
        if voiced_fraction < MIN_VOICED_FRACTION {
            tracing::debug!(
                "chunker dropping {:.2}s of mostly-silence (voiced {:.1}%)",
                buf_len as f32 / self.sample_rate as f32,
                voiced_fraction * 100.0
            );
            self.buf.clear();
            self.trailing_silence_samples = 0;
            self.voiced_samples = 0;
            return None;
        }

        // Pad the chunk if it's shorter than Groq's minimum billed length;
        // we'd be billed for 10s anyway, and a tiny clip can confuse the
        // model. Trail with silence.
        let mut samples = std::mem::take(&mut self.buf);
        let needed = (MIN_BILLED_SECONDS * self.sample_rate as f32) as usize;
        if samples.len() < needed {
            samples.resize(needed, 0);
        }

        self.trailing_silence_samples = 0;
        self.voiced_samples = 0;

        Some(AudioChunk {
            samples,
            sample_rate: self.sample_rate,
        })
    }
}

fn rms(samples: &[i16]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_sq: f64 = samples
        .iter()
        .map(|&s| {
            let f = s as f64 / i16::MAX as f64;
            f * f
        })
        .sum();
    (sum_sq / samples.len() as f64).sqrt() as f32
}

// ─────────────────────────── WAV encoding ────────────────────────────

fn encode_wav(samples: &[i16], sample_rate: u32) -> Vec<u8> {
    let bytes_per_sample: u32 = 2;
    let channels: u16 = 1;
    let byte_rate = sample_rate * bytes_per_sample;
    let block_align: u16 = (channels as u32 * bytes_per_sample) as u16;
    let data_size = (samples.len() as u32) * bytes_per_sample;
    let total_size = 36 + data_size;

    let mut out = Vec::with_capacity(44 + data_size as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&total_size.to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&((bytes_per_sample * 8) as u16).to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_size.to_le_bytes());
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

// ─────────────────────────── audio capture ───────────────────────────

// `evt_tx` feeds the Windows-only stream-error path below; on unix the
// parameter is deliberately unused (ALSA errors are advisory, see err_fn).
#[cfg_attr(not(windows), allow(unused_variables))]
fn run_audio_capture(
    audio_tx: std::sync::mpsc::Sender<Vec<i16>>,
    stop_rx: std::sync::mpsc::Receiver<()>,
    ready: Arc<(std::sync::Mutex<Option<Result<u32>>>, std::sync::Condvar)>,
    evt_tx: Sender<TranscriptEvent>,
) {
    let publish = |result: Result<u32>| {
        let (mtx, cvar) = &*ready;
        let mut g = mtx.lock().unwrap();
        *g = Some(result);
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

    // Try 16 kHz mono i16 first — Groq downsamples to that anyway.
    let preferred = device.supported_input_configs().ok().and_then(|mut iter| {
        iter.find(|c| {
            c.channels() == 1
                && c.sample_format() == cpal::SampleFormat::I16
                && c.min_sample_rate().0 <= TARGET_SAMPLE_RATE
                && c.max_sample_rate().0 >= TARGET_SAMPLE_RATE
        })
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

    // The error callback's meaning differs per cpal host, so its severity
    // must too:
    //   * ALSA's input worker `continue`s after every error_callback — the
    //     error is advisory and the stream keeps capturing. Tearing the
    //     session down here would kill dictation on recoverable hiccups.
    //   * WASAPI's `run_input` breaks its loop on EVERY error_callback path
    //     (device invalidation, GetBuffer failures, …) — the worker thread
    //     exits and the stream is permanently dead. Logging alone left the
    //     UI in "recording" forever with no chunks and no error, so surface
    //     a real Error plus the Finished that lets the UI unwind.
    // Only one match arm is evaluated, so exactly one closure instance
    // exists per session.
    #[cfg(not(windows))]
    let make_err_fn = || |e: cpal::StreamError| tracing::error!("audio stream error: {e}");
    #[cfg(windows)]
    let make_err_fn = || {
        let evt_tx = evt_tx.clone();
        let mut reported = false;
        move |e: cpal::StreamError| {
            tracing::error!("audio stream error: {e}");
            // WASAPI kills the stream after the first error, but belt and
            // braces: one Error/Finished pair is enough.
            if reported {
                return;
            }
            reported = true;
            let _ =
                evt_tx.send_blocking(TranscriptEvent::Error(format!("audio stream error: {e}")));
            let _ = evt_tx.send_blocking(TranscriptEvent::Finished);
        }
    };

    // WASAPI picks the format from the device's mix format, which cpal reports
    // as one of U8/I16/I32/I64/F32 — never U16. I32 (24-bit-in-32 interfaces)
    // and U8 used to fall through to the abort arm below, killing transcription
    // outright on those devices. The U16 arm stays: harmless, and ALSA uses it.
    let stream_result = match sample_format {
        cpal::SampleFormat::I16 => device.build_input_stream::<i16, _, _>(
            &stream_cfg,
            {
                let tx = audio_tx.clone();
                move |data, _| {
                    let _ = tx.send(downmix_i16(data, channels));
                }
            },
            make_err_fn(),
            None,
        ),
        cpal::SampleFormat::I32 => device.build_input_stream::<i32, _, _>(
            &stream_cfg,
            {
                let tx = audio_tx.clone();
                move |data, _| {
                    let _ = tx.send(i32_to_i16_mono(data, channels));
                }
            },
            make_err_fn(),
            None,
        ),
        cpal::SampleFormat::F32 => device.build_input_stream::<f32, _, _>(
            &stream_cfg,
            {
                let tx = audio_tx.clone();
                move |data, _| {
                    let _ = tx.send(f32_to_i16_mono(data, channels));
                }
            },
            make_err_fn(),
            None,
        ),
        cpal::SampleFormat::U16 => device.build_input_stream::<u16, _, _>(
            &stream_cfg,
            {
                let tx = audio_tx.clone();
                move |data, _| {
                    let _ = tx.send(u16_to_i16_mono(data, channels));
                }
            },
            make_err_fn(),
            None,
        ),
        cpal::SampleFormat::U8 => device.build_input_stream::<u8, _, _>(
            &stream_cfg,
            {
                let tx = audio_tx.clone();
                move |data, _| {
                    let _ = tx.send(u8_to_i16_mono(data, channels));
                }
            },
            make_err_fn(),
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
            // The most common cause on Windows is the microphone privacy
            // toggle, which surfaces as a bare HRESULT with no hint at all.
            #[cfg(windows)]
            let err = anyhow!(
                "build input stream: {e} — if this is a permission error, turn on \
                 Settings > Privacy & security > Microphone > \
                 \"Let desktop apps access your microphone\""
            );
            #[cfg(not(windows))]
            let err = anyhow!("build input stream: {e}");
            publish(Err(err));
            return;
        }
    };
    if let Err(e) = stream.play() {
        publish(Err(anyhow!("stream.play(): {e}")));
        return;
    }

    publish(Ok(sample_rate));
    let _ = stop_rx.recv();
    drop(stream);
}

fn downmix_i16(samples: &[i16], channels: usize) -> Vec<i16> {
    if channels <= 1 {
        return samples.to_vec();
    }
    samples
        .chunks_exact(channels)
        .map(|chunk| {
            let avg: i32 = chunk.iter().map(|&s| s as i32).sum::<i32>() / channels as i32;
            avg.clamp(i16::MIN as i32, i16::MAX as i32) as i16
        })
        .collect()
}

/// 32-bit signed PCM (typically 24-bit samples left-justified in an i32, which
/// WASAPI reports for pro/USB interfaces). Shifting right by 16 keeps the top
/// 16 bits — the same amplitude, just the resolution Whisper wants.
fn i32_to_i16_mono(samples: &[i32], channels: usize) -> Vec<i16> {
    if channels <= 1 {
        return samples.iter().map(|&s| (s >> 16) as i16).collect();
    }
    samples
        .chunks_exact(channels)
        .map(|chunk| {
            let avg: i64 = chunk.iter().map(|&s| (s >> 16) as i64).sum::<i64>() / channels as i64;
            avg.clamp(i16::MIN as i64, i16::MAX as i64) as i16
        })
        .collect()
}

/// 8-bit unsigned PCM: midpoint 128, so re-centre then scale to full range.
fn u8_to_i16_mono(samples: &[u8], channels: usize) -> Vec<i16> {
    let convert = |u: u8| -> i16 { (u as i16 - 128) << 8 };
    if channels <= 1 {
        return samples.iter().copied().map(convert).collect();
    }
    samples
        .chunks_exact(channels)
        .map(|chunk| {
            let avg: i32 =
                chunk.iter().map(|&u| (u as i32 - 128) << 8).sum::<i32>() / channels as i32;
            avg.clamp(i16::MIN as i32, i16::MAX as i32) as i16
        })
        .collect()
}

fn f32_to_i16_mono(samples: &[f32], channels: usize) -> Vec<i16> {
    if channels <= 1 {
        return samples
            .iter()
            .map(|&f| (f.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)
            .collect();
    }
    samples
        .chunks_exact(channels)
        .map(|chunk| {
            let avg: f32 = chunk.iter().sum::<f32>() / channels as f32;
            (avg.clamp(-1.0, 1.0) * i16::MAX as f32) as i16
        })
        .collect()
}

fn u16_to_i16_mono(samples: &[u16], channels: usize) -> Vec<i16> {
    let convert = |u: u16| -> i16 { (u as i32 - 32_768) as i16 };
    if channels <= 1 {
        return samples.iter().copied().map(convert).collect();
    }
    samples
        .chunks_exact(channels)
        .map(|chunk| {
            let avg: i32 = chunk.iter().map(|&u| u as i32 - 32_768).sum::<i32>() / channels as i32;
            avg.clamp(i16::MIN as i32, i16::MAX as i32) as i16
        })
        .collect()
}
