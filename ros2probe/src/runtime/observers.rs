use std::collections::{HashMap, VecDeque};

use ros2probe_common::TopicGid;
use std::sync::mpsc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use base64::Engine;

use crate::{
    command::protocol::{
        TopicBwStartRequest, TopicBwStartResponse, TopicBwStatusResponse, TopicBwStopResponse,
        TopicDelayMessage, TopicDelayStartRequest, TopicDelayStartResponse, TopicDelayStats,
        TopicDelayStatusResponse, TopicDelayStopResponse, TopicEchoMessage, TopicEchoStartRequest,
        TopicEchoStartResponse, TopicEchoStatusResponse, TopicEchoStopResponse,
        TopicHzStartRequest, TopicHzStartResponse, TopicHzStatusResponse, TopicHzStopResponse,
    },
    protocols::RtpsDataMessage,
    recorder::RecorderTopicMetadata,
    runtime::RuntimeReply,
};

// ── topic bw ─────────────────────────────────────────────────────────────────

pub(super) struct TopicBwSession {
    topic_name: String,
    window_size: usize,
    samples: VecDeque<BwSamplePoint>,
}

#[derive(Clone, Copy)]
struct BwSamplePoint {
    observed_at: Instant,
    payload_len: usize,
}

struct TopicBwStats {
    bytes_per_second: f64,
    message_count: usize,
    mean_size_bytes: f64,
    min_size_bytes: usize,
    max_size_bytes: usize,
}

pub(super) fn bw_start_session(
    request: TopicBwStartRequest,
    session: &mut Option<TopicBwSession>,
) -> anyhow::Result<TopicBwStartResponse> {
    if session.is_some() {
        bail!("topic bw is already active");
    }
    if !request.topic_name.starts_with('/') {
        bail!(
            "rp topic bw: topic '{}' must start with '/'; use '/chatter' instead of 'chatter'",
            request.topic_name
        );
    }
    if request.window_size == 0 {
        bail!("rp topic bw: --window must be greater than 0");
    }

    *session = Some(TopicBwSession {
        topic_name: request.topic_name.clone(),
        window_size: request.window_size,
        samples: VecDeque::with_capacity(request.window_size.min(1024)),
    });

    Ok(TopicBwStartResponse {
        topic_name: request.topic_name,
    })
}

pub(super) fn bw_build_status_response(
    session: Option<&TopicBwSession>,
) -> TopicBwStatusResponse {
    let Some(session) = session else {
        return TopicBwStatusResponse {
            active: false,
            topic_name: None,
            bytes_per_second: None,
            message_count: 0,
            window_size: 0,
            mean_size_bytes: None,
            min_size_bytes: None,
            max_size_bytes: None,
            last_message_secs_ago: None,
        };
    };

    let stats = session.bw_stats();
    TopicBwStatusResponse {
        active: true,
        topic_name: Some(session.topic_name.clone()),
        bytes_per_second: stats.as_ref().map(|s| s.bytes_per_second),
        message_count: stats.as_ref().map_or(session.samples.len(), |s| s.message_count),
        window_size: session.window_size,
        mean_size_bytes: stats.as_ref().map(|s| s.mean_size_bytes),
        min_size_bytes: stats.as_ref().map(|s| s.min_size_bytes),
        max_size_bytes: stats.as_ref().map(|s| s.max_size_bytes),
        last_message_secs_ago: session.samples.back().map(|s| s.observed_at.elapsed().as_secs_f64()),
    }
}

pub(super) fn bw_stop_session(
    session: &mut Option<TopicBwSession>,
) -> anyhow::Result<TopicBwStopResponse> {
    let Some(session) = session.take() else {
        return Ok(TopicBwStopResponse { stopped: false, topic_name: None });
    };
    Ok(TopicBwStopResponse { stopped: true, topic_name: Some(session.topic_name) })
}

pub(super) fn bw_observe_message(
    session: Option<&mut TopicBwSession>,
    message: &RtpsDataMessage,
    metadata: &RecorderTopicMetadata,
) {
    let Some(session) = session else { return };
    if metadata.topic_name != session.topic_name {
        return;
    }
    session.samples.push_back(BwSamplePoint {
        observed_at: Instant::now(),
        payload_len: message.payload.len(),
    });
    while session.samples.len() > session.window_size {
        session.samples.pop_front();
    }
}

impl TopicBwSession {
    pub(super) fn topic_name(&self) -> &str {
        &self.topic_name
    }

    fn bw_stats(&self) -> Option<TopicBwStats> {
        let message_count = self.samples.len();
        if message_count == 0 {
            return None;
        }
        let mut total_bytes = 0usize;
        let mut min_size_bytes = usize::MAX;
        let mut max_size_bytes = 0usize;
        for sample in &self.samples {
            total_bytes += sample.payload_len;
            if sample.payload_len < min_size_bytes {
                min_size_bytes = sample.payload_len;
            }
            if sample.payload_len > max_size_bytes {
                max_size_bytes = sample.payload_len;
            }
        }
        let mean_size_bytes = total_bytes as f64 / message_count as f64;
        let bytes_per_second =
            if let (Some(first), Some(last)) = (self.samples.front(), self.samples.back()) {
                let elapsed =
                    last.observed_at.saturating_duration_since(first.observed_at).as_secs_f64();
                if elapsed > f64::EPSILON { total_bytes as f64 / elapsed } else { 0.0 }
            } else {
                0.0
            };
        Some(TopicBwStats { bytes_per_second, message_count, mean_size_bytes, min_size_bytes, max_size_bytes })
    }
}

// ── topic hz ─────────────────────────────────────────────────────────────────

pub(super) struct TopicHzSession {
    topic_name: String,
    window_size: usize,
    last_message_at: Option<Instant>,
    periods: VecDeque<f64>,
}

struct TopicHzStats {
    average_rate_hz: f64,
    min_period_seconds: f64,
    max_period_seconds: f64,
    std_dev_seconds: f64,
    window: usize,
}

pub(super) fn hz_start_session(
    request: TopicHzStartRequest,
    session: &mut Option<TopicHzSession>,
) -> anyhow::Result<TopicHzStartResponse> {
    if session.is_some() {
        bail!("topic hz is already active");
    }
    if !request.topic_name.starts_with('/') {
        bail!(
            "rp topic hz: topic '{}' must start with '/'; use '/chatter' instead of 'chatter'",
            request.topic_name
        );
    }
    if request.window_size == 0 {
        bail!("rp topic hz: --window must be greater than 0");
    }

    *session = Some(TopicHzSession {
        topic_name: request.topic_name.clone(),
        window_size: request.window_size,
        last_message_at: None,
        periods: VecDeque::with_capacity(request.window_size.min(1024)),
    });

    Ok(TopicHzStartResponse { topic_name: request.topic_name })
}

pub(super) fn hz_build_status_response(
    session: Option<&mut TopicHzSession>,
) -> TopicHzStatusResponse {
    let Some(session) = session else {
        return TopicHzStatusResponse {
            active: false,
            topic_name: None,
            average_rate_hz: None,
            min_period_seconds: None,
            max_period_seconds: None,
            std_dev_seconds: None,
            last_message_secs_ago: None,
            window: 0,
        };
    };

    let stats = session.hz_stats();
    TopicHzStatusResponse {
        active: true,
        topic_name: Some(session.topic_name.clone()),
        average_rate_hz: stats.as_ref().map(|s| s.average_rate_hz),
        min_period_seconds: stats.as_ref().map(|s| s.min_period_seconds),
        max_period_seconds: stats.as_ref().map(|s| s.max_period_seconds),
        std_dev_seconds: stats.as_ref().map(|s| s.std_dev_seconds),
        window: stats.map_or(0, |s| s.window),
        last_message_secs_ago: session.last_message_at.map(|t| t.elapsed().as_secs_f64()),
    }
}

pub(super) fn hz_stop_session(
    session: &mut Option<TopicHzSession>,
) -> anyhow::Result<TopicHzStopResponse> {
    let Some(session) = session.take() else {
        return Ok(TopicHzStopResponse { stopped: false, topic_name: None });
    };
    Ok(TopicHzStopResponse { stopped: true, topic_name: Some(session.topic_name) })
}

pub(super) fn hz_observe_message(
    session: Option<&mut TopicHzSession>,
    _message: &RtpsDataMessage,
    metadata: &RecorderTopicMetadata,
) {
    let Some(session) = session else { return };
    if metadata.topic_name != session.topic_name {
        return;
    }
    let now = Instant::now();
    if let Some(previous) = session.last_message_at.replace(now) {
        let period = now.saturating_duration_since(previous).as_secs_f64();
        session.periods.push_back(period);
        while session.periods.len() > session.window_size {
            session.periods.pop_front();
        }
    }
}

impl TopicHzSession {
    pub(super) fn topic_name(&self) -> &str {
        &self.topic_name
    }

    fn hz_stats(&self) -> Option<TopicHzStats> {
        if self.periods.is_empty() {
            return None;
        }
        let window = self.periods.len();
        let sum = self.periods.iter().sum::<f64>();
        if sum <= f64::EPSILON {
            return None;
        }
        let mean_period = sum / window as f64;
        let variance = self
            .periods
            .iter()
            .map(|p| { let d = p - mean_period; d * d })
            .sum::<f64>()
            / window as f64;
        let min_period_seconds = self.periods.iter().copied().fold(f64::INFINITY, f64::min);
        let max_period_seconds = self.periods.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        Some(TopicHzStats {
            average_rate_hz: window as f64 / sum,
            min_period_seconds,
            max_period_seconds,
            std_dev_seconds: variance.sqrt(),
            window,
        })
    }
}

// ── topic echo ────────────────────────────────────────────────────────────────

const MAX_ECHO_PENDING: usize = 32;

pub(super) struct TopicEchoSession {
    topic_name: String,
    pending_messages: VecDeque<TopicEchoMessage>,
    waiting_reply: Option<mpsc::Sender<RuntimeReply>>,
    writer_seq_nums: HashMap<TopicGid, i64>,
}

pub(super) fn echo_start_session(
    request: TopicEchoStartRequest,
    session: &mut Option<TopicEchoSession>,
) -> anyhow::Result<TopicEchoStartResponse> {
    if session.is_some() {
        bail!("topic echo is already active");
    }
    if !request.topic_name.starts_with('/') {
        bail!(
            "rp topic echo: topic '{}' must start with '/'; use '/chatter' instead of 'chatter'",
            request.topic_name
        );
    }
    *session = Some(TopicEchoSession {
        topic_name: request.topic_name.clone(),
        pending_messages: VecDeque::with_capacity(MAX_ECHO_PENDING),
        waiting_reply: None,
        writer_seq_nums: HashMap::new(),
    });
    Ok(TopicEchoStartResponse { topic_name: request.topic_name })
}

pub(super) fn echo_build_status_response(
    session: Option<&mut TopicEchoSession>,
) -> TopicEchoStatusResponse {
    let Some(session) = session else {
        return TopicEchoStatusResponse { active: false, topic_name: None, messages: Vec::new() };
    };
    let messages = session.pending_messages.drain(..).collect::<Vec<_>>();
    TopicEchoStatusResponse { active: true, topic_name: Some(session.topic_name.clone()), messages }
}

pub(super) fn echo_reply_or_defer_status(
    session: Option<&mut TopicEchoSession>,
    reply: mpsc::Sender<RuntimeReply>,
) {
    let Some(session) = session else {
        let _ = reply.send(RuntimeReply::TopicEchoStatus(TopicEchoStatusResponse {
            active: false,
            topic_name: None,
            messages: Vec::new(),
        }));
        return;
    };

    if !session.pending_messages.is_empty() {
        let _ = reply.send(RuntimeReply::TopicEchoStatus(echo_build_status_response(Some(session))));
        return;
    }

    if let Some(previous_reply) = session.waiting_reply.replace(reply) {
        let _ = previous_reply.send(RuntimeReply::TopicEchoStatus(TopicEchoStatusResponse {
            active: true,
            topic_name: Some(session.topic_name.clone()),
            messages: Vec::new(),
        }));
    }
}

pub(super) fn echo_stop_session(
    session: &mut Option<TopicEchoSession>,
) -> anyhow::Result<TopicEchoStopResponse> {
    let Some(session) = session.take() else {
        return Ok(TopicEchoStopResponse { stopped: false, topic_name: None });
    };
    if let Some(reply) = session.waiting_reply {
        let _ = reply.send(RuntimeReply::TopicEchoStatus(TopicEchoStatusResponse {
            active: false,
            topic_name: None,
            messages: Vec::new(),
        }));
    }
    Ok(TopicEchoStopResponse { stopped: true, topic_name: Some(session.topic_name) })
}

pub(super) fn echo_observe_message(
    session: Option<&mut TopicEchoSession>,
    message: &RtpsDataMessage,
    metadata: &RecorderTopicMetadata,
) {
    let Some(session) = session else { return };
    if metadata.topic_name != session.topic_name {
        return;
    }

    let seq = rtps_seq_num(&message.sequence_number);
    let lost_before = match session.writer_seq_nums.get(&message.writer_gid) {
        Some(&last) if seq > last + 1 => usize::try_from(seq - last - 1).unwrap_or(usize::MAX),
        _ => 0,
    };
    session.writer_seq_nums.insert(message.writer_gid, seq);

    if session.pending_messages.len() == MAX_ECHO_PENDING {
        session.pending_messages.pop_front();
    }
    session.pending_messages.push_back(TopicEchoMessage {
        type_name: metadata.type_name.clone(),
        payload_base64: base64::engine::general_purpose::STANDARD.encode(&message.payload),
        lost_before,
    });
    if let Some(reply) = session.waiting_reply.take() {
        let _ = reply.send(RuntimeReply::TopicEchoStatus(echo_build_status_response(Some(session))));
    }
}

fn rtps_seq_num(bytes: &[u8; 8]) -> i64 {
    // RTPS SequenceNumber_t: int32 high + uint32 low (little-endian submessage body)
    let high = i32::from_le_bytes(bytes[0..4].try_into().unwrap()) as i64;
    let low = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as i64;
    (high << 32) | low
}

impl TopicEchoSession {
    pub(super) fn topic_name(&self) -> &str {
        &self.topic_name
    }
}

// ── topic delay ───────────────────────────────────────────────────────────────

const MAX_DELAY_PENDING: usize = 128;

pub(super) struct TopicDelaySession {
    topic_name: String,
    delays: VecDeque<f64>,
    window_size: usize,
    pending_messages: VecDeque<TopicDelayMessage>,
}

pub(super) fn delay_start_session(
    request: TopicDelayStartRequest,
    session: &mut Option<TopicDelaySession>,
) -> anyhow::Result<TopicDelayStartResponse> {
    if session.is_some() {
        bail!("topic delay is already active");
    }
    if !request.topic_name.starts_with('/') {
        bail!(
            "rp topic delay: topic '{}' must start with '/'; use '/chatter' instead of 'chatter'",
            request.topic_name
        );
    }
    if request.window_size == 0 {
        bail!("rp topic delay: --window must be greater than 0");
    }
    *session = Some(TopicDelaySession {
        topic_name: request.topic_name.clone(),
        delays: VecDeque::with_capacity(request.window_size.min(1024)),
        window_size: request.window_size,
        pending_messages: VecDeque::with_capacity(MAX_DELAY_PENDING),
    });
    Ok(TopicDelayStartResponse { topic_name: request.topic_name })
}

pub(super) fn delay_build_status_response(
    session: Option<&mut TopicDelaySession>,
) -> TopicDelayStatusResponse {
    let Some(session) = session else {
        return TopicDelayStatusResponse { active: false, topic_name: None, messages: Vec::new() };
    };
    let messages = session.pending_messages.drain(..).collect::<Vec<_>>();
    TopicDelayStatusResponse { active: true, topic_name: Some(session.topic_name.clone()), messages }
}

pub(super) fn delay_stop_session(
    session: &mut Option<TopicDelaySession>,
) -> anyhow::Result<TopicDelayStopResponse> {
    let Some(session) = session.take() else {
        return Ok(TopicDelayStopResponse { stopped: false, topic_name: None });
    };
    Ok(TopicDelayStopResponse { stopped: true, topic_name: Some(session.topic_name) })
}

pub(super) fn delay_observe_message(
    session: Option<&mut TopicDelaySession>,
    message: &RtpsDataMessage,
    metadata: &RecorderTopicMetadata,
) {
    let Some(session) = session else { return };
    if metadata.topic_name != session.topic_name {
        return;
    }
    if session.pending_messages.len() == MAX_DELAY_PENDING {
        session.pending_messages.pop_front();
    }
    let Ok(received_at_nanos) = system_time_to_nanos(message.socket_timestamp) else {
        return;
    };
    session.pending_messages.push_back(TopicDelayMessage {
        type_name: metadata.type_name.clone(),
        payload_base64: base64::engine::general_purpose::STANDARD.encode(&message.payload),
        received_at_nanos,
    });

    if let Some(stamp_nanos) = parse_cdr_stamp_nanos(&message.payload) {
        let delay_ms = if received_at_nanos >= stamp_nanos {
            (received_at_nanos - stamp_nanos) as f64 / 1_000_000.0
        } else {
            // stamp slightly in the future (clock jitter) — treat as near-zero delay
            (stamp_nanos - received_at_nanos) as f64 / 1_000_000.0
        };
        if delay_ms < 60_000.0 {
            session.delays.push_back(delay_ms);
            while session.delays.len() > session.window_size {
                session.delays.pop_front();
            }
        }
    }
}

impl TopicDelaySession {
    pub(super) fn topic_name(&self) -> &str {
        &self.topic_name
    }
}

pub(super) fn delay_build_stats_response(
    session: Option<&TopicDelaySession>,
) -> Option<TopicDelayStats> {
    let session = session?;
    if session.delays.is_empty() {
        return None;
    }
    let window = session.delays.len();
    let sum: f64 = session.delays.iter().sum();
    let mean = sum / window as f64;
    let variance = session
        .delays
        .iter()
        .map(|d| {
            let e = d - mean;
            e * e
        })
        .sum::<f64>()
        / window as f64;
    let min = session.delays.iter().copied().fold(f64::INFINITY, f64::min);
    let max = session.delays.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    Some(TopicDelayStats { avg_ms: mean, min_ms: min, max_ms: max, std_dev_ms: variance.sqrt(), window })
}

fn system_time_to_nanos(timestamp: SystemTime) -> anyhow::Result<u64> {
    let duration =
        timestamp.duration_since(UNIX_EPOCH).context("timestamp earlier than UNIX_EPOCH")?;
    u64::try_from(duration.as_nanos()).context("timestamp does not fit in u64 nanoseconds")
}

fn parse_cdr_stamp_nanos(payload: &[u8]) -> Option<u64> {
    // CDR encapsulation header: [0x00, 0x01|0x00, 0x00, 0x00] (CDR_LE or CDR_BE)
    // stamp.sec  at payload[4..8], stamp.nanosec at payload[8..12]
    if payload.len() < 12 {
        return None;
    }
    let enc = u16::from_be_bytes([payload[0], payload[1]]);
    let (little_endian, sec_offset) = match enc {
        0x0001 => (true, 4usize),   // CDR_LE with encapsulation header
        0x0000 => (false, 4usize),  // CDR_BE with encapsulation header
        _ => {
            // Encapsulation header absent or unknown — try assuming LE at offset 0
            (true, 0usize)
        }
    };
    if payload.len() < sec_offset + 8 {
        return None;
    }
    let sec = if little_endian {
        u32::from_le_bytes(payload[sec_offset..sec_offset + 4].try_into().ok()?)
    } else {
        u32::from_be_bytes(payload[sec_offset..sec_offset + 4].try_into().ok()?)
    };
    let nanosec = if little_endian {
        u32::from_le_bytes(payload[sec_offset + 4..sec_offset + 8].try_into().ok()?)
    } else {
        u32::from_be_bytes(payload[sec_offset + 4..sec_offset + 8].try_into().ok()?)
    };
    if nanosec >= 1_000_000_000 {
        return None;
    }
    Some(sec as u64 * 1_000_000_000 + nanosec as u64)
}
