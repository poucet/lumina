//! /debug — owner-only diagnostics, so a stuck bot can be read from Discord.

use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::time::Duration;

use serenity::all::{
    CommandInteraction, CreateAttachment, CreateInteractionResponse,
    CreateInteractionResponseMessage,
};

use super::LuminaContext;
use crate::clock;

const DEFAULT_LOG_LINES: i64 = 100;
const MAX_LOG_LINES: i64 = 2_000;
/// Only this much of the log's end is read, however large the file.
const LOG_TAIL_BYTES: u64 = 1 << 20;
/// Window for sampling per-worker busy time.
const BUSY_SAMPLE: Duration = Duration::from_secs(1);

#[lumina_macros::command_group(description = "Owner-only diagnostics")]
#[allow(clippy::module_inception)] // command_group names the slash command after the module
mod debug {
    use super::*;

    #[sub_command(description = "Voice pipeline heartbeats for this guild")]
    pub async fn voice(lx: &LuminaContext, cmd: &CommandInteraction) -> anyhow::Result<()> {
        if !owner_only(lx, cmd).await? { return Ok(()); }
        let guild_id = cmd.guild_id.ok_or_else(|| anyhow::anyhow!("Not in a guild"))?;
        let voice_mgr = crate::commands::voice::get_voice_manager(lx).await?;
        let body = match voice_mgr.debug_snapshot(guild_id) {
            Some(lines) => format!("```\n{}\n```", lines.join("\n")),
            None => "No voice session in this guild.".to_string(),
        };
        lx.reply_ephemeral(cmd, &body).await
    }

    #[sub_command(description = "Speak text in this guild's voice call, bypassing the LLM")]
    pub async fn say(
        lx: &LuminaContext,
        cmd: &CommandInteraction,
        #[describe("Text to speak")] text: String,
        #[describe("TTS provider (default: the session's)")] provider: Option<String>,
        #[describe("Voice (default: the session's)")] voice: Option<String>,
        #[describe("Level in percent (default 100)")] gain: Option<i64>,
    ) -> anyhow::Result<()> {
        if !owner_only(lx, cmd).await? { return Ok(()); }
        let guild_id = cmd.guild_id.ok_or_else(|| anyhow::anyhow!("Not in a guild"))?;
        let voice_mgr = crate::commands::voice::get_voice_manager(lx).await?;
        let gain = gain.unwrap_or(100).clamp(0, 400) as f32 / 100.0;
        let over = crate::voice::TtsOverride { provider, voice, gain };
        let reply = if voice_mgr.play_tts_as(guild_id, &text, over).await? {
            "Queued. Watch `/debug voice` for the synth and playback."
        } else {
            "Not in a voice call here (or no TTS provider): `/voice join` first."
        };
        lx.reply_ephemeral(cmd, reply).await
    }

    #[sub_command(description = "Tokio runtime metrics and uptime")]
    pub async fn runtime(lx: &LuminaContext, cmd: &CommandInteraction) -> anyhow::Result<()> {
        if !owner_only(lx, cmd).await? { return Ok(()); }
        let body = format!("```\n{}\n```", runtime_report().await);
        lx.reply_ephemeral(cmd, &body).await
    }

    #[sub_command(description = "Tail of the current log file")]
    pub async fn logs(
        lx: &LuminaContext,
        cmd: &CommandInteraction,
        #[describe("Lines to return (default 100, max 2000)")] lines: Option<i64>,
    ) -> anyhow::Result<()> {
        if !owner_only(lx, cmd).await? { return Ok(()); }
        let n = lines.unwrap_or(DEFAULT_LOG_LINES).clamp(1, MAX_LOG_LINES) as usize;
        let tail = tokio::task::spawn_blocking(move || -> anyhow::Result<(PathBuf, String)> {
            let path = newest_log()?;
            let tail = tail_lines(&path, n)?;
            Ok((path, tail))
        }).await?;
        let (path, tail) = match tail {
            Ok(t) => t,
            Err(e) => return lx.reply_ephemeral(cmd, &format!("No log: {e}")).await,
        };
        let name = path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
        cmd.create_response(&lx.http, CreateInteractionResponse::Message(
            CreateInteractionResponseMessage::new()
                .content(format!("Last {n} lines of `{name}`"))
                .add_file(CreateAttachment::bytes(tail.into_bytes(), format!("{name}.txt")))
                .ephemeral(true),
        )).await?;
        Ok(())
    }
}

/// Reply "owner only" to anyone else. Returns whether to go on.
async fn owner_only(lx: &LuminaContext, cmd: &CommandInteraction) -> anyhow::Result<bool> {
    if lx.config.is_owner(cmd.user.id.get()) {
        return Ok(true);
    }
    lx.reply_ephemeral(cmd, "Owner only.").await?;
    Ok(false)
}

/// Stable tokio metrics only (no `tokio_unstable`), plus per-worker busy
/// share over a short window: a worker pinned at 100% is spinning.
async fn runtime_report() -> String {
    let metrics = tokio::runtime::Handle::current().metrics();
    let workers = metrics.num_workers();
    let busy = |w: usize| metrics.worker_total_busy_duration(w);
    let before: Vec<Duration> = (0..workers).map(busy).collect();
    tokio::time::sleep(BUSY_SAMPLE).await;
    let shares: Vec<String> = (0..workers)
        .map(|w| {
            let delta = busy(w).saturating_sub(before[w]);
            format!("{:.0}%", 100.0 * delta.as_secs_f64() / BUSY_SAMPLE.as_secs_f64())
        })
        .collect();
    format!(
        "uptime {}\nworkers {workers} · alive tasks {} · global queue {}\nbusy over {}s: {}",
        clock::fmt_age(clock::uptime()),
        metrics.num_alive_tasks(),
        metrics.global_queue_depth(),
        BUSY_SAMPLE.as_secs(),
        shares.join(" "),
    )
}

/// The most recently written `lumina.log*` in the logs dir. Daily rolling
/// names each day's file `lumina.log.YYYY-MM-DD`.
fn newest_log() -> anyhow::Result<PathBuf> {
    let dir = ::config::PathManager::logs_dir()
        .ok_or_else(|| anyhow::anyhow!("no data dir, so no log file"))?;
    std::fs::read_dir(&dir)?
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with(crate::LOG_FILE_PREFIX))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .max_by_key(|(modified, _)| *modified)
        .map(|(_, path)| path)
        .ok_or_else(|| anyhow::anyhow!("no {}* in {}", crate::LOG_FILE_PREFIX, dir.display()))
}

/// Last `n` lines of a file, reading at most `LOG_TAIL_BYTES` from its end.
fn tail_lines(path: &std::path::Path, n: usize) -> anyhow::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let start = file.metadata()?.len().saturating_sub(LOG_TAIL_BYTES);
    file.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    let text = String::from_utf8_lossy(&buf);
    // Starting mid-file, the first line is partial.
    let skip = usize::from(start > 0);
    let lines: Vec<&str> = text.lines().skip(skip).collect();
    Ok(lines[lines.len().saturating_sub(n)..].join("\n"))
}
