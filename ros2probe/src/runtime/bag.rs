use std::{
    collections::HashMap,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::bail;
use chrono::Local;
use ros2probe_common::TopicGid;

use crate::{
    command::protocol::{
        BagLostMessages, BagRecordRequest, BagRecordResponse, BagSessionInfo, BagSetPausedResponse,
        BagStatusResponse, BagStopResponse, CompressionFormat,
    },
    recorder::{RecordMessage, RecorderHandle, RecorderTopicGidMap},
};

const ROS_DISCOVERY_INFO_TOPIC: &str = "/ros_discovery_info";

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
    channels: HashMap<TopicGid, u16>,
    channel_sequences: HashMap<u16, u32>,
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
        channels: HashMap::new(),
        channel_sequences: HashMap::new(),
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
            lost_messages: Vec::new(),
        });
    };

    gid_map.clear()?;
    // Invariant: if the shadow was `Some`, the actor also had an active
    // recording (both are set/cleared together in `start_recording` /
    // `stop_recording` / shutdown), so `stop()` returns `Some(path)` here.
    // We still use `.map()` defensively rather than `.expect()` because a
    // future actor refactor might legitimately break that coupling.
    let (output, lost_messages) = recorder_handle.stop()?;
    Ok(BagStopResponse {
        stopped: true,
        output: output.map(|p| p.display().to_string()),
        lost_messages: lost_messages
            .into_iter()
            .map(|(topic_name, count)| BagLostMessages { topic_name, count })
            .collect(),
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
    session: &mut RecordingSession,
    recorder_handle: &RecorderHandle,
    message: &crate::protocols::RtpsDataMessage,
    topic_gid: TopicGid,
    metadata: &crate::recorder::RecorderTopicMetadata,
) {
    if session.paused {
        return;
    }
    if should_skip_discovery_topic(session, &metadata.topic_name) {
        return;
    }

    if !session.channels.contains_key(&topic_gid) {
        let type_name = metadata.type_name.as_deref().unwrap_or("unknown/Unknown");
        match recorder_handle.ensure_channel(&metadata.topic_name, type_name, "") {
            Ok(channel_id) => {
                session.channels.insert(topic_gid, channel_id);
            }
            Err(err) => {
                log::warn!(
                    "failed to create MCAP channel for {}: {err:#}",
                    metadata.topic_name
                );
                return;
            }
        }
    }

    let Some(channel_id) = session.channels.get(&topic_gid).copied() else {
        return;
    };
    let Ok(timestamp) = system_time_to_nanos(message.captured_at) else {
        log::warn!("failed to convert message timestamp for {}", metadata.topic_name);
        return;
    };
    let sequence = session.channel_sequences.entry(channel_id).or_insert(0);
    let record = RecordMessage {
        channel_id,
        sequence: *sequence,
        log_time: timestamp,
        publish_time: timestamp,
        payload: message.payload.clone(),
    };
    if recorder_handle.try_record(record, &metadata.topic_name) {
        *sequence = sequence.saturating_add(1);
    }
    // Drops are tracked by the handle's counter; log at a low rate elsewhere
    // so we don't spam the log on overload.
}

fn system_time_to_nanos(timestamp: SystemTime) -> anyhow::Result<u64> {
    let duration = timestamp.duration_since(UNIX_EPOCH)?;
    Ok(u64::try_from(duration.as_nanos())?)
}

fn should_skip_discovery_topic(session: &RecordingSession, topic_name: &str) -> bool {
    if topic_name != ROS_DISCOVERY_INFO_TOPIC {
        return false;
    }

    session.no_discovery
        || !session
            .topics
            .iter()
            .any(|topic| topic == ROS_DISCOVERY_INFO_TOPIC)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn session(topics: &[&str], no_discovery: bool) -> RecordingSession {
        RecordingSession {
            output: PathBuf::from("test.mcap"),
            topics: topics.iter().map(|topic| topic.to_string()).collect(),
            compression: CompressionConfig::None,
            no_discovery,
            paused: false,
            channels: HashMap::new(),
            channel_sequences: HashMap::new(),
        }
    }

    #[test]
    fn skips_discovery_metadata_unless_explicitly_requested() {
        assert!(should_skip_discovery_topic(
            &session(&["/stress"], false),
            ROS_DISCOVERY_INFO_TOPIC
        ));
        assert!(should_skip_discovery_topic(
            &session(&[], false),
            ROS_DISCOVERY_INFO_TOPIC
        ));
        assert!(!should_skip_discovery_topic(
            &session(&[ROS_DISCOVERY_INFO_TOPIC], false),
            ROS_DISCOVERY_INFO_TOPIC
        ));
    }

    #[test]
    fn no_discovery_skips_even_when_explicitly_requested() {
        assert!(should_skip_discovery_topic(
            &session(&[ROS_DISCOVERY_INFO_TOPIC], true),
            ROS_DISCOVERY_INFO_TOPIC
        ));
    }

    #[test]
    fn user_topics_are_not_skipped_by_discovery_policy() {
        assert!(!should_skip_discovery_topic(
            &session(&["/stress"], false),
            "/stress"
        ));
    }
}
