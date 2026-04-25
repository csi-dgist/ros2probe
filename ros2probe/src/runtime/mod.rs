mod bag;
mod command;
mod observers;

use std::{
    collections::{BTreeSet, HashSet, VecDeque},
    fs,
    io,
    path::Path,
    sync::{
        Arc, Mutex, mpsc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

use anyhow::Context;
use aya::programs::SocketFilter;
use log::warn;
use ros2probe_common::MAX_FRAGMENT_FLOWS;
use tokio::{signal, time::Duration};

use crate::{
    capture::{CaptureBuffer, CaptureEngine},
    command::{server, state, state::SharedState},
    discovery::{self, DiscoveryChange, DiscoveryTable, NodeTable},
    protocols::{RtpsDataMessage, RtpsEvent, RtpsProcessor},
    recorder::{RecorderHandle, RecorderTopicGidMap},
    shadow::sub::ShadowSubscriber,
};

pub(crate) use bag::CompressionConfig;
use bag::RecordingSession;
pub use command::{RuntimeCommand, RuntimeReply};
use command::{handle_runtime_commands, sync_shadow_subs};
use observers::{TopicBwSession, TopicDelaySession, TopicEchoSession, TopicHzSession};

const CAPTURE_POLL_INTERVAL: Duration = Duration::from_millis(10);
const DEFAULT_DISCOVERY_SWEEP_INTERVAL: Duration = Duration::from_secs(1);
const RECENT_SAMPLE_CACHE_CAPACITY: usize = 65_536;

#[derive(Clone, Debug, Default)]
pub struct RuntimeConfig;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct SampleIdentity {
    writer_gid: ros2probe_common::TopicGid,
    sequence_number: [u8; 8],
}

struct RecentSampleCache {
    seen: HashSet<SampleIdentity>,
    order: VecDeque<SampleIdentity>,
    capacity: usize,
}

struct LocalIps {
    v4: HashSet<[u8; 4]>,
    v6: HashSet<[u8; 16]>,
}

impl LocalIps {
    fn collect() -> Self {
        let mut v4: HashSet<[u8; 4]> = HashSet::new();
        let mut v6: HashSet<[u8; 16]> = HashSet::new();
        unsafe {
            let mut ifaddrs: *mut libc::ifaddrs = std::ptr::null_mut();
            if libc::getifaddrs(&mut ifaddrs) == 0 {
                let mut ifa = ifaddrs;
                while !ifa.is_null() {
                    let addr = (*ifa).ifa_addr;
                    if !addr.is_null() {
                        match (*addr).sa_family as libc::c_int {
                            libc::AF_INET => {
                                let s = addr as *const libc::sockaddr_in;
                                // s_addr is in network byte order; to_ne_bytes gives the raw
                                // memory bytes which are the octets in network (big-endian) order.
                                v4.insert((*s).sin_addr.s_addr.to_ne_bytes());
                            }
                            libc::AF_INET6 => {
                                let s = addr as *const libc::sockaddr_in6;
                                v6.insert((*s).sin6_addr.s6_addr);
                            }
                            _ => {}
                        }
                    }
                    ifa = (*ifa).ifa_next;
                }
                libc::freeifaddrs(ifaddrs);
            }
        }
        Self { v4, v6 }
    }

    fn contains(&self, ip: &ros2probe_common::IpAddr) -> bool {
        match ip.family {
            ros2probe_common::IP_FAMILY_V4 => {
                if let Ok(b) = <[u8; 4]>::try_from(&ip.bytes[..4]) {
                    self.v4.contains(&b)
                } else {
                    false
                }
            }
            ros2probe_common::IP_FAMILY_V6 => self.v6.contains(&ip.bytes),
            _ => false,
        }
    }
}

struct CaptureWorkerEvent {
    /// RTPS events already decoded on the worker thread. Moving decode off the
    /// single async consumer lets RTPS submessage parsing and DATA_FRAG
    /// reassembly run in parallel per capture interface.
    events: Vec<RtpsEvent>,
}

pub async fn run(_config: RuntimeConfig) -> anyhow::Result<()> {
    let rlim = libc::rlimit {
        rlim_cur: libc::RLIM_INFINITY,
        rlim_max: libc::RLIM_INFINITY,
    };
    let _ = unsafe { libc::setrlimit(libc::RLIMIT_MEMLOCK, &rlim) };

    let mut ebpf = aya::Ebpf::load(aya::include_bytes_aligned!(concat!(
        env!("OUT_DIR"),
        "/ros2probe"
    )))?;
    let interfaces = resolve_capture_interfaces()?;
    println!("capturing interfaces: {}", interfaces.join(", "));

    let prog: &mut SocketFilter = ebpf
        .program_mut("ros2probe")
        .context("eBPF program 'ros2probe' not found")?
        .try_into()?;
    prog.load()?;
    let mut captures = Vec::with_capacity(interfaces.len());
    for interface in &interfaces {
        let mut capture = CaptureEngine::open(interface, MAX_FRAGMENT_FLOWS as usize)?;
        prog.attach(capture.socket_mut().as_mut_inner())
            .with_context(|| format!("attach socket filter to interface {interface}"))?;
        captures.push((interface.clone(), capture));
    }

    let mut gid_map = RecorderTopicGidMap::from_ebpf(&mut ebpf)?;
    // RTPS decoding now happens on each capture worker (one RtpsProcessor per
    // interface). The main loop only consumes already-decoded RtpsEvents.
    let mut discovery_table = DiscoveryTable::default();
    let mut node_table = NodeTable::default();
    let topic_list_state = state::shared_state();
    let (runtime_command_tx, runtime_command_rx) = mpsc::channel();
    let _command_server = server::spawn(
        server::default_socket_path(),
        topic_list_state.clone(),
        runtime_command_tx,
    )
    .context("start command socket server")?;
    let mut discovery_sweep = tokio::time::interval(DEFAULT_DISCOVERY_SWEEP_INTERVAL);
    let mut recording_session = None;
    // MCAP writer runs on its own thread; main runtime only sends data/commands.
    let recorder_handle = RecorderHandle::spawn();
    let mut topic_bw_session = None;
    let mut topic_delay_session = None;
    let mut topic_echo_session = None;
    let mut topic_hz_session = None;
    let mut shadow_subs: std::collections::HashMap<String, ShadowSubscriber> = std::collections::HashMap::new();
    let mut recent_samples = RecentSampleCache::new(RECENT_SAMPLE_CACHE_CAPACITY);
    // Participant GUIDs whose SPDP announcement arrived from a non-local IP → remote participants.
    let remote_participants: Arc<Mutex<HashSet<ros2probe_common::TopicGid>>> =
        Arc::new(Mutex::new(HashSet::new()));
    let local_ips = LocalIps::collect();
    let (capture_event_tx, capture_event_rx) = mpsc::channel();
    let capture_stop = Arc::new(AtomicBool::new(false));
    let mut capture_workers = Vec::with_capacity(captures.len());
    for (interface, capture) in captures {
        capture_workers.push(spawn_capture_worker(
            interface,
            capture,
            capture_event_tx.clone(),
            Arc::clone(&capture_stop),
        ));
    }
    drop(capture_event_tx);

    let ctrl_c = signal::ctrl_c();
    tokio::pin!(ctrl_c);

    loop {
        tokio::select! {
            result = &mut ctrl_c => {
                result?;
                break;
            }
            _ = tokio::time::sleep(CAPTURE_POLL_INTERVAL) => {
                drain_capture_events(
                    &capture_event_rx,
                    &mut discovery_table,
                    &mut node_table,
                    &mut gid_map,
                    recording_session.as_mut(),
                    topic_bw_session.as_mut(),
                    topic_delay_session.as_mut(),
                    topic_echo_session.as_mut(),
                    topic_hz_session.as_mut(),
                    &mut recent_samples,
                    &topic_list_state,
                    &remote_participants,
                    &local_ips,
                    &recorder_handle,
                )?;

                handle_runtime_commands(
                    &runtime_command_rx,
                    &mut recording_session,
                    &mut topic_bw_session,
                    &mut topic_delay_session,
                    &mut topic_echo_session,
                    &mut topic_hz_session,
                    &mut gid_map,
                    &mut shadow_subs,
                    &discovery_table,
                    &recorder_handle,
                )?;
            }
            _ = discovery_sweep.tick() => {
                let now = std::time::SystemTime::now();
                let expire_stats = discovery_table.expire_stale(now);
                if expire_stats.participants_removed > 0
                    || expire_stats.publications_removed > 0
                    || expire_stats.subscriptions_removed > 0
                {
                    for gid in &expire_stats.removed_participant_gids {
                        node_table.replace_participant_nodes(*gid, vec![], now);
                    }
                    sync_topic_filter(
                        &mut gid_map,
                        &discovery_table,
                        recording_session.as_ref(),
                        topic_bw_session.as_ref(),
                        topic_delay_session.as_ref(),
                        topic_echo_session.as_ref(),
                        topic_hz_session.as_ref(),
                    )?;
                    sync_shadow_subs(
                        &mut shadow_subs,
                        recording_session.as_ref(),
                        topic_bw_session.as_ref(),
                        topic_delay_session.as_ref(),
                        topic_echo_session.as_ref(),
                        topic_hz_session.as_ref(),
                        &discovery_table,
                    );
                    state::refresh_from_discovery(&topic_list_state, &discovery_table, &node_table, &remote_participants);
                }
            }
        }
    }

    capture_stop.store(true, Ordering::Relaxed);
    for worker in capture_workers {
        let _ = worker.join();
    }

    if recording_session.take().is_some() {
        let _ = recorder_handle.stop()?;
    }
    // Dropping `recorder_handle` triggers a graceful shutdown: the actor
    // drains any remaining queued data, finalizes an open MCAP (if one was
    // opened and not yet stopped), and exits. The explicit stop above
    // handles the common Ctrl-C path where a recording was in progress.

    Ok(())
}

fn is_interrupted_syscall(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<io::Error>()
            .is_some_and(|io_err| {
                io_err.kind() == io::ErrorKind::Interrupted || io_err.raw_os_error() == Some(libc::EINTR)
            })
            || cause.to_string().contains("Interrupted system call")
            || cause.to_string().contains("os error 4")
    })
}

/// Apply one RTPS event to local state. Returns `true` if any discovery change
/// happened that requires a follow-up `state::refresh_from_discovery`.
/// `sync_topic_filter` is invoked inline whenever the GID set actually changes
/// (Inserted / Removed) so the kernel filter is up-to-date before the next
/// DATA arrives — TRANSIENT_LOCAL topics like `/ros_discovery_info` often emit
/// DATA microseconds after their SEDP. Heartbeats (Updated) skip the filter
/// rebuild because they don't change the GID set, just refresh timestamps.
fn handle_rtps_event(
    event: RtpsEvent,
    discovery_table: &mut DiscoveryTable,
    node_table: &mut NodeTable,
    gid_map: &mut RecorderTopicGidMap,
    recording_session: Option<&mut RecordingSession>,
    topic_bw_session: Option<&mut TopicBwSession>,
    topic_delay_session: Option<&mut TopicDelaySession>,
    topic_echo_session: Option<&mut TopicEchoSession>,
    topic_hz_session: Option<&mut TopicHzSession>,
    recent_samples: &mut RecentSampleCache,
    remote_participants: &Arc<Mutex<HashSet<ros2probe_common::TopicGid>>>,
    local_ips: &LocalIps,
    recorder_handle: &RecorderHandle,
) -> anyhow::Result<bool> {
    // Since RTPS decoding moved into the per-interface workers, the same
    // multicast SPDP DATA can now arrive decoded by two workers (loopback +
    // real NIC). All downstream state updates below — `remote_participants`
    // insert, `discovery_table.apply_sample`, `recent_samples.insert` —
    // are idempotent under duplicate delivery, so no extra dedup is required
    // on the consumer entry point.
    match event {
        RtpsEvent::Discovery(message) => {
            let observed_at = message.socket_timestamp;
            if let Some(sample) = discovery::parse_message(&message)? {
                if let discovery::DiscoverySample::Participant(participant) = &sample {
                    if let Some(guid) = participant.guid {
                        // If SPDP came from a non-local IP, this participant is on a remote host.
                        if !local_ips.contains(&message.src_ip) {
                            remote_participants
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .insert(guid);
                        }
                    }
                }
                let change = discovery_table.apply_sample(sample, observed_at);
                match change {
                    DiscoveryChange::Noop => return Ok(false),
                    DiscoveryChange::Updated => return Ok(true),
                    DiscoveryChange::Inserted | DiscoveryChange::RemovedEndpoint => {
                        sync_topic_filter(
                            gid_map,
                            discovery_table,
                            recording_session.as_deref(),
                            topic_bw_session.as_deref(),
                            topic_delay_session.as_deref(),
                            topic_echo_session.as_deref(),
                            topic_hz_session.as_deref(),
                        )?;
                        return Ok(true);
                    }
                    DiscoveryChange::RemovedParticipant(gid) => {
                        node_table.replace_participant_nodes(gid, vec![], observed_at);
                        sync_topic_filter(
                            gid_map,
                            discovery_table,
                            recording_session.as_deref(),
                            topic_bw_session.as_deref(),
                            topic_delay_session.as_deref(),
                            topic_echo_session.as_deref(),
                            topic_hz_session.as_deref(),
                        )?;
                        return Ok(true);
                    }
                }
            }
        }
        RtpsEvent::Message(message) => {
            if !recent_samples.insert(SampleIdentity {
                writer_gid: message.writer_gid,
                sequence_number: message.sequence_number,
            }) {
                return Ok(false);
            }
            // ros_discovery_info DATA mutates node_table; that change must
            // also drive a CommandState refresh so `rp node list` and the
            // GUI graph see the new nodes.
            return Ok(handle_data_message(
                message,
                node_table,
                discovery_table,
                gid_map,
                recording_session,
                topic_bw_session,
                topic_delay_session,
                topic_echo_session,
                topic_hz_session,
                recorder_handle,
            ));
        }
    }
    Ok(false)
}

#[allow(clippy::too_many_arguments)]
fn drain_capture_events(
    capture_event_rx: &mpsc::Receiver<CaptureWorkerEvent>,
    discovery_table: &mut DiscoveryTable,
    node_table: &mut NodeTable,
    gid_map: &mut RecorderTopicGidMap,
    mut recording_session: Option<&mut RecordingSession>,
    mut topic_bw_session: Option<&mut TopicBwSession>,
    mut topic_delay_session: Option<&mut TopicDelaySession>,
    mut topic_echo_session: Option<&mut TopicEchoSession>,
    mut topic_hz_session: Option<&mut TopicHzSession>,
    recent_samples: &mut RecentSampleCache,
    topic_list_state: &SharedState,
    remote_participants: &Arc<Mutex<HashSet<ros2probe_common::TopicGid>>>,
    local_ips: &LocalIps,
    recorder_handle: &RecorderHandle,
) -> anyhow::Result<()> {
    let mut dirty = false;
    while let Ok(batch) = capture_event_rx.try_recv() {
        for event in batch.events {
            if handle_rtps_event(
                event,
                discovery_table,
                node_table,
                gid_map,
                recording_session.as_deref_mut(),
                topic_bw_session.as_deref_mut(),
                topic_delay_session.as_deref_mut(),
                topic_echo_session.as_deref_mut(),
                topic_hz_session.as_deref_mut(),
                recent_samples,
                remote_participants,
                local_ips,
                recorder_handle,
            )? {
                dirty = true;
            }
        }
    }

    // The eBPF filter is synced inline in handle_rtps_event whenever the GID
    // set actually changes (Inserted / Removed). Here we only do the heavier
    // CommandState rebuild, batching it across whatever the burst contained
    // so a discovery storm doesn't trigger one rebuild per event.
    if dirty {
        state::refresh_from_discovery(topic_list_state, discovery_table, node_table, remote_participants);
    }

    Ok(())
}

fn spawn_capture_worker(
    interface: String,
    mut capture: CaptureEngine,
    capture_event_tx: mpsc::Sender<CaptureWorkerEvent>,
    stop: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut buffer = CaptureBuffer::new(MAX_FRAGMENT_FLOWS as usize);
        // Each worker owns its own RtpsProcessor. DATA_FRAG reassembly state
        // is per-interface, which is correct: IP fragments of a single RTPS
        // sample traverse one NIC, so the fragment flow always terminates on
        // the worker that saw its first chunk.
        let mut rtps = RtpsProcessor::new(MAX_FRAGMENT_FLOWS as usize);
        while !stop.load(Ordering::Relaxed) {
            match capture.pump_once_blocking(&mut buffer) {
                Ok(()) => {
                    let mut events = Vec::with_capacity(buffer.packets().len());
                    while let Some(packet) = buffer.pop() {
                        if let Ok(pkt_events) = rtps.process_packet(packet) {
                            events.extend(pkt_events);
                        }
                    }
                    if events.is_empty() {
                        continue;
                    }
                    if capture_event_tx
                        .send(CaptureWorkerEvent { events })
                        .is_err()
                    {
                        break;
                    }
                }
                Err(err) if is_interrupted_syscall(&err) => break,
                Err(err) => {
                    warn!("capture worker for interface {interface} failed: {err:#}");
                    break;
                }
            }
        }
    })
}

fn resolve_capture_interfaces() -> anyhow::Result<Vec<String>> {
    // Always include loopback: DDS SPDP/SEDP discovery is always UDP, even
    // for participants that use shared-memory or intra-process data transport.
    // Without lo we can't see topics/nodes on standalone machines.
    let mut interfaces = vec!["lo".to_string()];

    let mut external: Vec<String> = fs::read_dir("/sys/class/net")
        .context("read /sys/class/net")?
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            is_external_interface(&name, &entry.path()).then_some(name)
        })
        .collect();
    external.sort();
    interfaces.extend(external);

    Ok(interfaces)
}

fn is_external_interface(name: &str, path: &Path) -> bool {
    if name == "lo" {
        return false;
    }

    let canonical = match fs::canonicalize(path) {
        Ok(path) => path,
        Err(_) => return false,
    };
    if canonical.to_string_lossy().contains("/virtual/") {
        return false;
    }

    let operstate = fs::read_to_string(path.join("operstate"))
        .ok()
        .map(|state| state.trim().to_string());
    !matches!(operstate.as_deref(), Some("down") | Some("notpresent"))
}

fn handle_data_message(
    message: RtpsDataMessage,
    node_table: &mut NodeTable,
    discovery_table: &DiscoveryTable,
    gid_map: &RecorderTopicGidMap,
    recording_session: Option<&mut RecordingSession>,
    topic_bw_session: Option<&mut TopicBwSession>,
    topic_delay_session: Option<&mut TopicDelaySession>,
    topic_echo_session: Option<&mut TopicEchoSession>,
    topic_hz_session: Option<&mut TopicHzSession>,
    recorder_handle: &RecorderHandle,
) -> bool {
    let mut node_table_dirty = false;
    let gid_metadata = gid_map.metadata_for_message(&message.writer_gid, &message.reader_gid);
    let topic_name = gid_metadata
        .map(|metadata| metadata.topic_name.as_str())
        .or_else(|| {
            discovery_table
                .publication(&message.writer_gid)
                .and_then(|endpoint| endpoint.topic_name.as_deref())
        });

    if topic_name.is_some_and(discovery::is_ros_discovery_info) {
        match discovery::parse_participant_entities_info(&message.payload) {
            Ok(info) => {
                node_table.replace_participant_nodes(
                    info.participant_gid,
                    info.nodes,
                    message.socket_timestamp,
                );
                node_table_dirty = true;
            }
            Err(err) => warn!("failed to parse /ros_discovery_info: {err:#}"),
        }
    }

    let Some(metadata) = gid_metadata else {
        return node_table_dirty;
    };

    if let Some(session) = recording_session {
        bag::record_message(session, recorder_handle, &message, metadata);
    }
    observers::bw_observe_message(topic_bw_session, &message, metadata);
    observers::delay_observe_message(topic_delay_session, &message, metadata);
    observers::echo_observe_message(topic_echo_session, &message, metadata);
    observers::hz_observe_message(topic_hz_session, &message, metadata);
    node_table_dirty
}

fn sync_topic_filter(
    gid_map: &mut RecorderTopicGidMap,
    discovery_table: &DiscoveryTable,
    recording_session: Option<&RecordingSession>,
    topic_bw_session: Option<&TopicBwSession>,
    topic_delay_session: Option<&TopicDelaySession>,
    topic_echo_session: Option<&TopicEchoSession>,
    topic_hz_session: Option<&TopicHzSession>,
) -> anyhow::Result<()> {
    let mut topics = BTreeSet::new();
    // `all_topics` only applies to `bag record --all`. Previously a second
    // branch (`no session active`) also triggered All, which meant the eBPF
    // filter let every user topic's DATA through whenever nothing was being
    // observed — defeating the purpose of having a filter. Now idle state
    // falls into `GidMapMode::None` and DATA is dropped in-kernel.
    let all_topics = recording_session.is_some_and(|session| session.topics.is_empty());

    if !all_topics {
        if let Some(session) = recording_session {
            topics.extend(session.topics.iter().cloned());
        }
        if let Some(session) = topic_bw_session {
            topics.insert(session.topic_name().to_string());
        }
        if let Some(session) = topic_delay_session {
            topics.insert(session.topic_name().to_string());
        }
        if let Some(session) = topic_echo_session {
            topics.insert(session.topic_name().to_string());
        }
        if let Some(session) = topic_hz_session {
            topics.insert(session.topic_name().to_string());
        }
    }

    if all_topics {
        gid_map.configure(&[]);
    } else if topics.is_empty() {
        gid_map.configure_none();
    } else {
        let selected_topics = topics.into_iter().collect::<Vec<_>>();
        gid_map.configure(&selected_topics);
    }
    gid_map.rebuild_from_table(discovery_table)?;
    Ok(())
}

impl RecentSampleCache {
    fn new(capacity: usize) -> Self {
        Self {
            seen: HashSet::with_capacity(capacity),
            order: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    fn insert(&mut self, sample: SampleIdentity) -> bool {
        if self.seen.contains(&sample) {
            return false;
        }

        if self.order.len() == self.capacity {
            if let Some(oldest) = self.order.pop_front() {
                self.seen.remove(&oldest);
            }
        }

        self.order.push_back(sample);
        self.seen.insert(sample);
        true
    }
}
