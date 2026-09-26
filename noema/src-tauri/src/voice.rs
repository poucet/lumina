//! Native audio capture for Noema via cpal.
//!
//! The webview could use `navigator.mediaDevices.getUserMedia` + an
//! AudioWorklet (that's what admin does), but noema is a Tauri app with a
//! Rust backend — going through the webview forces HTTPS, hides device
//! selection, and pipes PCM back through JSON. Doing it in Rust lets us:
//!   - enumerate input devices (user-visible mic picker),
//!   - downmix / resample to the daemon's expected 16 kHz mono PCM16,
//!   - open the daemon's voice WS directly and forward `VoiceEvent`s back to
//!     the webview as Tauri events.
//!
//! The webview layer just calls `start_voice_capture` / `stop_voice_capture`
//! and listens for `voice_event`.
//!
//! cpal's `Stream` is `!Send` on macOS, so we own it on a dedicated OS thread
//! — dropping the stream on that thread stops capture.

use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, StreamConfig};
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use tauri::{AppHandle, Emitter, State};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

use crate::state::AppState;

/// Daemon voice stream expects mono PCM16 @ 16 kHz. Resampling on the Noema
/// side keeps providers implementation-agnostic.
const TARGET_SAMPLE_RATE: u32 = 16_000;

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct AudioDeviceInfo {
    pub id: String,
    pub name: String,
    pub is_default: bool,
}

/// Handle to an active capture. Kept in app state so `start` / `stop` can
/// find and tear it down.
pub(crate) struct CaptureHandle {
    /// Signals the cpal thread to drop its `Stream`.
    stream_stop: std::sync::mpsc::Sender<()>,
    /// Signals the WS pump to exit.
    ws_stop: mpsc::Sender<()>,
    /// WS pump task — aborted on stop as a belt-and-braces.
    ws_task: JoinHandle<()>,
}

#[derive(Default)]
pub struct VoiceCaptureState(pub Mutex<Option<CaptureHandle>>);

// ---------------------------------------------------------------------------
// Tauri commands
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn list_audio_devices() -> Result<Vec<AudioDeviceInfo>, String> {
    let host = cpal::default_host();
    let default_name = host.default_input_device().and_then(|d| d.name().ok());
    let devices = host
        .input_devices()
        .map_err(|e| format!("enumerate input devices: {e}"))?
        .filter_map(|d| {
            let name = d.name().ok()?;
            let is_default = default_name.as_deref() == Some(name.as_str());
            Some(AudioDeviceInfo {
                id: name.clone(),
                name,
                is_default,
            })
        })
        .collect();
    Ok(devices)
}

#[tauri::command]
pub async fn start_voice_capture(
    app: AppHandle,
    app_state: State<'_, Arc<AppState>>,
    capture_state: State<'_, Arc<VoiceCaptureState>>,
    provider_id: String,
    device_id: Option<String>,
) -> Result<(), String> {
    // Idempotent: stop any previous session first so the button can be a
    // simple toggle in the UI.
    stop_capture_locked(&capture_state).await;

    let base = app_state
        .rest_base_url
        .get()
        .cloned()
        .ok_or_else(|| "daemon not initialized".to_string())?;
    let ws_url = base
        .replace("https://", "wss://")
        .replace("http://", "ws://");
    let ws_url = format!("{ws_url}/api/voice/stream/{provider_id}");

    let (ws_stream, _resp) = connect_async(&ws_url)
        .await
        .map_err(|e| format!("voice WS connect ({ws_url}): {e}"))?;
    let (mut ws_tx, mut ws_rx) = ws_stream.split();

    // Pump PCM from cpal → WS across threads. Use unbounded so the audio
    // callback (which must be fast) never blocks on a full channel.
    let (pcm_tx, mut pcm_rx) = mpsc::unbounded_channel::<Vec<i16>>();
    let (stream_stop_tx, stream_stop_rx) = std::sync::mpsc::channel();

    spawn_cpal_capture(device_id, pcm_tx, stream_stop_rx)
        .map_err(|e| format!("start cpal capture: {e}"))?;

    let (ws_stop_tx, mut ws_stop_rx) = mpsc::channel::<()>(1);
    let ws_task = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = ws_stop_rx.recv() => break,
                Some(pcm) = pcm_rx.recv() => {
                    let bytes: Vec<u8> = pcm.iter().flat_map(|s| s.to_le_bytes()).collect();
                    let msg = serde_json::json!({ "Audio": { "data": bytes } });
                    if ws_tx.send(Message::Text(msg.to_string().into())).await.is_err() {
                        break;
                    }
                }
                Some(frame) = ws_rx.next() => {
                    match frame {
                        Ok(Message::Text(text)) => {
                            match serde_json::from_str::<serde_json::Value>(&text) {
                                Ok(parsed) => {
                                    // rpc_service's stream dispatch wraps events
                                    // as WsNotification { method, params }; swallow
                                    // the ack ({ id, result }) on connect.
                                    if parsed.get("id").is_some() && parsed.get("result").is_some() {
                                        continue;
                                    }
                                    let payload = parsed.get("params").cloned().unwrap_or(parsed);
                                    let _ = app.emit("voice_event", payload);
                                }
                                Err(e) => tracing::warn!(error = %e, text = %text, "voice WS: bad JSON"),
                            }
                        }
                        Ok(Message::Close(_)) => break,
                        Ok(_) => {}
                        Err(e) => {
                            tracing::warn!(error = %e, "voice WS: receive error");
                            break;
                        }
                    }
                }
                else => break,
            }
        }
    });

    *capture_state.0.lock().unwrap() = Some(CaptureHandle {
        stream_stop: stream_stop_tx,
        ws_stop: ws_stop_tx,
        ws_task,
    });
    Ok(())
}

#[tauri::command]
pub async fn stop_voice_capture(
    capture_state: State<'_, Arc<VoiceCaptureState>>,
) -> Result<(), String> {
    stop_capture_locked(&capture_state).await;
    Ok(())
}

async fn stop_capture_locked(capture_state: &Arc<VoiceCaptureState>) {
    let handle = capture_state.0.lock().unwrap().take();
    if let Some(h) = handle {
        let _ = h.stream_stop.send(());
        let _ = h.ws_stop.send(()).await;
        h.ws_task.abort();
    }
}

// ---------------------------------------------------------------------------
// cpal capture on a dedicated thread
// ---------------------------------------------------------------------------

fn spawn_cpal_capture(
    device_id: Option<String>,
    pcm_tx: mpsc::UnboundedSender<Vec<i16>>,
    stop: std::sync::mpsc::Receiver<()>,
) -> Result<()> {
    let host = cpal::default_host();
    let device = match device_id.as_deref() {
        Some(id) => host
            .input_devices()
            .context("enumerate devices")?
            .find(|d| d.name().ok().as_deref() == Some(id))
            .ok_or_else(|| anyhow!("input device not found: {id}"))?,
        None => host
            .default_input_device()
            .ok_or_else(|| anyhow!("no default input device"))?,
    };

    let config = device
        .default_input_config()
        .context("default input config")?;
    let sample_format = config.sample_format();
    let input_rate = config.sample_rate().0;
    let input_channels = config.channels();
    let stream_config: StreamConfig = config.into();

    // Build + own the stream on a dedicated OS thread. cpal::Stream is !Send
    // on macOS, so we can't hand it back to the tokio runtime.
    thread::spawn(move || {
        let err_fn = |e| tracing::error!(error = %e, "cpal input stream error");
        let build_result = match sample_format {
            SampleFormat::F32 => {
                let pcm_tx = pcm_tx.clone();
                device.build_input_stream(
                    &stream_config,
                    move |data: &[f32], _: &cpal::InputCallbackInfo| {
                        let pcm = process_f32(data, input_channels, input_rate);
                        let _ = pcm_tx.send(pcm);
                    },
                    err_fn,
                    None,
                )
            }
            SampleFormat::I16 => {
                let pcm_tx = pcm_tx.clone();
                device.build_input_stream(
                    &stream_config,
                    move |data: &[i16], _: &cpal::InputCallbackInfo| {
                        let pcm = process_i16(data, input_channels, input_rate);
                        let _ = pcm_tx.send(pcm);
                    },
                    err_fn,
                    None,
                )
            }
            SampleFormat::U16 => {
                let pcm_tx = pcm_tx.clone();
                device.build_input_stream(
                    &stream_config,
                    move |data: &[u16], _: &cpal::InputCallbackInfo| {
                        let pcm = process_u16(data, input_channels, input_rate);
                        let _ = pcm_tx.send(pcm);
                    },
                    err_fn,
                    None,
                )
            }
            other => {
                tracing::error!(?other, "unsupported sample format");
                return;
            }
        };
        let stream = match build_result {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "build_input_stream");
                return;
            }
        };
        if let Err(e) = stream.play() {
            tracing::error!(error = %e, "stream.play");
            return;
        }
        // Park until asked to stop. Dropping `stream` here (end of scope) is
        // what actually halts capture.
        let _ = stop.recv();
        drop(stream);
    });

    Ok(())
}

// ---------------------------------------------------------------------------
// Sample conversion: any cpal format → mono PCM16 @ 16 kHz
// ---------------------------------------------------------------------------

fn process_f32(data: &[f32], channels: u16, rate: u32) -> Vec<i16> {
    let mono = downmix_f32(data, channels);
    let resampled = resample_linear(&mono, rate, TARGET_SAMPLE_RATE);
    resampled
        .into_iter()
        .map(|s| (s.clamp(-1.0, 1.0) * 32767.0) as i16)
        .collect()
}

fn process_i16(data: &[i16], channels: u16, rate: u32) -> Vec<i16> {
    let f32_samples: Vec<f32> = data.iter().map(|&s| s as f32 / 32768.0).collect();
    process_f32(&f32_samples, channels, rate)
}

fn process_u16(data: &[u16], channels: u16, rate: u32) -> Vec<i16> {
    // Shift u16 [0..65535] to signed [-32768..32767].
    let f32_samples: Vec<f32> = data
        .iter()
        .map(|&s| (s as i32 - 32768) as f32 / 32768.0)
        .collect();
    process_f32(&f32_samples, channels, rate)
}

fn downmix_f32(data: &[f32], channels: u16) -> Vec<f32> {
    if channels == 1 {
        return data.to_vec();
    }
    let ch = channels as usize;
    data.chunks(ch)
        .map(|frame| frame.iter().sum::<f32>() / ch as f32)
        .collect()
}

/// Linear interpolation resampler — fine for speech, avoids pulling rubato.
/// Quality matters less here because the daemon's STT models are robust.
fn resample_linear(src: &[f32], src_rate: u32, dst_rate: u32) -> Vec<f32> {
    if src.is_empty() || src_rate == dst_rate {
        return src.to_vec();
    }
    let ratio = src_rate as f64 / dst_rate as f64;
    let out_len = (src.len() as f64 / ratio) as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let pos = i as f64 * ratio;
        let idx = pos as usize;
        let frac = (pos - idx as f64) as f32;
        let a = src.get(idx).copied().unwrap_or(0.0);
        let b = src.get(idx + 1).copied().unwrap_or(a);
        out.push(a + (b - a) * frac);
    }
    out
}
