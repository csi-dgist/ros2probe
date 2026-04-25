use std::path::PathBuf;

use anyhow::bail;
use chrono::Local;

use crate::{
    command::protocol::{
        BagRecordRequest, BagRecordResponse, BagSessionInfo, BagSetPausedResponse,
        BagStatusResponse, BagStopResponse, CompressionFormat,
    },
    recorder::{RecorderHandle, RecorderTopicGidMap},
};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum CompressionConfig {
    #[default]
    None,
    Zstd,
    Lz4,
}

#[derive(Clone, Debug)]
pub(super) struct BagRecordOptions {
    pub topics: Vec<String>,
    pub output: PathBuf,
    pub compression: CompressionConfig,
    pub no_discovery: bool,
    pub start_paused: bool,
}

/// Lightweight state that stays on the runtime thread while the actual MCAP
/// writer runs in the recorder actor. Holds only what `sync_topic_filter`,
/// status replies, and the data-dispatch decision need.
pub(super) struct RecordingSession {
    pub output: PathBuf,
    pub topics: Vec<String>,
    pub compression: CompressionConfig,
    pub no_discovery: bool,
    pub paused: bool,
}

pub(super) fn normalize_topics(topics: &[String], source: &str) -> anyhow::Result<Vec<String>> {
    if topics.iter().any(|topic| topic.eq_ignore_ascii_case("all")) {
        if topics.len() == 1 {
            return Ok(Vec::new());
        }
        bail!(
            "{source}: topics must be either ['all'] or a list of absolute topic names like ['/chatter']"
        );
    }

    let mut normalized = Vec::with_capacity(topics.len());
    for topic in topics {
        if !topic.starts_with('/') {
            bail!(
                "{source}: explicit topic '{topic}' must start with '/'; use ['/chatter'] instead of ['chatter']"
            );
        }
        normalized.push(topic.clone());
    }

    Ok(normalized)
}

pub(super) fn handle_bag_record_command(
    request: BagRecordRequest,
    recording_session: &mut Option<RecordingSession>,
    gid_map: &mut RecorderTopicGidMap,
    discovery_table: &crate::discovery::DiscoveryTable,
    recorder_handle: &RecorderHandle,
) -> anyhow::Result<BagRecordResponse> {
    if recording_session.is_some() {
        bail!("bag recording is already active");
    }

    let options = bag_record_options_from_request(request)?;
    let response = BagRecordResponse {
        output: options.output.display().to_string(),
        paused: options.start_paused,
        topics: options.topics.clone(),
        all_topics: options.topics.is_empty(),
        no_discovery: options.no_discovery,
        compression_format: compression_format_from_config(options.compression),
    };

    *recording_session = Some(start_recording(
        options,
        gid_map,
        discovery_table,
        recorder_handle,
    )?);
    Ok(response)
}

pub(super) fn start_recording(
    options: BagRecordOptions,
    gid_map: &mut RecorderTopicGidMap,
    discovery_table: &crate::discovery::DiscoveryTable,
    recorder_handle: &RecorderHandle,
) -> anyhow::Result<RecordingSession> {
    gid_map.configure(&options.topics);
    gid_map.rebuild_from_table(discovery_table)?;

    recorder_handle.start(options.output.clone(), options.compression)?;
    Ok(RecordingSession {
        output: options.output,
        topics: options.topics,
        compression: options.compression,
        no_discovery: options.no_discovery,
        paused: options.start_paused,
    })
}

pub(super) fn stop_recording(
    recording_session: &mut Option<RecordingSession>,
    gid_map: &mut RecorderTopicGidMap,
    recorder_handle: &RecorderHandle,
) -> anyhow::Result<BagStopResponse> {
    if recording_session.take().is_none() {
        return Ok(BagStopResponse {
            stopped: false,
            output: None,
        });
    };

    gid_map.clear()?;
    // Invariant: if the shadow was `Some`, the actor also had an active
    // recording (both are set/cleared together in `start_recording` /
    // `stop_recording` / shutdown), so `stop()` returns `Some(path)` here.
    // We still use `.map()` defensively rather than `.expect()` because a
    // future actor refactor might legitimately break that coupling.
    let output = recorder_handle.stop()?;
    Ok(BagStopResponse {
        stopped: true,
        output: output.map(|p| p.display().to_string()),
    })
}

pub(super) fn set_paused(
    recording_session: &mut Option<RecordingSession>,
    paused: bool,
) -> anyhow::Result<BagSetPausedResponse> {
    let Some(session) = recording_session.as_mut() else {
        return Ok(BagSetPausedResponse {
            active: false,
            paused,
        });
    };

    session.paused = paused;
    Ok(BagSetPausedResponse {
        active: true,
        paused: session.paused,
    })
}

pub(super) fn build_bag_status_response(
    recording_session: Option<&RecordingSession>,
) -> BagStatusResponse {
    BagStatusResponse {
        active: recording_session.is_some(),
        session: recording_session.map(|session| BagSessionInfo {
            output: session.output.display().to_string(),
            paused: session.paused,
            topics: session.topics.clone(),
            all_topics: session.topics.is_empty(),
            no_discovery: session.no_discovery,
            compression_format: compression_format_from_config(session.compression),
        }),
    }
}

/// Forward a matching data message to the recorder actor. Runs on the main
/// runtime thread; keep the work here trivial (no MCAP IO) — the actor is
/// responsible for any blocking write, compression, and fsync.
pub(super) fn record_message(
    session: &RecordingSession,
    recorder_handle: &RecorderHandle,
    message: &crate::protocols::RtpsDataMessage,
    metadata: &crate::recorder::RecorderTopicMetadata,
) {
    if session.paused {
        return;
    }
    // `RtpsDataMessage::payload` is an `Arc<[u8]>`, so cloning the message
    // is just a refcount bump plus a handful of small field copies — no
    // payload allocation regardless of message size. Metadata (strings +
    // GID) is small, so cloning that is also cheap.
    let _queued = recorder_handle.try_record(message.clone(), metadata.clone());
    // Drops are tracked by the handle's counter; log at a low rate elsewhere
    // so we don't spam the log on overload.
}

fn bag_record_options_from_request(request: BagRecordRequest) -> anyhow::Result<BagRecordOptions> {
    if request.all && !request.topics.is_empty() {
        bail!("--all cannot be combined with explicit topic names");
    }

    let topics = normalize_topics(&request.topics, "rp bag record")?;
    if !request.all && topics.is_empty() {
        bail!("pass one or more topics, or use --all");
    }

    Ok(BagRecordOptions {
        topics: if request.all { Vec::new() } else { topics },
        output: request
            .output
            .map(PathBuf::from)
            .unwrap_or_else(default_bag_output_path),
        compression: request
            .compression_format
            .map(compression_config_from_format)
            .unwrap_or_default(),
        no_discovery: request.no_discovery,
        start_paused: request.start_paused,
    })
}

fn compression_config_from_format(format: CompressionFormat) -> CompressionConfig {
    match format {
        CompressionFormat::None => CompressionConfig::None,
        CompressionFormat::Zstd => CompressionConfig::Zstd,
        CompressionFormat::Lz4 => CompressionConfig::Lz4,
    }
}

fn compression_format_from_config(config: CompressionConfig) -> CompressionFormat {
    match config {
        CompressionConfig::None => CompressionFormat::None,
        CompressionConfig::Zstd => CompressionFormat::Zstd,
        CompressionConfig::Lz4 => CompressionFormat::Lz4,
    }
}

fn default_bag_output_path() -> PathBuf {
    let base = format!("rosbag2_{}", Local::now().format("%Y_%m_%d-%H_%M_%S"));
    PathBuf::from(&base).join(format!("{base}.mcap"))
}
