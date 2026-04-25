use std::collections::HashMap;
use std::sync::mpsc;

use log::warn;

use crate::{
    command::protocol::{
        BagRecordRequest, BagRecordResponse, BagSetPausedRequest, BagSetPausedResponse,
        BagStatusResponse, BagStopResponse, TopicBwStartRequest, TopicBwStartResponse,
        TopicBwStatusResponse, TopicBwStopResponse, TopicDelayStartRequest,
        TopicDelayStartResponse, TopicDelayStatusResponse, TopicDelayStopResponse,
        TopicEchoStartRequest, TopicEchoStartResponse, TopicEchoStatusResponse,
        TopicEchoStopResponse, TopicHzBwStatusResponse, TopicHzStartRequest, TopicHzStartResponse,
        TopicHzStatusResponse, TopicHzStopResponse,
    },
    discovery::{DiscoveryTable, EndpointEntry},
    recorder::{RecorderHandle, RecorderTopicGidMap},
    shadow::sub::{endpoint_needs_shadow, ShadowSubscriber},
};

use super::{
    bag,
    observers::{
        self, TopicBwSession, TopicDelaySession, TopicEchoSession, TopicHzSession,
    },
    sync_topic_filter, RecordingSession,
};

#[derive(Debug)]
pub enum RuntimeCommand {
    BagRecord {
        request: BagRecordRequest,
        reply: mpsc::Sender<RuntimeReply>,
    },
    BagStatus {
        reply: mpsc::Sender<RuntimeReply>,
    },
    BagSetPaused {
        request: BagSetPausedRequest,
        reply: mpsc::Sender<RuntimeReply>,
    },
    BagStop {
        reply: mpsc::Sender<RuntimeReply>,
    },
    TopicBwStart {
        request: TopicBwStartRequest,
        reply: mpsc::Sender<RuntimeReply>,
    },
    TopicBwStatus {
        reply: mpsc::Sender<RuntimeReply>,
    },
    TopicBwStop {
        reply: mpsc::Sender<RuntimeReply>,
    },
    TopicDelayStart {
        request: TopicDelayStartRequest,
        reply: mpsc::Sender<RuntimeReply>,
    },
    TopicDelayStatus {
        reply: mpsc::Sender<RuntimeReply>,
    },
    TopicDelayStop {
        reply: mpsc::Sender<RuntimeReply>,
    },
    TopicEchoStart {
        request: TopicEchoStartRequest,
        reply: mpsc::Sender<RuntimeReply>,
    },
    TopicEchoStatus {
        reply: mpsc::Sender<RuntimeReply>,
    },
    TopicEchoStop {
        reply: mpsc::Sender<RuntimeReply>,
    },
    TopicHzStart {
        request: TopicHzStartRequest,
        reply: mpsc::Sender<RuntimeReply>,
    },
    TopicHzBwStatus {
        reply: mpsc::Sender<RuntimeReply>,
    },
    TopicHzStatus {
        reply: mpsc::Sender<RuntimeReply>,
    },
    TopicHzStop {
        reply: mpsc::Sender<RuntimeReply>,
    },
}

#[derive(Debug)]
pub enum RuntimeReply {
    BagStarted(BagRecordResponse),
    BagStatus(BagStatusResponse),
    BagSetPaused(BagSetPausedResponse),
    BagStopped(BagStopResponse),
    TopicBwStarted(TopicBwStartResponse),
    TopicBwStatus(TopicBwStatusResponse),
    TopicBwStopped(TopicBwStopResponse),
    TopicDelayStarted(TopicDelayStartResponse),
    TopicDelayStatus(TopicDelayStatusResponse),
    TopicDelayStopped(TopicDelayStopResponse),
    TopicEchoStarted(TopicEchoStartResponse),
    TopicEchoStatus(TopicEchoStatusResponse),
    TopicEchoStopped(TopicEchoStopResponse),
    TopicHzStarted(TopicHzStartResponse),
    TopicHzBwStatus(TopicHzBwStatusResponse),
    TopicHzStatus(TopicHzStatusResponse),
    TopicHzStopped(TopicHzStopResponse),
    Error(String),
}

pub(super) fn handle_runtime_commands(
    runtime_command_rx: &mpsc::Receiver<RuntimeCommand>,
    recording_session: &mut Option<RecordingSession>,
    topic_bw_session: &mut Option<TopicBwSession>,
    topic_delay_session: &mut Option<TopicDelaySession>,
    topic_echo_session: &mut Option<TopicEchoSession>,
    topic_hz_session: &mut Option<TopicHzSession>,
    gid_map: &mut RecorderTopicGidMap,
    shadow_subs: &mut HashMap<String, ShadowSubscriber>,
    discovery_table: &DiscoveryTable,
    recorder_handle: &RecorderHandle,
) -> anyhow::Result<()> {
    while let Ok(command) = runtime_command_rx.try_recv() {
        match command {
            RuntimeCommand::BagRecord { request, reply } => {
                let result = bag::handle_bag_record_command(
                    request,
                    recording_session,
                    gid_map,
                    discovery_table,
                    recorder_handle,
                );
                if result.is_ok() {
                    sync_filter_and_shadow(gid_map, shadow_subs, discovery_table, recording_session.as_ref(), topic_bw_session.as_ref(), topic_delay_session.as_ref(), topic_echo_session.as_ref(), topic_hz_session.as_ref());
                }
                let _ = reply.send(match result {
                    Ok(response) => RuntimeReply::BagStarted(response),
                    Err(err) => RuntimeReply::Error(err.to_string()),
                });
            }
            RuntimeCommand::BagStatus { reply } => {
                let _ = reply.send(RuntimeReply::BagStatus(bag::build_bag_status_response(recording_session.as_ref())));
            }
            RuntimeCommand::BagSetPaused { request, reply } => {
                let result = bag::set_paused(recording_session, request.paused);
                let _ = reply.send(match result {
                    Ok(response) => RuntimeReply::BagSetPaused(response),
                    Err(err) => RuntimeReply::Error(err.to_string()),
                });
            }
            RuntimeCommand::BagStop { reply } => {
                let result = bag::stop_recording(recording_session, gid_map, recorder_handle);
                if result.is_ok() {
                    sync_filter_and_shadow(gid_map, shadow_subs, discovery_table, recording_session.as_ref(), topic_bw_session.as_ref(), topic_delay_session.as_ref(), topic_echo_session.as_ref(), topic_hz_session.as_ref());
                }
                let _ = reply.send(match result {
                    Ok(response) => RuntimeReply::BagStopped(response),
                    Err(err) => RuntimeReply::Error(err.to_string()),
                });
            }
            RuntimeCommand::TopicBwStart { request, reply } => {
                let result = observers::bw_start_session(request, topic_bw_session);
                if result.is_ok() {
                    sync_filter_and_shadow(gid_map, shadow_subs, discovery_table, recording_session.as_ref(), topic_bw_session.as_ref(), topic_delay_session.as_ref(), topic_echo_session.as_ref(), topic_hz_session.as_ref());
                }
                let _ = reply.send(match result {
                    Ok(response) => RuntimeReply::TopicBwStarted(response),
                    Err(err) => RuntimeReply::Error(err.to_string()),
                });
            }
            RuntimeCommand::TopicBwStatus { reply } => {
                let _ = reply.send(RuntimeReply::TopicBwStatus(
                    observers::bw_build_status_response(topic_bw_session.as_ref()),
                ));
            }
            RuntimeCommand::TopicBwStop { reply } => {
                let response = observers::bw_stop_session(topic_bw_session);
                sync_filter_and_shadow(gid_map, shadow_subs, discovery_table, recording_session.as_ref(), topic_bw_session.as_ref(), topic_delay_session.as_ref(), topic_echo_session.as_ref(), topic_hz_session.as_ref());
                let _ = reply.send(match response {
                    Ok(response) => RuntimeReply::TopicBwStopped(response),
                    Err(err) => RuntimeReply::Error(err.to_string()),
                });
            }
            RuntimeCommand::TopicDelayStart { request, reply } => {
                let result = observers::delay_start_session(request, topic_delay_session);
                if result.is_ok() {
                    sync_filter_and_shadow(gid_map, shadow_subs, discovery_table, recording_session.as_ref(), topic_bw_session.as_ref(), topic_delay_session.as_ref(), topic_echo_session.as_ref(), topic_hz_session.as_ref());
                }
                let _ = reply.send(match result {
                    Ok(response) => RuntimeReply::TopicDelayStarted(response),
                    Err(err) => RuntimeReply::Error(err.to_string()),
                });
            }
            RuntimeCommand::TopicDelayStatus { reply } => {
                let _ = reply.send(RuntimeReply::TopicDelayStatus(
                    observers::delay_build_status_response(topic_delay_session.as_mut()),
                ));
            }
            RuntimeCommand::TopicDelayStop { reply } => {
                let response = observers::delay_stop_session(topic_delay_session);
                sync_filter_and_shadow(gid_map, shadow_subs, discovery_table, recording_session.as_ref(), topic_bw_session.as_ref(), topic_delay_session.as_ref(), topic_echo_session.as_ref(), topic_hz_session.as_ref());
                let _ = reply.send(match response {
                    Ok(response) => RuntimeReply::TopicDelayStopped(response),
                    Err(err) => RuntimeReply::Error(err.to_string()),
                });
            }
            RuntimeCommand::TopicEchoStart { request, reply } => {
                let result = observers::echo_start_session(request, topic_echo_session);
                if result.is_ok() {
                    sync_filter_and_shadow(gid_map, shadow_subs, discovery_table, recording_session.as_ref(), topic_bw_session.as_ref(), topic_delay_session.as_ref(), topic_echo_session.as_ref(), topic_hz_session.as_ref());
                }
                let _ = reply.send(match result {
                    Ok(response) => RuntimeReply::TopicEchoStarted(response),
                    Err(err) => RuntimeReply::Error(err.to_string()),
                });
            }
            RuntimeCommand::TopicEchoStatus { reply } => {
                observers::echo_reply_or_defer_status(topic_echo_session.as_mut(), reply);
            }
            RuntimeCommand::TopicEchoStop { reply } => {
                let response = observers::echo_stop_session(topic_echo_session);
                sync_filter_and_shadow(gid_map, shadow_subs, discovery_table, recording_session.as_ref(), topic_bw_session.as_ref(), topic_delay_session.as_ref(), topic_echo_session.as_ref(), topic_hz_session.as_ref());
                let _ = reply.send(match response {
                    Ok(response) => RuntimeReply::TopicEchoStopped(response),
                    Err(err) => RuntimeReply::Error(err.to_string()),
                });
            }
            RuntimeCommand::TopicHzStart { request, reply } => {
                let result = observers::hz_start_session(request, topic_hz_session);
                if result.is_ok() {
                    sync_filter_and_shadow(gid_map, shadow_subs, discovery_table, recording_session.as_ref(), topic_bw_session.as_ref(), topic_delay_session.as_ref(), topic_echo_session.as_ref(), topic_hz_session.as_ref());
                }
                let _ = reply.send(match result {
                    Ok(response) => RuntimeReply::TopicHzStarted(response),
                    Err(err) => RuntimeReply::Error(err.to_string()),
                });
            }
            RuntimeCommand::TopicHzBwStatus { reply } => {
                let hz = observers::hz_build_status_response(topic_hz_session.as_mut());
                let bw = observers::bw_build_status_response(topic_bw_session.as_ref());
                let delay = observers::delay_build_stats_response(topic_delay_session.as_ref());
                let _ = reply.send(RuntimeReply::TopicHzBwStatus(TopicHzBwStatusResponse { hz, bw, delay }));
            }
            RuntimeCommand::TopicHzStatus { reply } => {
                let _ = reply.send(RuntimeReply::TopicHzStatus(
                    observers::hz_build_status_response(topic_hz_session.as_mut()),
                ));
            }
            RuntimeCommand::TopicHzStop { reply } => {
                let response = observers::hz_stop_session(topic_hz_session);
                sync_filter_and_shadow(gid_map, shadow_subs, discovery_table, recording_session.as_ref(), topic_bw_session.as_ref(), topic_delay_session.as_ref(), topic_echo_session.as_ref(), topic_hz_session.as_ref());
                let _ = reply.send(match response {
                    Ok(response) => RuntimeReply::TopicHzStopped(response),
                    Err(err) => RuntimeReply::Error(err.to_string()),
                });
            }
        }
    }
    Ok(())
}

// ── Shadow subscriber management ─────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn sync_filter_and_shadow(
    gid_map: &mut RecorderTopicGidMap,
    shadow_subs: &mut HashMap<String, ShadowSubscriber>,
    discovery_table: &DiscoveryTable,
    recording_session: Option<&RecordingSession>,
    topic_bw_session: Option<&TopicBwSession>,
    topic_delay_session: Option<&TopicDelaySession>,
    topic_echo_session: Option<&TopicEchoSession>,
    topic_hz_session: Option<&TopicHzSession>,
) {
    let _ = sync_topic_filter(
        gid_map,
        discovery_table,
        recording_session,
        topic_bw_session,
        topic_delay_session,
        topic_echo_session,
        topic_hz_session,
    );
    sync_shadow_subs(
        shadow_subs,
        recording_session,
        topic_bw_session,
        topic_delay_session,
        topic_echo_session,
        topic_hz_session,
        discovery_table,
    );
}

/// Reconciles the shadow subscriber map against the current set of active
/// topics. Spawns a subscriber for any SHM-only topic that is being observed
/// but has no subscriber yet, and drops subscribers for topics that are no
/// longer observed.
///
/// Called from `mod.rs` (discovery sweep) and `command.rs` (Start/Stop
/// commands) so that the map stays current whenever the session set changes or
/// new SHM endpoints are discovered.
#[allow(clippy::too_many_arguments)]
pub(super) fn sync_shadow_subs(
    shadow_subs: &mut HashMap<String, ShadowSubscriber>,
    recording_session: Option<&RecordingSession>,
    topic_bw_session: Option<&TopicBwSession>,
    topic_delay_session: Option<&TopicDelaySession>,
    topic_echo_session: Option<&TopicEchoSession>,
    topic_hz_session: Option<&TopicHzSession>,
    discovery_table: &DiscoveryTable,
) {
    let mut wanted: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let all_topics = recording_session.is_some_and(|s| s.topics.is_empty());

    if all_topics {
        // bag record --all: pick up every SHM-only topic from the discovery table.
        for endpoint in discovery_table.publications().values() {
            if let Some(ros2) = endpoint.topic_name.as_deref().and_then(ShadowSubscriber::ros2_topic) {
                if pub_needs_shadow(endpoint, discovery_table) {
                    wanted.insert(ros2);
                }
            }
        }
    } else {
        let mut candidates: Vec<&str> = Vec::new();
        if let Some(s) = recording_session {
            for t in &s.topics { candidates.push(t); }
        }
        if let Some(s) = topic_bw_session    { candidates.push(s.topic_name()); }
        if let Some(s) = topic_delay_session  { candidates.push(s.topic_name()); }
        if let Some(s) = topic_echo_session   { candidates.push(s.topic_name()); }
        if let Some(s) = topic_hz_session     { candidates.push(s.topic_name()); }

        for topic in candidates {
            if topic_has_shm_publisher(topic, discovery_table) {
                wanted.insert(topic.to_string());
            }
        }
    }

    // Spawn for newly wanted SHM topics.
    for topic in &wanted {
        if !shadow_subs.contains_key(topic) {
            match spawn_shadow_sub(topic, discovery_table) {
                Ok(sub) => { shadow_subs.insert(topic.clone(), sub); }
                Err(e) => warn!("shadow sub: failed to spawn for {topic}: {e:#}"),
            }
        }
    }

    shadow_subs.retain(|topic, _| wanted.contains(topic));
}

fn topic_has_shm_publisher(ros2_topic: &str, discovery_table: &DiscoveryTable) -> bool {
    discovery_table.publications().values().any(|endpoint| {
        endpoint.topic_name.as_deref()
            .and_then(ShadowSubscriber::ros2_topic)
            .as_deref() == Some(ros2_topic)
            && pub_needs_shadow(endpoint, discovery_table)
    })
}

fn pub_needs_shadow(endpoint: &EndpointEntry, discovery_table: &DiscoveryTable) -> bool {
    let participant_locators = endpoint
        .participant_gid
        .and_then(|pgid| discovery_table.participant(&pgid))
        .map(|p| p.default_unicast_locators.as_slice())
        .unwrap_or(&[]);
    endpoint_needs_shadow(endpoint, participant_locators)
}

fn spawn_shadow_sub(ros2_topic: &str, _discovery_table: &DiscoveryTable) -> anyhow::Result<ShadowSubscriber> {
    ShadowSubscriber::spawn(ros2_topic)
}
