mod bag;
mod command;
mod observers;
mod zenoh_discover;
mod zenoh_shadow;

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque},
    fs, io,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
};

use anyhow::{Context, bail};
use aya::{
    Ebpf,
    maps::{HashMap as BpfHashMap, MapData},
    programs::SocketFilter,
};
use log::{debug, trace, warn};
use ros2probe_common::{MAX_FRAGMENT_FLOWS, MAX_ZENOH_PORTS};
use tokio::{
    signal,
    time::{Duration, Instant},
};

use crate::protocols::rtps::{RTPS_FRAGMENT_DEFRAG_TOTAL_CAPACITY, RtpsFragmentMemoryBudget};
use crate::protocols::zenoh::{ZENOH_FRAGMENT_DEFRAG_TOTAL_CAPACITY, ZenohFragmentMemoryBudget};
use crate::{
    capture::{CaptureBuffer, CaptureEngine, TransportProtocol, ZenohCapturePorts},
    command::{
        protocol::{DiscoverMode, DiscoverRequest, DiscoverResponse},
        server, state,
        state::SharedState,
    },
    discovery::{
        self, DiscoveredEndpoint, DiscoveredParticipant, DiscoveryChange, DiscoverySample,
        DiscoveryTable, EndpointId, NodeKey, NodeSample, NodeTable, ParticipantId,
    },
    protocols::{
        RtpsDataMessage, RtpsEvent, RtpsProcessor, ZenohEvent, ZenohProcessor, ZenohRosEntityKind,
        ZenohRosLivelinessEntity, ZenohRosSampleIdentity, ZenohRosTopicSample, ZenohSemanticEvent,
        ZenohUnresolvedTopicSample, parse_ros_liveliness_keyexpr, rmw_zenoh_entity_gid,
        rmw_zenoh_topic_keyexpr,
    },
    recorder::{RecorderHandle, RecorderTopicGidMap},
    shadow::sub::ShadowSubscriber,
};

pub(crate) use bag::CompressionConfig;
use bag::RecordingSession;
pub use command::{RuntimeCommand, RuntimeReply};
use command::{handle_runtime_commands, sync_shadow_subs};
use observers::{TopicBwSession, TopicDelaySession, TopicEchoSession, TopicHzSession};
use zenoh_shadow::{ZenohShadow, ZenohShadowSample};

const CAPTURE_POLL_INTERVAL: Duration = Duration::from_millis(10);
const DEFAULT_DISCOVERY_SWEEP_INTERVAL: Duration = Duration::from_secs(1);
const CAPTURE_EVENT_CHANNEL_CAPACITY: usize = 1024;
const CAPTURE_STATS_INTERVAL: Duration = Duration::from_secs(1);
const ZENOH_SHADOW_CHANNEL_CAPACITY: usize = 4096;
const RECENT_SAMPLE_CACHE_CAPACITY: usize = 65_536;
const ZENOH_FLOW_INACTIVITY_TIMEOUT: Duration = Duration::from_secs(90);
const ZENOH_UDP_PORTS_MAP: &str = "ZENOH_UDP_PORTS";
const ZENOH_TCP_PORTS_MAP: &str = "ZENOH_TCP_PORTS";

#[derive(Clone, Debug, Default)]
pub struct RuntimeConfig {
    pub zenoh_ports: ZenohCapturePorts,
}

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

struct RecentZenohSampleCache {
    seen: HashSet<ZenohRosSampleIdentity>,
    order: VecDeque<ZenohRosSampleIdentity>,
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
    /// Zenoh TCP/UDP batches framed by the worker. The payload is still a raw
    /// Zenoh batch; transport-message decoding is the next layer.
    zenoh_events: Vec<ZenohEvent>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct RuntimeZenohNodeKey {
    participant_id: ParticipantId,
    node_namespace: String,
    node_name: String,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct RuntimeZenohFlowKey {
    protocol: TransportProtocol,
    flow: ros2probe_common::FlowTuple,
}

impl RuntimeZenohFlowKey {
    fn active_discovery() -> Self {
        let loopback = ros2probe_common::IpAddr::from_v4(0x7f00_0001);
        Self {
            protocol: TransportProtocol::Tcp,
            flow: ros2probe_common::FlowTuple::new(loopback, loopback, 0, 0),
        }
    }
}

#[derive(Clone, Debug)]
struct RuntimeZenohNode {
    key: RuntimeZenohNodeKey,
    node_alive: bool,
    writer_gids: BTreeSet<ros2probe_common::TopicGid>,
    reader_gids: BTreeSet<ros2probe_common::TopicGid>,
    writer_endpoint_ids: BTreeSet<EndpointId>,
    reader_endpoint_ids: BTreeSet<EndpointId>,
}

#[derive(Clone, Debug)]
enum RuntimeZenohEntityRef {
    Node {
        key: RuntimeZenohNodeKey,
    },
    Writer {
        key: RuntimeZenohNodeKey,
        endpoint_id: EndpointId,
        endpoint_gid: ros2probe_common::TopicGid,
        topic_name: String,
        type_name: String,
        type_hash: String,
        qos: String,
    },
    Reader {
        key: RuntimeZenohNodeKey,
        endpoint_id: EndpointId,
        endpoint_gid: ros2probe_common::TopicGid,
        topic_name: String,
        type_name: String,
        type_hash: String,
        qos: String,
    },
}

#[derive(Debug, Default)]
struct RuntimeZenohGraph {
    nodes: BTreeMap<RuntimeZenohNodeKey, RuntimeZenohNode>,
    entities: BTreeMap<String, HashMap<RuntimeZenohFlowKey, RuntimeZenohEntityRef>>,
    flow_entities: HashMap<RuntimeZenohFlowKey, BTreeSet<String>>,
    flow_last_seen: HashMap<RuntimeZenohFlowKey, std::time::SystemTime>,
}

pub async fn run(config: RuntimeConfig) -> anyhow::Result<()> {
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

    {
        let prog: &mut SocketFilter = ebpf
            .program_mut("ros2probe")
            .context("eBPF program 'ros2probe' not found")?
            .try_into()?;
        prog.load()?;
    }
    configure_zenoh_port_maps(&mut ebpf, &config.zenoh_ports)?;
    let mut captures = Vec::with_capacity(interfaces.len());
    for interface in &interfaces {
        let mut capture = CaptureEngine::open(
            interface,
            MAX_FRAGMENT_FLOWS as usize,
            config.zenoh_ports.clone(),
        )?;
        let prog: &mut SocketFilter = ebpf
            .program_mut("ros2probe")
            .context("eBPF program 'ros2probe' not found")?
            .try_into()?;
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
    let mut zenoh_graph = RuntimeZenohGraph::default();
    let mut observed_zenoh_transports = HashSet::new();
    let mut zenoh_shm_topics = BTreeSet::new();
    let (zenoh_shadow_tx, zenoh_shadow_rx) = mpsc::sync_channel(ZENOH_SHADOW_CHANNEL_CAPACITY);
    let mut zenoh_shadow = ZenohShadow::new(zenoh_shadow_tx);
    let mut shadow_subs: std::collections::HashMap<String, ShadowSubscriber> =
        std::collections::HashMap::new();
    let mut recent_samples = RecentSampleCache::new(RECENT_SAMPLE_CACHE_CAPACITY);
    let mut recent_zenoh_samples = RecentZenohSampleCache::new(RECENT_SAMPLE_CACHE_CAPACITY);
    // Participant GUIDs whose SPDP announcement arrived from a non-local IP → remote participants.
    let remote_participants: Arc<Mutex<HashSet<ParticipantId>>> =
        Arc::new(Mutex::new(HashSet::new()));
    let local_ips = LocalIps::collect();
    let (capture_event_tx, capture_event_rx) = mpsc::sync_channel(CAPTURE_EVENT_CHANNEL_CAPACITY);
    let capture_stop = Arc::new(AtomicBool::new(false));
    let rtps_fragment_budget = Arc::new(RtpsFragmentMemoryBudget::new(
        RTPS_FRAGMENT_DEFRAG_TOTAL_CAPACITY,
    ));
    let zenoh_fragment_budget = Arc::new(ZenohFragmentMemoryBudget::new(
        ZENOH_FRAGMENT_DEFRAG_TOTAL_CAPACITY,
    ));
    let mut capture_workers = Vec::with_capacity(captures.len());
    for (interface, capture) in captures {
        capture_workers.push(spawn_capture_worker(
            interface,
            capture,
            capture_event_tx.clone(),
            Arc::clone(&capture_stop),
            Arc::clone(&rtps_fragment_budget),
            Arc::clone(&zenoh_fragment_budget),
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
                    &mut zenoh_graph,
                    &mut recent_samples,
                    &mut recent_zenoh_samples,
                    &mut observed_zenoh_transports,
                    &mut zenoh_shm_topics,
                    &topic_list_state,
                    &remote_participants,
                    &local_ips,
                    &recorder_handle,
                )?;
                drain_zenoh_shadow_samples(
                    &zenoh_shadow_rx,
                    recording_session.as_mut(),
                    topic_bw_session.as_mut(),
                    topic_delay_session.as_mut(),
                    topic_echo_session.as_mut(),
                    topic_hz_session.as_mut(),
                    &mut recent_zenoh_samples,
                    &recorder_handle,
                );

                let rp = remote_participants
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                handle_runtime_commands(
                    &runtime_command_rx,
                    &config.zenoh_ports,
                    &observed_zenoh_transports,
                    &mut recording_session,
                    &mut topic_bw_session,
                    &mut topic_delay_session,
                    &mut topic_echo_session,
                    &mut topic_hz_session,
                    &mut gid_map,
                    &mut shadow_subs,
                    &mut zenoh_graph,
                    &mut discovery_table,
                    &mut node_table,
                    &topic_list_state,
                    &recorder_handle,
                    &remote_participants,
                    &rp,
                ).await?;

                let shadow_keyexprs = active_zenoh_shadow_keyexprs(
                    recording_session.as_ref(),
                    topic_bw_session.as_ref(),
                    topic_delay_session.as_ref(),
                    topic_echo_session.as_ref(),
                    topic_hz_session.as_ref(),
                    &discovery_table,
                    &zenoh_shm_topics,
                );
                if let Err(err) = zenoh_shadow
                    .sync(
                        shadow_keyexprs,
                        &config.zenoh_ports,
                        &observed_zenoh_transports,
                    )
                    .await
                {
                    warn!("Zenoh shadow subscriber sync failed: {err:#}");
                }
            }
            _ = discovery_sweep.tick() => {
                let now = std::time::SystemTime::now();
                let zenoh_expired = zenoh_graph.expire_inactive_flows(
                    now,
                    ZENOH_FLOW_INACTIVITY_TIMEOUT,
                    &mut discovery_table,
                    &mut node_table,
                );
                let zenoh_refreshed = zenoh_graph.refresh_discovery(
                    now,
                    &mut discovery_table,
                    &mut node_table,
                );
                let expire_stats = discovery_table.expire_stale(now);
                if zenoh_expired
                    || zenoh_refreshed
                    || expire_stats.participants_removed > 0
                    || expire_stats.publications_removed > 0
                    || expire_stats.subscriptions_removed > 0
                {
                    for gid in &expire_stats.removed_participant_gids {
                        node_table.replace_participant_nodes(*gid, vec![], now);
                    }
                    {
                        let mut rp = remote_participants.lock().unwrap_or_else(|e| e.into_inner());
                        for id in &expire_stats.removed_participant_ids {
                            rp.remove(id);
                        }
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
                    {
                        let rp = remote_participants.lock().unwrap_or_else(|e| e.into_inner());
                        sync_shadow_subs(
                            &mut shadow_subs,
                            recording_session.as_ref(),
                            topic_bw_session.as_ref(),
                            topic_delay_session.as_ref(),
                            topic_echo_session.as_ref(),
                            topic_hz_session.as_ref(),
                            &discovery_table,
                            &rp,
                        );
                    }
                    state::refresh_from_discovery(
                        &topic_list_state,
                        &discovery_table,
                        &node_table,
                        &remote_participants,
                    );
                }
            }
        }
    }

    if let Err(err) = zenoh_shadow.close().await {
        warn!("close Zenoh shadow subscriber: {err:#}");
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
        cause.downcast_ref::<io::Error>().is_some_and(|io_err| {
            io_err.kind() == io::ErrorKind::Interrupted
                || io_err.raw_os_error() == Some(libc::EINTR)
        }) || cause.to_string().contains("Interrupted system call")
            || cause.to_string().contains("os error 4")
    })
}

async fn handle_discover_runtime_command(
    request: DiscoverRequest,
    zenoh_ports: &ZenohCapturePorts,
    observed_zenoh_transports: &HashSet<TransportProtocol>,
    zenoh_graph: &mut RuntimeZenohGraph,
    discovery_table: &mut DiscoveryTable,
    node_table: &mut NodeTable,
    topic_list_state: &SharedState,
    remote_participants: &Arc<Mutex<HashSet<ParticipantId>>>,
) -> anyhow::Result<DiscoverResponse> {
    let now = std::time::SystemTime::now();
    let (run_rtps, run_zenoh) = discover_methods(request.mode);
    let mut response = DiscoverResponse {
        triggered: run_rtps || run_zenoh,
        rtps_triggered: false,
        zenoh_triggered: false,
        zenoh_tokens: 0,
        messages: Vec::new(),
    };

    if run_rtps {
        crate::shadow::discover::run_discovery();
        response.rtps_triggered = true;
        response
            .messages
            .push("RTPS discovery triggered.".to_string());
    }

    if run_zenoh {
        match zenoh_discover::liveliness_get(&request, zenoh_ports, observed_zenoh_transports).await
        {
            Ok(snapshot) => {
                let keyexprs = snapshot.tokens;
                let mut parsed = 0usize;
                let mut dirty = false;
                let flow = RuntimeZenohFlowKey::active_discovery();
                zenoh_graph.touch_flow(flow, now);
                let mut current_keyexprs = BTreeSet::new();
                let mut active_participants = HashSet::new();
                let mut entities = Vec::new();
                let mut node_tokens = 0usize;
                let mut publisher_tokens = 0usize;
                let mut subscription_tokens = 0usize;
                let mut service_server_tokens = 0usize;
                let mut service_client_tokens = 0usize;
                for keyexpr in keyexprs {
                    match parse_ros_liveliness_keyexpr(&keyexpr) {
                        Ok(entity) => {
                            parsed += 1;
                            match entity.kind {
                                ZenohRosEntityKind::Node => node_tokens += 1,
                                ZenohRosEntityKind::Publisher => publisher_tokens += 1,
                                ZenohRosEntityKind::Subscription => subscription_tokens += 1,
                                ZenohRosEntityKind::ServiceServer => service_server_tokens += 1,
                                ZenohRosEntityKind::ServiceClient => service_client_tokens += 1,
                            }
                            active_participants.insert(zenoh_participant_id(&entity));
                            current_keyexprs.insert(entity.keyexpr.clone());
                            entities.push(entity);
                        }
                        Err(err) => {
                            trace!("ignore non-ROS Zenoh liveliness token {keyexpr}: {err:?}");
                        }
                    }
                }

                let stale_keyexprs = zenoh_graph
                    .flow_entities
                    .get(&flow)
                    .into_iter()
                    .flat_map(|keyexprs| keyexprs.difference(&current_keyexprs))
                    .cloned()
                    .collect::<Vec<_>>();
                for keyexpr in stale_keyexprs {
                    if zenoh_graph.remove_entity_from_flow(
                        flow,
                        &keyexpr,
                        now,
                        discovery_table,
                        node_table,
                    ) {
                        dirty = true;
                    }
                }

                if !active_participants.is_empty() {
                    let mut remote = remote_participants
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    for participant_gid in active_participants {
                        if remote.insert(participant_gid) {
                            dirty = true;
                        }
                    }
                }

                for entity in entities {
                    if zenoh_graph.insert_entity(flow, entity, now, discovery_table, node_table) {
                        dirty = true;
                    }
                }

                if zenoh_graph.refresh_discovery(now, discovery_table, node_table) {
                    dirty = true;
                }
                if dirty {
                    state::refresh_from_discovery(
                        topic_list_state,
                        discovery_table,
                        node_table,
                        remote_participants,
                    );
                }

                response.zenoh_triggered = true;
                response.zenoh_tokens = parsed;
                response.messages.push(format!(
                    "Zenoh liveliness refresh imported {parsed} ROS token(s) from {} via {}: nodes={node_tokens}, publishers={publisher_tokens}, subscriptions={subscription_tokens}, service_servers={service_server_tokens}, service_clients={service_client_tokens}.",
                    snapshot.queried_keyexprs.join(", "),
                    snapshot
                        .successful_endpoints
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
                for (endpoint, error) in snapshot.failed_endpoints {
                    response.messages.push(format!(
                        "Zenoh endpoint {endpoint} was unavailable during discovery: {error}"
                    ));
                }
                response
                    .messages
                    .extend(discovery_graph_summary(discovery_table, node_table));
            }
            Err(err) => {
                response
                    .messages
                    .push(format!("Zenoh liveliness refresh failed: {err:#}"));
            }
        }
    }

    Ok(response)
}

fn discover_methods(mode: DiscoverMode) -> (bool, bool) {
    match mode {
        DiscoverMode::Rtps => (true, false),
        DiscoverMode::Zenoh => (false, true),
        DiscoverMode::All => (true, true),
        DiscoverMode::Auto => (true, true),
    }
}

fn discovery_graph_summary(
    discovery_table: &DiscoveryTable,
    node_table: &NodeTable,
) -> Vec<String> {
    let mut messages = Vec::new();

    let mut nodes = node_table
        .nodes()
        .values()
        .map(|node| full_node_name(&node.key.node_namespace, &node.key.node_name))
        .collect::<Vec<_>>();
    nodes.sort();
    nodes.dedup();
    if nodes.is_empty() {
        messages.push("Nodes: none".to_string());
    } else {
        messages.push(format!("Nodes ({}): {}", nodes.len(), nodes.join(", ")));
    }

    let mut topics = discovery_table
        .topics()
        .into_iter()
        .filter(|topic| {
            !topic.topic_name.starts_with("rq/")
                && !topic.topic_name.starts_with("rr/")
                && topic
                    .type_names
                    .iter()
                    .all(|type_name| !type_name.contains("::srv::dds_::"))
        })
        .map(|topic| {
            let topic_name = topic
                .topic_name
                .strip_prefix("rt/")
                .map(|name| format!("/{name}"))
                .unwrap_or(topic.topic_name);
            let type_names = if topic.type_names.is_empty() {
                "-".to_string()
            } else {
                topic
                    .type_names
                    .iter()
                    .map(|type_name| normalize_zenoh_sample_type_name(type_name))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            format!(
                "{topic_name} [{type_names}] pub={} sub={}",
                topic.publisher_count, topic.subscription_count
            )
        })
        .collect::<Vec<_>>();
    topics.sort();
    if topics.is_empty() {
        messages.push("Topics: none".to_string());
    } else {
        messages.push(format!("Topics ({}): {}", topics.len(), topics.join("; ")));
    }

    messages
}

fn full_node_name(namespace: &str, name: &str) -> String {
    if namespace == "/" {
        format!("/{name}")
    } else {
        format!("{namespace}/{name}")
    }
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
    remote_participants: &Arc<Mutex<HashSet<ParticipantId>>>,
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
                                .insert(ParticipantId::rtps(guid));
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
                    DiscoveryChange::RemovedParticipant { rtps_gid, .. } => {
                        if let Some(gid) = rtps_gid {
                            node_table.replace_participant_nodes(gid, vec![], observed_at);
                        }
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

impl RuntimeZenohGraph {
    fn first_entity_ref(
        refs: &HashMap<RuntimeZenohFlowKey, RuntimeZenohEntityRef>,
    ) -> Option<&RuntimeZenohEntityRef> {
        refs.get(&RuntimeZenohFlowKey::active_discovery())
            .or_else(|| refs.values().next())
    }

    fn insert_entity(
        &mut self,
        flow: RuntimeZenohFlowKey,
        entity: ZenohRosLivelinessEntity,
        observed_at: std::time::SystemTime,
        discovery_table: &mut DiscoveryTable,
        node_table: &mut NodeTable,
    ) -> bool {
        let mut dirty = false;
        if self
            .entities
            .get(&entity.keyexpr)
            .is_some_and(|refs| refs.contains_key(&flow))
        {
            self.flow_last_seen.insert(flow, observed_at);
            self.insert_flow_entity(flow, entity.keyexpr);
            return false;
        }
        self.flow_last_seen.insert(flow, observed_at);
        let is_first_ref = !self.entities.contains_key(&entity.keyexpr);

        let participant_id = zenoh_participant_id(&entity);
        if is_first_ref {
            dirty |= discovery_table.apply_sample(
                DiscoverySample::Participant(DiscoveredParticipant {
                    guid: None,
                    participant_id: Some(participant_id.clone()),
                    ..DiscoveredParticipant::default()
                }),
                observed_at,
            ) != DiscoveryChange::Noop;
        }

        let key = RuntimeZenohNodeKey {
            participant_id: participant_id.clone(),
            node_namespace: entity.node.namespace.clone(),
            node_name: entity.node.node_name.clone(),
        };
        match entity.kind {
            ZenohRosEntityKind::Node => {
                let node = self
                    .nodes
                    .entry(key.clone())
                    .or_insert_with(|| RuntimeZenohNode {
                        key: key.clone(),
                        node_alive: false,
                        writer_gids: BTreeSet::new(),
                        reader_gids: BTreeSet::new(),
                        writer_endpoint_ids: BTreeSet::new(),
                        reader_endpoint_ids: BTreeSet::new(),
                    });
                if !node.node_alive {
                    node.node_alive = true;
                    upsert_zenoh_node_sample(node_table, node, observed_at);
                    dirty = true;
                }
                let keyexpr = entity.keyexpr;
                self.insert_flow_entity(flow, keyexpr.clone());
                self.entities
                    .entry(keyexpr)
                    .or_default()
                    .insert(flow, RuntimeZenohEntityRef::Node { key: key.clone() });
                dirty |= is_first_ref;
            }
            ZenohRosEntityKind::Publisher
            | ZenohRosEntityKind::ServiceClient
            | ZenohRosEntityKind::Subscription
            | ZenohRosEntityKind::ServiceServer => {
                let Some(topic) = entity.topic.as_ref() else {
                    return dirty;
                };
                let endpoint_id = zenoh_endpoint_id(&entity);
                let endpoint_gid = rmw_zenoh_entity_gid(&entity);
                let endpoint = DiscoveredEndpoint {
                    endpoint_gid: Some(endpoint_gid),
                    endpoint_id: Some(endpoint_id.clone()),
                    participant_gid: None,
                    participant_id: Some(participant_id.clone()),
                    topic_name: Some(topic.name.clone()),
                    type_name: Some(topic.type_name.clone()),
                    type_hash: Some(topic.type_hash.clone()),
                    history: zenoh_history_qos(&topic.qos),
                    reliability: zenoh_reliability_qos(&topic.qos),
                    durability: zenoh_durability_qos(&topic.qos),
                    deadline: zenoh_deadline_qos(&topic.qos),
                    lifespan: zenoh_lifespan_qos(&topic.qos),
                    liveliness: zenoh_liveliness_qos(&topic.qos),
                    liveliness_lease_duration: zenoh_liveliness_lease_duration_qos(&topic.qos),
                    ..DiscoveredEndpoint::default()
                };
                let (sample, entity_ref) = match entity.kind {
                    ZenohRosEntityKind::Publisher | ZenohRosEntityKind::ServiceClient => {
                        let node =
                            self.nodes
                                .entry(key.clone())
                                .or_insert_with(|| RuntimeZenohNode {
                                    key: key.clone(),
                                    node_alive: false,
                                    writer_gids: BTreeSet::new(),
                                    reader_gids: BTreeSet::new(),
                                    writer_endpoint_ids: BTreeSet::new(),
                                    reader_endpoint_ids: BTreeSet::new(),
                                });
                        let id_inserted = node.writer_endpoint_ids.insert(endpoint_id.clone());
                        let gid_inserted = node.writer_gids.insert(endpoint_gid);
                        if id_inserted || gid_inserted {
                            upsert_zenoh_node_sample(node_table, node, observed_at);
                            dirty = true;
                        }
                        (
                            DiscoverySample::Publication(endpoint),
                            RuntimeZenohEntityRef::Writer {
                                key: key.clone(),
                                endpoint_id: endpoint_id.clone(),
                                endpoint_gid,
                                topic_name: topic.name.clone(),
                                type_name: topic.type_name.clone(),
                                type_hash: topic.type_hash.clone(),
                                qos: topic.qos.clone(),
                            },
                        )
                    }
                    ZenohRosEntityKind::Subscription | ZenohRosEntityKind::ServiceServer => {
                        let node =
                            self.nodes
                                .entry(key.clone())
                                .or_insert_with(|| RuntimeZenohNode {
                                    key: key.clone(),
                                    node_alive: false,
                                    writer_gids: BTreeSet::new(),
                                    reader_gids: BTreeSet::new(),
                                    writer_endpoint_ids: BTreeSet::new(),
                                    reader_endpoint_ids: BTreeSet::new(),
                                });
                        let id_inserted = node.reader_endpoint_ids.insert(endpoint_id.clone());
                        let gid_inserted = node.reader_gids.insert(endpoint_gid);
                        if id_inserted || gid_inserted {
                            upsert_zenoh_node_sample(node_table, node, observed_at);
                            dirty = true;
                        }
                        (
                            DiscoverySample::Subscription(endpoint),
                            RuntimeZenohEntityRef::Reader {
                                key: key.clone(),
                                endpoint_id: endpoint_id.clone(),
                                endpoint_gid,
                                topic_name: topic.name.clone(),
                                type_name: topic.type_name.clone(),
                                type_hash: topic.type_hash.clone(),
                                qos: topic.qos.clone(),
                            },
                        )
                    }
                    ZenohRosEntityKind::Node => unreachable!(),
                };

                if is_first_ref || !discovery_sample_exists(discovery_table, &sample) {
                    dirty |=
                        discovery_table.apply_sample(sample, observed_at) != DiscoveryChange::Noop;
                }
                self.insert_flow_entity(flow, entity.keyexpr.clone());
                self.entities
                    .entry(entity.keyexpr)
                    .or_default()
                    .insert(flow, entity_ref);
                dirty |= is_first_ref;
            }
        }

        dirty
    }

    fn remove_entity_from_flow(
        &mut self,
        flow: RuntimeZenohFlowKey,
        keyexpr: &str,
        observed_at: std::time::SystemTime,
        discovery_table: &mut DiscoveryTable,
        node_table: &mut NodeTable,
    ) -> bool {
        let (entity_ref, final_ref) = {
            let Some(refs) = self.entities.get_mut(keyexpr) else {
                return false;
            };
            let Some(entity_ref) = refs.remove(&flow) else {
                return false;
            };
            (entity_ref, refs.is_empty())
        };
        self.remove_flow_entity(flow, keyexpr);
        if !final_ref {
            return false;
        }
        self.entities.remove(keyexpr);
        self.remove_entity_ref(
            keyexpr,
            entity_ref,
            observed_at,
            discovery_table,
            node_table,
        )
    }

    fn remove_entity_ref(
        &mut self,
        _keyexpr: &str,
        entity_ref: RuntimeZenohEntityRef,
        observed_at: std::time::SystemTime,
        discovery_table: &mut DiscoveryTable,
        node_table: &mut NodeTable,
    ) -> bool {
        let mut dirty = false;
        match entity_ref {
            RuntimeZenohEntityRef::Node { key } => {
                if let Some(node) = self.nodes.get_mut(&key) {
                    node.node_alive = false;
                }
                dirty |= self.sync_or_remove_node(key, observed_at, discovery_table, node_table);
            }
            RuntimeZenohEntityRef::Writer {
                key,
                endpoint_id,
                endpoint_gid,
                ..
            } => {
                dirty |= discovery_table
                    .remove_publication_by_id(&endpoint_id)
                    .is_some();
                if let Some(node) = self.nodes.get_mut(&key) {
                    node.writer_endpoint_ids.remove(&endpoint_id);
                    node.writer_gids.remove(&endpoint_gid);
                }
                dirty |= self.sync_or_remove_node(key, observed_at, discovery_table, node_table);
            }
            RuntimeZenohEntityRef::Reader {
                key,
                endpoint_id,
                endpoint_gid,
                ..
            } => {
                dirty |= discovery_table
                    .remove_subscription_by_id(&endpoint_id)
                    .is_some();
                if let Some(node) = self.nodes.get_mut(&key) {
                    node.reader_endpoint_ids.remove(&endpoint_id);
                    node.reader_gids.remove(&endpoint_gid);
                }
                dirty |= self.sync_or_remove_node(key, observed_at, discovery_table, node_table);
            }
        }
        dirty
    }

    fn sync_or_remove_node(
        &mut self,
        key: RuntimeZenohNodeKey,
        observed_at: std::time::SystemTime,
        discovery_table: &mut DiscoveryTable,
        node_table: &mut NodeTable,
    ) -> bool {
        let Some(node) = self.nodes.get(&key) else {
            return false;
        };
        if node.node_alive
            || !node.writer_endpoint_ids.is_empty()
            || !node.reader_endpoint_ids.is_empty()
        {
            upsert_zenoh_node_sample(node_table, node, observed_at);
            return true;
        }

        self.nodes.remove(&key);
        node_table.remove_node(&NodeKey {
            participant_id: key.participant_id.clone(),
            participant_gid: None,
            node_namespace: key.node_namespace,
            node_name: key.node_name,
        });
        let _ = observed_at;
        let _ = discovery_table.remove_participant_by_id(&key.participant_id);
        true
    }

    fn touch_flow(&mut self, flow: RuntimeZenohFlowKey, observed_at: std::time::SystemTime) {
        if self.flow_entities.contains_key(&flow) {
            self.flow_last_seen.insert(flow, observed_at);
        }
    }

    fn insert_flow_entity(&mut self, flow: RuntimeZenohFlowKey, keyexpr: String) {
        self.flow_entities.entry(flow).or_default().insert(keyexpr);
    }

    fn remove_flow_entity(&mut self, flow: RuntimeZenohFlowKey, keyexpr: &str) {
        let Some(keyexprs) = self.flow_entities.get_mut(&flow) else {
            return;
        };
        keyexprs.remove(keyexpr);
        if keyexprs.is_empty() {
            self.flow_entities.remove(&flow);
            self.flow_last_seen.remove(&flow);
        }
    }

    fn expire_inactive_flows(
        &mut self,
        now: std::time::SystemTime,
        timeout: Duration,
        discovery_table: &mut DiscoveryTable,
        node_table: &mut NodeTable,
    ) -> bool {
        let expired = self
            .flow_last_seen
            .iter()
            .filter(|(flow, _)| **flow != RuntimeZenohFlowKey::active_discovery())
            .filter_map(|(flow, last_seen)| {
                now.duration_since(*last_seen)
                    .ok()
                    .filter(|elapsed| *elapsed > timeout)
                    .map(|_| *flow)
            })
            .collect::<Vec<_>>();

        let mut dirty = false;
        for flow in expired {
            self.flow_last_seen.remove(&flow);
            let keyexprs = self
                .flow_entities
                .remove(&flow)
                .into_iter()
                .flatten()
                .collect::<Vec<_>>();
            for keyexpr in keyexprs {
                dirty |=
                    self.remove_entity_from_flow(flow, &keyexpr, now, discovery_table, node_table);
            }
        }
        dirty
    }

    fn refresh_discovery(
        &self,
        observed_at: std::time::SystemTime,
        discovery_table: &mut DiscoveryTable,
        node_table: &mut NodeTable,
    ) -> bool {
        let mut dirty = false;

        for node in self.nodes.values() {
            let node_key = NodeKey {
                participant_id: node.key.participant_id.clone(),
                participant_gid: None,
                node_namespace: node.key.node_namespace.clone(),
                node_name: node.key.node_name.clone(),
            };
            if node_table.node(&node_key).is_none() {
                dirty = true;
            }
            let change = discovery_table.apply_sample(
                DiscoverySample::Participant(DiscoveredParticipant {
                    guid: None,
                    participant_id: Some(node.key.participant_id.clone()),
                    ..DiscoveredParticipant::default()
                }),
                observed_at,
            );
            if change == DiscoveryChange::Inserted {
                dirty = true;
            }
            upsert_zenoh_node_sample(node_table, node, observed_at);
        }

        for refs in self.entities.values() {
            let Some(entity_ref) = Self::first_entity_ref(refs) else {
                continue;
            };
            match entity_ref {
                RuntimeZenohEntityRef::Node { .. } => {}
                RuntimeZenohEntityRef::Writer {
                    key,
                    endpoint_id,
                    endpoint_gid,
                    topic_name,
                    type_name,
                    type_hash,
                    qos,
                    ..
                } => {
                    if discovery_table.publication_by_id(endpoint_id).is_none() {
                        dirty = true;
                    }
                    let _ = discovery_table.apply_sample(
                        DiscoverySample::Publication(zenoh_endpoint_sample(
                            endpoint_id.clone(),
                            *endpoint_gid,
                            key.participant_id.clone(),
                            topic_name,
                            type_name,
                            type_hash,
                            qos,
                        )),
                        observed_at,
                    );
                }
                RuntimeZenohEntityRef::Reader {
                    key,
                    endpoint_id,
                    endpoint_gid,
                    topic_name,
                    type_name,
                    type_hash,
                    qos,
                    ..
                } => {
                    if discovery_table.subscription_by_id(endpoint_id).is_none() {
                        dirty = true;
                    }
                    let _ = discovery_table.apply_sample(
                        DiscoverySample::Subscription(zenoh_endpoint_sample(
                            endpoint_id.clone(),
                            *endpoint_gid,
                            key.participant_id.clone(),
                            topic_name,
                            type_name,
                            type_hash,
                            qos,
                        )),
                        observed_at,
                    );
                }
            }
        }

        dirty
    }
}

fn configure_zenoh_port_maps(
    ebpf: &mut Ebpf,
    zenoh_ports: &ZenohCapturePorts,
) -> anyhow::Result<()> {
    if zenoh_ports.udp_ports().len() > MAX_ZENOH_PORTS as usize {
        bail!(
            "configured {} Zenoh UDP ports, exceeding eBPF map capacity {}",
            zenoh_ports.udp_ports().len(),
            MAX_ZENOH_PORTS
        );
    }
    if zenoh_ports.tcp_ports().len() > MAX_ZENOH_PORTS as usize {
        bail!(
            "configured {} Zenoh TCP ports, exceeding eBPF map capacity {}",
            zenoh_ports.tcp_ports().len(),
            MAX_ZENOH_PORTS
        );
    }

    let mut udp_ports = BpfHashMap::<MapData, u16, u8>::try_from(
        ebpf.take_map(ZENOH_UDP_PORTS_MAP)
            .context("take ZENOH_UDP_PORTS map")?,
    )
    .context("open ZENOH_UDP_PORTS map")?;
    for port in zenoh_ports.udp_ports() {
        udp_ports
            .insert(*port, 1, 0)
            .with_context(|| format!("insert Zenoh UDP port {port}"))?;
    }

    let mut tcp_ports = BpfHashMap::<MapData, u16, u8>::try_from(
        ebpf.take_map(ZENOH_TCP_PORTS_MAP)
            .context("take ZENOH_TCP_PORTS map")?,
    )
    .context("open ZENOH_TCP_PORTS map")?;
    for port in zenoh_ports.tcp_ports() {
        tcp_ports
            .insert(*port, 1, 0)
            .with_context(|| format!("insert Zenoh TCP port {port}"))?;
    }

    Ok(())
}

fn upsert_zenoh_node_sample(
    node_table: &mut NodeTable,
    node: &RuntimeZenohNode,
    observed_at: std::time::SystemTime,
) {
    node_table.upsert_sample(
        NodeSample {
            participant_gid: None,
            participant_id: Some(node.key.participant_id.clone()),
            node_namespace: node.key.node_namespace.clone(),
            node_name: node.key.node_name.clone(),
            writer_gids: node.writer_gids.iter().copied().collect(),
            reader_gids: node.reader_gids.iter().copied().collect(),
            writer_endpoint_ids: node.writer_endpoint_ids.iter().cloned().collect(),
            reader_endpoint_ids: node.reader_endpoint_ids.iter().cloned().collect(),
        },
        observed_at,
    );
}

fn discovery_sample_exists(discovery_table: &DiscoveryTable, sample: &DiscoverySample) -> bool {
    match sample {
        DiscoverySample::Publication(endpoint) => endpoint
            .endpoint_id
            .as_ref()
            .is_some_and(|id| discovery_table.publication_by_id(id).is_some()),
        DiscoverySample::Subscription(endpoint) => endpoint
            .endpoint_id
            .as_ref()
            .is_some_and(|id| discovery_table.subscription_by_id(id).is_some()),
        DiscoverySample::Participant(participant) => participant
            .participant_id
            .as_ref()
            .is_some_and(|id| discovery_table.participant_by_id(id).is_some()),
        DiscoverySample::UnknownBuiltin(_) | DiscoverySample::Disposed { .. } => false,
    }
}

fn zenoh_endpoint_sample(
    endpoint_id: EndpointId,
    endpoint_gid: ros2probe_common::TopicGid,
    participant_id: ParticipantId,
    topic_name: &str,
    type_name: &str,
    type_hash: &str,
    qos: &str,
) -> DiscoveredEndpoint {
    DiscoveredEndpoint {
        endpoint_gid: Some(endpoint_gid),
        endpoint_id: Some(endpoint_id),
        participant_gid: None,
        participant_id: Some(participant_id),
        topic_name: Some(topic_name.to_string()),
        type_name: Some(type_name.to_string()),
        type_hash: Some(type_hash.to_string()),
        history: zenoh_history_qos(qos),
        reliability: zenoh_reliability_qos(qos),
        durability: zenoh_durability_qos(qos),
        deadline: zenoh_deadline_qos(qos),
        lifespan: zenoh_lifespan_qos(qos),
        liveliness: zenoh_liveliness_qos(qos),
        liveliness_lease_duration: zenoh_liveliness_lease_duration_qos(qos),
        ..DiscoveredEndpoint::default()
    }
}

fn zenoh_history_qos(qos: &str) -> Option<discovery::HistoryQos> {
    let profile = parse_zenoh_qos(qos)?;
    Some(discovery::HistoryQos {
        kind: profile.history,
        depth: profile.depth,
    })
}

fn zenoh_reliability_qos(qos: &str) -> Option<discovery::ReliabilityQos> {
    let profile = parse_zenoh_qos(qos)?;
    Some(discovery::ReliabilityQos {
        kind: match profile.reliability {
            1 => 2, // RMW reliable -> DDS RELIABLE
            2 => 1, // RMW best effort -> DDS BEST_EFFORT
            _ => profile.reliability,
        },
        max_blocking_time: discovery::DurationValue {
            seconds: 0,
            fraction: 0,
        },
    })
}

fn zenoh_durability_qos(qos: &str) -> Option<discovery::DurabilityQos> {
    let profile = parse_zenoh_qos(qos)?;
    Some(discovery::DurabilityQos {
        kind: match profile.durability {
            1 => 1, // RMW transient local -> DDS TRANSIENT_LOCAL
            2 => 0, // RMW volatile -> DDS VOLATILE
            _ => profile.durability,
        },
    })
}

fn zenoh_deadline_qos(qos: &str) -> Option<discovery::DurationValue> {
    parse_zenoh_qos(qos).map(|profile| profile.deadline)
}

fn zenoh_lifespan_qos(qos: &str) -> Option<discovery::DurationValue> {
    parse_zenoh_qos(qos).map(|profile| profile.lifespan)
}

fn zenoh_liveliness_qos(qos: &str) -> Option<discovery::LivelinessQos> {
    let profile = parse_zenoh_qos(qos)?;
    Some(discovery::LivelinessQos {
        kind: match profile.liveliness {
            1 => 0, // RMW automatic -> DDS AUTOMATIC
            2 => 1, // deprecated RMW manual by node -> DDS MANUAL_BY_PARTICIPANT
            3 => 2, // RMW manual by topic -> DDS MANUAL_BY_TOPIC
            _ => profile.liveliness,
        },
        lease_duration: profile.liveliness_lease_duration,
    })
}

fn zenoh_liveliness_lease_duration_qos(qos: &str) -> Option<discovery::DurationValue> {
    parse_zenoh_qos(qos).map(|profile| profile.liveliness_lease_duration)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ZenohQosProfile {
    history: u32,
    depth: u32,
    reliability: u32,
    durability: u32,
    deadline: discovery::DurationValue,
    lifespan: discovery::DurationValue,
    liveliness: u32,
    liveliness_lease_duration: discovery::DurationValue,
}

fn parse_zenoh_qos(qos: &str) -> Option<ZenohQosProfile> {
    let parts = qos.split(':').collect::<Vec<_>>();
    if parts.len() < 6 {
        return None;
    }

    let history_parts = split_zenoh_qos_component(parts[2], 2)?;
    let deadline_parts = split_zenoh_qos_component(parts[3], 2)?;
    let lifespan_parts = split_zenoh_qos_component(parts[4], 2)?;
    let liveliness_parts = split_zenoh_qos_component(parts[5], 3)?;

    Some(ZenohQosProfile {
        history: parse_zenoh_qos_u32(history_parts[0], 1)?,
        depth: parse_zenoh_qos_u32(history_parts[1], 42)?,
        reliability: parse_zenoh_qos_u32(parts[0], 1)?,
        durability: parse_zenoh_qos_u32(parts[1], 2)?,
        deadline: parse_zenoh_qos_duration(deadline_parts[0], deadline_parts[1])?,
        lifespan: parse_zenoh_qos_duration(lifespan_parts[0], lifespan_parts[1])?,
        liveliness: parse_zenoh_qos_u32(liveliness_parts[0], 1)?,
        liveliness_lease_duration: parse_zenoh_qos_duration(
            liveliness_parts[1],
            liveliness_parts[2],
        )?,
    })
}

fn split_zenoh_qos_component(component: &str, min_parts: usize) -> Option<Vec<&str>> {
    let parts = component.split(',').collect::<Vec<_>>();
    (parts.len() >= min_parts).then_some(parts)
}

fn parse_zenoh_qos_u32(value: &str, default: u32) -> Option<u32> {
    if value.is_empty() {
        Some(default)
    } else {
        value.parse::<u32>().ok()
    }
}

fn parse_zenoh_qos_duration(seconds: &str, fraction: &str) -> Option<discovery::DurationValue> {
    const RMW_DURATION_INFINITE_SECONDS: i64 = 9_223_372_036;
    const RMW_DURATION_INFINITE_NANOS: u32 = 854_775_807;

    let seconds = parse_zenoh_qos_seconds(seconds, RMW_DURATION_INFINITE_SECONDS)?;
    let fraction = parse_zenoh_qos_fraction(fraction, RMW_DURATION_INFINITE_NANOS)?;
    if seconds >= i64::from(i32::MAX) {
        Some(discovery::DurationValue {
            seconds: i32::MAX,
            fraction: u32::MAX,
        })
    } else {
        Some(discovery::DurationValue {
            seconds: i32::try_from(seconds).ok()?,
            fraction,
        })
    }
}

fn parse_zenoh_qos_seconds(value: &str, default: i64) -> Option<i64> {
    let seconds = if value.is_empty() {
        default
    } else {
        value.parse::<i64>().ok()?
    };
    (seconds >= 0).then_some(seconds)
}

fn parse_zenoh_qos_fraction(value: &str, default: u32) -> Option<u32> {
    if value.is_empty() {
        Some(default)
    } else {
        value.parse::<u32>().ok()
    }
}

fn resolve_zenoh_topic_sample(
    sample: ZenohUnresolvedTopicSample,
    discovery_table: &DiscoveryTable,
) -> Option<ZenohRosTopicSample> {
    let endpoint = discovery_table.publication(&sample.identity.source_gid)?;
    let EndpointId::Zenoh { domain_id, .. } = &endpoint.endpoint_id else {
        return None;
    };
    let topic_name = endpoint.topic_name.clone()?;
    let type_name = endpoint.type_name.clone()?;
    let type_hash = endpoint.type_hash.clone()?;

    Some(ZenohRosTopicSample {
        keyexpr: rmw_zenoh_topic_keyexpr(*domain_id, &topic_name, &type_name, &type_hash),
        domain_id: *domain_id,
        topic_name,
        type_name,
        type_hash,
        payload: sample.payload,
        payload_len: sample.payload_len,
        identity: Some(sample.identity),
        attachment_len: sample.attachment_len,
    })
}

#[allow(clippy::too_many_arguments)]
fn handle_zenoh_topic_sample(
    sample: ZenohRosTopicSample,
    observed_at: std::time::SystemTime,
    recording_session: Option<&mut RecordingSession>,
    topic_bw_session: Option<&mut TopicBwSession>,
    topic_delay_session: Option<&mut TopicDelaySession>,
    topic_echo_session: Option<&mut TopicEchoSession>,
    topic_hz_session: Option<&mut TopicHzSession>,
    recent_zenoh_samples: &mut RecentZenohSampleCache,
    recorder_handle: &RecorderHandle,
) {
    if !zenoh_topic_is_interested(
        &sample.topic_name,
        recording_session.as_deref(),
        topic_bw_session.as_deref(),
        topic_delay_session.as_deref(),
        topic_echo_session.as_deref(),
        topic_hz_session.as_deref(),
    ) {
        trace!(
            "skip uninterested Zenoh sample topic={} type={}",
            sample.topic_name, sample.type_name
        );
        return;
    }
    if let Some(identity) = sample.identity {
        if !recent_zenoh_samples.insert(identity) {
            trace!(
                "skip duplicate Zenoh sample topic={} type={} source_gid={:?} seq={}",
                sample.topic_name, sample.type_name, identity.source_gid, identity.sequence_number
            );
            return;
        }
    }
    observe_zenoh_topic_sample(
        &sample,
        observed_at,
        recording_session,
        topic_bw_session,
        topic_delay_session,
        topic_echo_session,
        topic_hz_session,
        recorder_handle,
    );
}

fn observe_zenoh_topic_sample(
    sample: &ZenohRosTopicSample,
    observed_at: std::time::SystemTime,
    recording_session: Option<&mut RecordingSession>,
    topic_bw_session: Option<&mut TopicBwSession>,
    topic_delay_session: Option<&mut TopicDelaySession>,
    topic_echo_session: Option<&mut TopicEchoSession>,
    topic_hz_session: Option<&mut TopicHzSession>,
    recorder_handle: &RecorderHandle,
) {
    if let Some(session) = recording_session {
        let normalized_type_name = normalize_zenoh_sample_type_name(&sample.type_name);
        bag::record_topic_sample(
            session,
            recorder_handle,
            &sample.topic_name,
            Some(&normalized_type_name),
            observed_at,
            sample.payload.clone(),
        );
    }
    let normalized_type_name = normalize_zenoh_sample_type_name(&sample.type_name);
    observers::bw_observe_topic_sample(topic_bw_session, &sample.topic_name, sample.payload_len);
    observers::delay_observe_topic_sample(
        topic_delay_session,
        &sample.topic_name,
        observed_at,
        &sample.payload,
    );
    observers::echo_observe_topic_sample(
        topic_echo_session,
        &sample.topic_name,
        Some(&normalized_type_name),
        &sample.payload,
    );
    observers::hz_observe_topic_sample(topic_hz_session, &sample.topic_name);
}

fn zenoh_topic_is_interested(
    topic_name: &str,
    recording_session: Option<&RecordingSession>,
    topic_bw_session: Option<&TopicBwSession>,
    topic_delay_session: Option<&TopicDelaySession>,
    topic_echo_session: Option<&TopicEchoSession>,
    topic_hz_session: Option<&TopicHzSession>,
) -> bool {
    if recording_session.is_some_and(|session| {
        session.topics.is_empty() || session.topics.iter().any(|topic| topic == topic_name)
    }) {
        return true;
    }

    topic_bw_session.is_some_and(|session| session.topic_name() == topic_name)
        || topic_delay_session.is_some_and(|session| session.topic_name() == topic_name)
        || topic_echo_session.is_some_and(|session| session.topic_name() == topic_name)
        || topic_hz_session.is_some_and(|session| session.topic_name() == topic_name)
}

fn normalize_zenoh_sample_type_name(type_name: &str) -> String {
    if type_name.contains('/') {
        return type_name.to_string();
    }

    let Some((package, remainder)) = type_name.split_once("::") else {
        return type_name.to_string();
    };
    let (kind, remainder) = if let Some(rest) = remainder.strip_prefix("msg::dds_::") {
        ("msg", rest)
    } else if let Some(rest) = remainder.strip_prefix("srv::dds_::") {
        ("srv", rest)
    } else if let Some(rest) = remainder.strip_prefix("action::dds_::") {
        ("action", rest)
    } else {
        return type_name.to_string();
    };

    let type_name = remainder.strip_suffix('_').unwrap_or(remainder);
    format!("{package}/{kind}/{type_name}")
}

fn zenoh_participant_id(entity: &ZenohRosLivelinessEntity) -> ParticipantId {
    ParticipantId::Zenoh {
        domain_id: entity.node.domain_id,
        zid: entity.node.zid.clone(),
        node_id: entity.node.node_id.clone(),
    }
}

fn zenoh_endpoint_id(entity: &ZenohRosLivelinessEntity) -> EndpointId {
    EndpointId::Zenoh {
        domain_id: entity.node.domain_id,
        zid: entity.node.zid.clone(),
        node_id: entity.node.node_id.clone(),
        entity_id: entity.node.entity_id.clone(),
        kind: zenoh_entity_kind_tag(entity.kind).to_string(),
    }
}

fn zenoh_entity_kind_tag(kind: ZenohRosEntityKind) -> &'static str {
    match kind {
        ZenohRosEntityKind::Node => "NN",
        ZenohRosEntityKind::Publisher => "MP",
        ZenohRosEntityKind::Subscription => "MS",
        ZenohRosEntityKind::ServiceServer => "SS",
        ZenohRosEntityKind::ServiceClient => "SC",
    }
}

fn handle_zenoh_event(
    event: ZenohEvent,
    zenoh_graph: &mut RuntimeZenohGraph,
    discovery_table: &mut DiscoveryTable,
    node_table: &mut NodeTable,
    mut recording_session: Option<&mut RecordingSession>,
    mut topic_bw_session: Option<&mut TopicBwSession>,
    mut topic_delay_session: Option<&mut TopicDelaySession>,
    mut topic_echo_session: Option<&mut TopicEchoSession>,
    mut topic_hz_session: Option<&mut TopicHzSession>,
    recent_zenoh_samples: &mut RecentZenohSampleCache,
    zenoh_shm_topics: &mut BTreeSet<String>,
    remote_participants: &Arc<Mutex<HashSet<ParticipantId>>>,
    local_ips: &LocalIps,
    recorder_handle: &RecorderHandle,
) -> bool {
    let mut dirty = false;
    let ZenohEvent::Batch(batch) = event;
    let flow = RuntimeZenohFlowKey {
        protocol: batch.protocol,
        flow: batch.flow,
    };
    zenoh_graph.touch_flow(flow, batch.socket_timestamp);

    for event in batch.semantic_events {
        match event {
            ZenohSemanticEvent::RosEntityDiscovered(entity) => {
                let participant_id = zenoh_participant_id(&entity);
                if !local_ips.contains(&batch.flow.src_ip) {
                    remote_participants
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(participant_id);
                }
                if zenoh_graph.insert_entity(
                    flow,
                    entity,
                    batch.socket_timestamp,
                    discovery_table,
                    node_table,
                ) {
                    dirty = true;
                }
            }
            ZenohSemanticEvent::RosEntityUndiscovered(entity) => {
                if zenoh_graph.remove_entity_from_flow(
                    flow,
                    &entity.keyexpr,
                    batch.socket_timestamp,
                    discovery_table,
                    node_table,
                ) {
                    dirty = true;
                }
            }
            ZenohSemanticEvent::TopicSample(sample) => {
                handle_zenoh_topic_sample(
                    sample,
                    batch.socket_timestamp,
                    recording_session.as_deref_mut(),
                    topic_bw_session.as_deref_mut(),
                    topic_delay_session.as_deref_mut(),
                    topic_echo_session.as_deref_mut(),
                    topic_hz_session.as_deref_mut(),
                    recent_zenoh_samples,
                    recorder_handle,
                );
            }
            ZenohSemanticEvent::UnresolvedTopicSample(sample) => {
                let source_gid = sample.identity.source_gid;
                let scope = sample.wire_expr.scope;
                let Some(sample) = resolve_zenoh_topic_sample(sample, discovery_table) else {
                    trace!(
                        "skip unresolved Zenoh sample scope={} source_gid={:?}: publisher not discovered",
                        scope, source_gid
                    );
                    continue;
                };
                handle_zenoh_topic_sample(
                    sample,
                    batch.socket_timestamp,
                    recording_session.as_deref_mut(),
                    topic_bw_session.as_deref_mut(),
                    topic_delay_session.as_deref_mut(),
                    topic_echo_session.as_deref_mut(),
                    topic_hz_session.as_deref_mut(),
                    recent_zenoh_samples,
                    recorder_handle,
                );
            }
            ZenohSemanticEvent::ShmTopicSample {
                topic_name,
                identity,
            } => {
                let topic_name = topic_name.or_else(|| {
                    let source_gid = identity?.source_gid;
                    discovery_table
                        .publication(&source_gid)
                        .and_then(|endpoint| endpoint.topic_name.clone())
                });
                if let Some(topic_name) = topic_name
                    && zenoh_shm_topics.insert(topic_name.clone())
                {
                    debug!("detected Zenoh SHM payload for topic={topic_name}");
                }
            }
        }
    }

    dirty
}

#[allow(clippy::too_many_arguments)]
fn active_zenoh_shadow_keyexprs(
    recording_session: Option<&RecordingSession>,
    topic_bw_session: Option<&TopicBwSession>,
    topic_delay_session: Option<&TopicDelaySession>,
    topic_echo_session: Option<&TopicEchoSession>,
    topic_hz_session: Option<&TopicHzSession>,
    discovery_table: &DiscoveryTable,
    zenoh_shm_topics: &BTreeSet<String>,
) -> BTreeSet<String> {
    let observe_all = recording_session.is_some_and(|session| session.topics.is_empty());
    let mut active_topics = BTreeSet::new();
    if let Some(session) = recording_session {
        active_topics.extend(session.topics.iter().cloned());
    }
    if let Some(session) = topic_bw_session {
        active_topics.insert(session.topic_name().to_string());
    }
    if let Some(session) = topic_delay_session {
        active_topics.insert(session.topic_name().to_string());
    }
    if let Some(session) = topic_echo_session {
        active_topics.insert(session.topic_name().to_string());
    }
    if let Some(session) = topic_hz_session {
        active_topics.insert(session.topic_name().to_string());
    }

    discovery_table
        .publications()
        .values()
        .filter_map(|endpoint| {
            let EndpointId::Zenoh { domain_id, .. } = &endpoint.endpoint_id else {
                return None;
            };
            let topic_name = endpoint.topic_name.as_deref()?;
            if !zenoh_shm_topics.contains(topic_name)
                || (!observe_all && !active_topics.contains(topic_name))
            {
                return None;
            }
            Some(rmw_zenoh_topic_keyexpr(
                *domain_id,
                topic_name,
                endpoint.type_name.as_deref()?,
                endpoint.type_hash.as_deref()?,
            ))
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn drain_zenoh_shadow_samples(
    sample_rx: &mpsc::Receiver<ZenohShadowSample>,
    mut recording_session: Option<&mut RecordingSession>,
    mut topic_bw_session: Option<&mut TopicBwSession>,
    mut topic_delay_session: Option<&mut TopicDelaySession>,
    mut topic_echo_session: Option<&mut TopicEchoSession>,
    mut topic_hz_session: Option<&mut TopicHzSession>,
    recent_zenoh_samples: &mut RecentZenohSampleCache,
    recorder_handle: &RecorderHandle,
) {
    while let Ok(sample) = sample_rx.try_recv() {
        handle_zenoh_topic_sample(
            sample.sample,
            sample.observed_at,
            recording_session.as_deref_mut(),
            topic_bw_session.as_deref_mut(),
            topic_delay_session.as_deref_mut(),
            topic_echo_session.as_deref_mut(),
            topic_hz_session.as_deref_mut(),
            recent_zenoh_samples,
            recorder_handle,
        );
    }
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
    zenoh_graph: &mut RuntimeZenohGraph,
    recent_samples: &mut RecentSampleCache,
    recent_zenoh_samples: &mut RecentZenohSampleCache,
    observed_zenoh_transports: &mut HashSet<TransportProtocol>,
    zenoh_shm_topics: &mut BTreeSet<String>,
    topic_list_state: &SharedState,
    remote_participants: &Arc<Mutex<HashSet<ParticipantId>>>,
    local_ips: &LocalIps,
    recorder_handle: &RecorderHandle,
) -> anyhow::Result<()> {
    let mut dirty = false;
    while let Ok(batch) = capture_event_rx.try_recv() {
        if !batch.zenoh_events.is_empty() {
            trace!("captured {} Zenoh batch event(s)", batch.zenoh_events.len());
        }
        for event in batch.zenoh_events {
            let ZenohEvent::Batch(zenoh_batch) = &event;
            observed_zenoh_transports.insert(zenoh_batch.protocol);
            let event_dirty = handle_zenoh_event(
                event,
                zenoh_graph,
                discovery_table,
                node_table,
                recording_session.as_deref_mut(),
                topic_bw_session.as_deref_mut(),
                topic_delay_session.as_deref_mut(),
                topic_echo_session.as_deref_mut(),
                topic_hz_session.as_deref_mut(),
                recent_zenoh_samples,
                zenoh_shm_topics,
                remote_participants,
                local_ips,
                recorder_handle,
            );
            if event_dirty {
                dirty = true;
            }
        }
        for event in batch.events {
            let event_dirty = handle_rtps_event(
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
            )?;
            if event_dirty {
                dirty = true;
            }
        }
    }

    // The eBPF filter is synced inline in handle_rtps_event whenever the GID
    // set actually changes (Inserted / Removed). Here we only do the heavier
    // CommandState rebuild, batching it across whatever the burst contained
    // so a discovery storm doesn't trigger one rebuild per event.
    if dirty {
        state::refresh_from_discovery(
            topic_list_state,
            discovery_table,
            node_table,
            remote_participants,
        );
    }

    Ok(())
}

fn spawn_capture_worker(
    interface: String,
    mut capture: CaptureEngine,
    capture_event_tx: mpsc::SyncSender<CaptureWorkerEvent>,
    stop: Arc<AtomicBool>,
    rtps_fragment_budget: Arc<RtpsFragmentMemoryBudget>,
    zenoh_fragment_budget: Arc<ZenohFragmentMemoryBudget>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut buffer = CaptureBuffer::new(MAX_FRAGMENT_FLOWS as usize);
        // Each worker owns its own RtpsProcessor. DATA_FRAG reassembly state
        // is per-interface, which is correct: IP fragments of a single RTPS
        // sample traverse one NIC, so the fragment flow always terminates on
        // the worker that saw its first chunk.
        let mut rtps =
            RtpsProcessor::with_fragment_budget(MAX_FRAGMENT_FLOWS as usize, rtps_fragment_budget);
        let mut zenoh = ZenohProcessor::with_fragment_budget(
            MAX_FRAGMENT_FLOWS as usize,
            zenoh_fragment_budget,
        );
        let mut backpressure_waits = 0u64;
        let mut last_capture_stats = Instant::now();
        while !stop.load(Ordering::Relaxed) {
            match capture.pump_once_blocking(&mut buffer) {
                Ok(()) => {
                    if last_capture_stats.elapsed() >= CAPTURE_STATS_INTERVAL {
                        match capture.socket_stats() {
                            Ok(stats) if stats.drops > 0 || stats.freeze_count > 0 => {
                                warn!(
                                    "AF_PACKET capture pressure on interface {interface}: received={}, dropped={}, frozen={}",
                                    stats.packets, stats.drops, stats.freeze_count
                                );
                            }
                            Ok(_) => {}
                            Err(err) => {
                                debug!(
                                    "failed to read capture statistics on interface {interface}: {err:#}"
                                );
                            }
                        }
                        last_capture_stats = Instant::now();
                    }
                    let expired_fragments = zenoh.expire_inactive_fragments();
                    if expired_fragments > 0 {
                        debug!(
                            "expired {expired_fragments} incomplete Zenoh fragment flow(s) on interface={interface}"
                        );
                    }
                    let mut events = Vec::with_capacity(buffer.packets().len());
                    while let Some(packet) = buffer.pop() {
                        if let Ok(pkt_events) = rtps.process_packet(packet) {
                            events.extend(pkt_events);
                        }
                    }
                    let mut zenoh_events = Vec::new();
                    while let Some(packet) = buffer.pop_zenoh() {
                        match zenoh.process_packet(packet) {
                            Ok(pkt_events) => {
                                for event in &pkt_events {
                                    let ZenohEvent::Batch(batch) = event;
                                    debug!(
                                        "zenoh batch interface={} protocol={:?} src_port={} dst_port={} batch_len={} frame_len={} ip_fragments={} was_ip_fragmented={} messages={} batch_head={}",
                                        interface,
                                        batch.protocol,
                                        batch.flow.src_port,
                                        batch.flow.dst_port,
                                        batch.payload.len(),
                                        batch.frame_len,
                                        batch.ip_fragment_count,
                                        batch.was_ip_fragmented,
                                        batch
                                            .messages
                                            .iter()
                                            .map(ToString::to_string)
                                            .collect::<Vec<_>>()
                                            .join(","),
                                        hex_preview(&batch.payload, 16),
                                    );
                                }
                                zenoh_events.extend(pkt_events);
                            }
                            Err(err) => {
                                warn!(
                                    "Zenoh packet processing failed on interface {interface}: {err:#}"
                                );
                            }
                        }
                    }
                    if events.is_empty() && zenoh_events.is_empty() {
                        continue;
                    }
                    let mut pending = CaptureWorkerEvent {
                        events,
                        zenoh_events,
                    };
                    loop {
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                        match capture_event_tx.try_send(pending) {
                            Ok(()) => break,
                            Err(mpsc::TrySendError::Full(event)) => {
                                pending = event;
                                backpressure_waits = backpressure_waits.saturating_add(1);
                                if backpressure_waits.is_power_of_two() {
                                    warn!(
                                        "capture event queue backpressure on interface {interface}; waited {backpressure_waits} time(s) without dropping batches"
                                    );
                                }
                                thread::sleep(Duration::from_millis(1));
                            }
                            Err(mpsc::TrySendError::Disconnected(_)) => return,
                        }
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

fn hex_preview(bytes: &[u8], max_len: usize) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";

    let preview_len = bytes.len().min(max_len);
    let mut out = String::with_capacity(preview_len.saturating_mul(3).saturating_sub(1) + 3);
    for (idx, byte) in bytes.iter().take(preview_len).copied().enumerate() {
        if idx > 0 {
            out.push(' ');
        }
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    if bytes.len() > preview_len {
        out.push_str("...");
    }
    out
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
    let gid_metadata =
        gid_map.metadata_for_message_with_gid(&message.writer_gid, &message.reader_gid);
    let topic_name = gid_metadata
        .map(|(_, metadata)| metadata.topic_name.as_str())
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

    let Some((topic_gid, metadata)) = gid_metadata else {
        return node_table_dirty;
    };

    if let Some(session) = recording_session {
        bag::record_message(session, recorder_handle, &message, topic_gid, metadata);
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
        if self.capacity == 0 {
            return false;
        }
        if self.seen.contains(&sample) {
            return false;
        }

        if self.order.len() >= self.capacity
            && let Some(oldest) = self.order.pop_front()
        {
            self.seen.remove(&oldest);
        }

        self.order.push_back(sample);
        self.seen.insert(sample);
        true
    }
}

impl RecentZenohSampleCache {
    fn new(capacity: usize) -> Self {
        Self {
            seen: HashSet::with_capacity(capacity),
            order: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    fn insert(&mut self, sample: ZenohRosSampleIdentity) -> bool {
        if self.capacity == 0 {
            return false;
        }
        if self.seen.contains(&sample) {
            return false;
        }

        if self.order.len() >= self.capacity
            && let Some(oldest) = self.order.pop_front()
        {
            self.seen.remove(&oldest);
        }

        self.order.push_back(sample);
        self.seen.insert(sample);
        true
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use ros2probe_common::TopicGid;

    use crate::command::protocol::TopicHzStartRequest;
    use crate::protocols::zenoh::{ZenohRosNodeInfo, ZenohRosTopicInfo};

    use super::*;

    #[test]
    fn runtime_zenoh_graph_adds_and_removes_publisher_from_discovery_state() {
        let mut graph = RuntimeZenohGraph::default();
        let mut discovery_table = DiscoveryTable::default();
        let mut node_table = NodeTable::default();
        let entity = zenoh_publisher_entity();
        let endpoint_id = zenoh_endpoint_id(&entity);
        let endpoint_gid = rmw_zenoh_entity_gid(&entity);

        assert!(graph.insert_entity(
            zenoh_flow(32100, 7447),
            entity.clone(),
            SystemTime::UNIX_EPOCH,
            &mut discovery_table,
            &mut node_table,
        ));

        let topic = discovery_table.topic("/chatter").unwrap();
        assert_eq!(topic.publisher_count, 1);
        assert_eq!(topic.subscription_count, 0);
        assert_eq!(topic.type_names, vec!["std_msgs::msg::dds_::String_"]);
        let node = node_table.node_for_writer_id(&endpoint_id).unwrap();
        assert_eq!(node.key.node_namespace, "/");
        assert_eq!(node.key.node_name, "talker");
        assert!(discovery_table.publication_by_id(&endpoint_id).is_some());
        assert_eq!(
            discovery_table
                .publication(&endpoint_gid)
                .map(|entry| &entry.endpoint_id),
            Some(&endpoint_id)
        );
        assert!(node_table.node_for_writer(&endpoint_gid).is_some());
        assert!(node_table.node_for_writer_id(&endpoint_id).is_some());

        assert!(graph.remove_entity_from_flow(
            zenoh_flow(32100, 7447),
            &entity.keyexpr,
            SystemTime::UNIX_EPOCH,
            &mut discovery_table,
            &mut node_table,
        ));
        assert!(discovery_table.topic("/chatter").is_none());
        assert!(node_table.node_for_writer_id(&endpoint_id).is_none());
        assert!(discovery_table.publication_by_id(&endpoint_id).is_none());
        assert!(discovery_table.publication(&endpoint_gid).is_none());
        assert!(node_table.node_for_writer(&endpoint_gid).is_none());
        assert!(node_table.node_for_writer_id(&endpoint_id).is_none());
    }

    #[test]
    fn resolves_unknown_scope_sample_from_discovered_publisher_gid() {
        let mut graph = RuntimeZenohGraph::default();
        let mut discovery_table = DiscoveryTable::default();
        let mut node_table = NodeTable::default();
        let entity = zenoh_publisher_entity();
        let source_gid = rmw_zenoh_entity_gid(&entity);
        graph.insert_entity(
            RuntimeZenohFlowKey::active_discovery(),
            entity,
            SystemTime::UNIX_EPOCH,
            &mut discovery_table,
            &mut node_table,
        );

        let sample = resolve_zenoh_topic_sample(
            ZenohUnresolvedTopicSample {
                wire_expr: crate::protocols::zenoh::ZenohWireExpr {
                    scope: 99,
                    suffix: String::new(),
                    mapping: zenoh_protocol::network::Mapping::Sender,
                },
                payload: bytes::Bytes::from_static(b"hello"),
                payload_len: 5,
                identity: ZenohRosSampleIdentity {
                    source_gid,
                    sequence_number: 42,
                },
                attachment_len: Some(33),
            },
            &discovery_table,
        )
        .unwrap();

        assert_eq!(sample.topic_name, "/chatter");
        assert_eq!(sample.type_name, "std_msgs::msg::dds_::String_");
        assert_eq!(sample.type_hash, "RIHS01_abcd");
        assert_eq!(
            sample.keyexpr,
            "0/chatter/std_msgs::msg::dds_::String_/RIHS01_abcd"
        );
        assert_eq!(sample.payload.as_ref(), b"hello");
        assert_eq!(sample.identity.unwrap().source_gid, source_gid);
    }

    #[test]
    fn recent_zenoh_sample_cache_dedups_by_source_gid_and_sequence() {
        let mut cache = RecentZenohSampleCache::new(16);

        assert!(cache.insert(zenoh_sample_identity(1, 42)));
        assert!(!cache.insert(zenoh_sample_identity(1, 42)));
        assert!(cache.insert(zenoh_sample_identity(2, 42)));
        assert!(cache.insert(zenoh_sample_identity(1, 43)));
    }

    #[test]
    fn recent_zenoh_sample_cache_evicts_oldest_identity() {
        let mut cache = RecentZenohSampleCache::new(2);

        assert!(cache.insert(zenoh_sample_identity(1, 1)));
        assert!(cache.insert(zenoh_sample_identity(1, 2)));
        assert!(cache.insert(zenoh_sample_identity(1, 3)));
        assert!(cache.insert(zenoh_sample_identity(1, 1)));
    }

    #[test]
    fn zenoh_topic_interest_is_empty_without_active_consumers() {
        assert!(!zenoh_topic_is_interested(
            "/chatter", None, None, None, None, None
        ));
    }

    #[test]
    fn zenoh_topic_interest_matches_active_topic_session() {
        let mut hz_session = None;
        observers::hz_start_session(
            TopicHzStartRequest {
                topic_name: "/chatter".to_string(),
                window_size: 10,
            },
            &mut hz_session,
        )
        .unwrap();

        assert!(zenoh_topic_is_interested(
            "/chatter",
            None,
            None,
            None,
            None,
            hz_session.as_ref(),
        ));
        assert!(!zenoh_topic_is_interested(
            "/other",
            None,
            None,
            None,
            None,
            hz_session.as_ref(),
        ));
    }

    #[test]
    fn zenoh_shadow_targets_only_active_topics_with_shm_samples() {
        let mut graph = RuntimeZenohGraph::default();
        let mut discovery_table = DiscoveryTable::default();
        let mut node_table = NodeTable::default();
        graph.insert_entity(
            zenoh_flow(32100, 7447),
            zenoh_publisher_entity(),
            SystemTime::UNIX_EPOCH,
            &mut discovery_table,
            &mut node_table,
        );
        let mut hz_session = None;
        observers::hz_start_session(
            TopicHzStartRequest {
                topic_name: "/chatter".to_string(),
                window_size: 10,
            },
            &mut hz_session,
        )
        .unwrap();

        assert!(
            active_zenoh_shadow_keyexprs(
                None,
                None,
                None,
                None,
                hz_session.as_ref(),
                &discovery_table,
                &BTreeSet::new(),
            )
            .is_empty()
        );

        let keyexprs = active_zenoh_shadow_keyexprs(
            None,
            None,
            None,
            None,
            hz_session.as_ref(),
            &discovery_table,
            &BTreeSet::from(["/chatter".to_string()]),
        );
        assert_eq!(
            keyexprs,
            BTreeSet::from(["0/chatter/std_msgs::msg::dds_::String_/RIHS01_abcd".to_string()])
        );
    }

    #[test]
    fn runtime_zenoh_graph_refresh_keeps_liveliness_endpoints_alive() {
        let mut graph = RuntimeZenohGraph::default();
        let mut discovery_table = DiscoveryTable::new(Duration::from_secs(1));
        let mut node_table = NodeTable::default();
        let entity = zenoh_publisher_entity();

        graph.insert_entity(
            zenoh_flow(32100, 7447),
            entity,
            SystemTime::UNIX_EPOCH,
            &mut discovery_table,
            &mut node_table,
        );
        assert!(discovery_table.topic("/chatter").is_some());

        let later = SystemTime::UNIX_EPOCH + Duration::from_secs(2);
        assert!(!graph.refresh_discovery(later, &mut discovery_table, &mut node_table));
        let expire_stats = discovery_table.expire_stale(later);

        assert_eq!(expire_stats.publications_removed, 0);
        assert!(discovery_table.topic("/chatter").is_some());
    }

    #[test]
    fn runtime_zenoh_graph_expires_entities_when_flow_is_inactive() {
        let mut graph = RuntimeZenohGraph::default();
        let mut discovery_table = DiscoveryTable::default();
        let mut node_table = NodeTable::default();
        let entity = zenoh_publisher_entity();
        let endpoint_id = zenoh_endpoint_id(&entity);

        graph.insert_entity(
            zenoh_flow(32100, 7447),
            entity,
            SystemTime::UNIX_EPOCH,
            &mut discovery_table,
            &mut node_table,
        );
        assert!(discovery_table.topic("/chatter").is_some());

        let later = SystemTime::UNIX_EPOCH + Duration::from_secs(91);
        assert!(graph.expire_inactive_flows(
            later,
            Duration::from_secs(90),
            &mut discovery_table,
            &mut node_table,
        ));

        assert!(discovery_table.topic("/chatter").is_none());
        assert!(node_table.node_for_writer_id(&endpoint_id).is_none());
    }

    #[test]
    fn runtime_zenoh_graph_touch_flow_prevents_inactivity_expiration() {
        let mut graph = RuntimeZenohGraph::default();
        let mut discovery_table = DiscoveryTable::default();
        let mut node_table = NodeTable::default();
        let flow = zenoh_flow(32100, 7447);

        graph.insert_entity(
            flow,
            zenoh_publisher_entity(),
            SystemTime::UNIX_EPOCH,
            &mut discovery_table,
            &mut node_table,
        );
        graph.touch_flow(flow, SystemTime::UNIX_EPOCH + Duration::from_secs(45));

        let later = SystemTime::UNIX_EPOCH + Duration::from_secs(91);
        assert!(!graph.expire_inactive_flows(
            later,
            Duration::from_secs(90),
            &mut discovery_table,
            &mut node_table,
        ));
        assert!(discovery_table.topic("/chatter").is_some());
    }

    #[test]
    fn runtime_zenoh_graph_active_discovery_flow_does_not_expire() {
        let mut graph = RuntimeZenohGraph::default();
        let mut discovery_table = DiscoveryTable::default();
        let mut node_table = NodeTable::default();
        let flow = RuntimeZenohFlowKey::active_discovery();

        graph.insert_entity(
            flow,
            zenoh_publisher_entity(),
            SystemTime::UNIX_EPOCH,
            &mut discovery_table,
            &mut node_table,
        );
        graph.touch_flow(flow, SystemTime::UNIX_EPOCH);

        let later = SystemTime::UNIX_EPOCH + Duration::from_secs(3600);
        assert!(!graph.expire_inactive_flows(
            later,
            Duration::from_secs(90),
            &mut discovery_table,
            &mut node_table,
        ));
        assert!(discovery_table.topic("/chatter").is_some());
    }

    #[test]
    fn runtime_zenoh_graph_keeps_active_discovery_ref_when_query_flow_closes() {
        let mut graph = RuntimeZenohGraph::default();
        let mut discovery_table = DiscoveryTable::default();
        let mut node_table = NodeTable::default();
        let active_flow = RuntimeZenohFlowKey::active_discovery();
        let query_flow = zenoh_flow(40000, 7447);
        let entity = zenoh_publisher_entity();
        let endpoint_id = zenoh_endpoint_id(&entity);

        assert!(graph.insert_entity(
            active_flow,
            entity.clone(),
            SystemTime::UNIX_EPOCH,
            &mut discovery_table,
            &mut node_table,
        ));
        assert!(!graph.insert_entity(
            query_flow,
            entity.clone(),
            SystemTime::UNIX_EPOCH,
            &mut discovery_table,
            &mut node_table,
        ));

        assert!(!graph.remove_entity_from_flow(
            query_flow,
            &entity.keyexpr,
            SystemTime::UNIX_EPOCH + Duration::from_secs(1),
            &mut discovery_table,
            &mut node_table,
        ));
        assert!(discovery_table.topic("/chatter").is_some());
        assert!(node_table.node_for_writer_id(&endpoint_id).is_some());

        assert!(graph.remove_entity_from_flow(
            active_flow,
            &entity.keyexpr,
            SystemTime::UNIX_EPOCH + Duration::from_secs(2),
            &mut discovery_table,
            &mut node_table,
        ));
        assert!(discovery_table.topic("/chatter").is_none());
        assert!(node_table.node_for_writer_id(&endpoint_id).is_none());
    }

    #[test]
    fn discover_auto_runs_graph_middlewares() {
        assert_eq!(discover_methods(DiscoverMode::Auto), (true, true));
        assert_eq!(discover_methods(DiscoverMode::Rtps), (true, false));
        assert_eq!(discover_methods(DiscoverMode::Zenoh), (false, true));
    }

    #[test]
    fn zenoh_sample_type_names_are_normalized_for_observers() {
        assert_eq!(
            normalize_zenoh_sample_type_name("std_msgs::msg::dds_::String_"),
            "std_msgs/msg/String"
        );
        assert_eq!(
            normalize_zenoh_sample_type_name("example_interfaces::srv::dds_::AddTwoInts_Request_"),
            "example_interfaces/srv/AddTwoInts_Request"
        );
        assert_eq!(
            normalize_zenoh_sample_type_name("std_msgs/msg/String"),
            "std_msgs/msg/String"
        );
    }

    #[test]
    fn zenoh_qos_keyexpr_maps_to_discovery_qos() {
        let qos = "2:1:1,7:5,7:60,3000:3,8,9";

        assert_eq!(
            zenoh_history_qos(qos).unwrap(),
            discovery::HistoryQos { kind: 1, depth: 7 }
        );
        assert_eq!(zenoh_reliability_qos(qos).unwrap().kind, 1);
        assert_eq!(zenoh_durability_qos(qos).unwrap().kind, 1);
        assert_eq!(
            zenoh_deadline_qos(qos).unwrap(),
            discovery::DurationValue {
                seconds: 5,
                fraction: 7
            }
        );
        assert_eq!(
            zenoh_lifespan_qos(qos).unwrap(),
            discovery::DurationValue {
                seconds: 60,
                fraction: 3000
            }
        );
        let liveliness = zenoh_liveliness_qos(qos).unwrap();
        assert_eq!(liveliness.kind, 2);
        assert_eq!(
            zenoh_liveliness_lease_duration_qos(qos).unwrap(),
            discovery::DurationValue {
                seconds: 8,
                fraction: 9
            }
        );
        assert_eq!(
            liveliness.lease_duration,
            discovery::DurationValue {
                seconds: 8,
                fraction: 9
            }
        );
    }

    #[test]
    fn zenoh_qos_keyexpr_fills_rmw_zenoh_defaults() {
        let qos = "::,:,:,:,,";

        assert_eq!(
            zenoh_history_qos(qos).unwrap(),
            discovery::HistoryQos { kind: 1, depth: 42 }
        );
        assert_eq!(zenoh_reliability_qos(qos).unwrap().kind, 2);
        assert_eq!(zenoh_durability_qos(qos).unwrap().kind, 0);
        assert_eq!(
            zenoh_deadline_qos(qos).unwrap(),
            discovery::DurationValue {
                seconds: i32::MAX,
                fraction: u32::MAX
            }
        );
        let liveliness = zenoh_liveliness_qos(qos).unwrap();
        assert_eq!(liveliness.kind, 0);
        assert_eq!(
            zenoh_liveliness_lease_duration_qos(qos).unwrap(),
            discovery::DurationValue {
                seconds: i32::MAX,
                fraction: u32::MAX
            }
        );
    }

    #[test]
    fn zenoh_qos_keyexpr_can_report_lease_without_liveliness_policy() {
        let qos = "::,:,:,:,8,9";

        assert_eq!(zenoh_liveliness_qos(qos).unwrap().kind, 0);
        assert_eq!(
            zenoh_liveliness_lease_duration_qos(qos).unwrap(),
            discovery::DurationValue {
                seconds: 8,
                fraction: 9
            }
        );
    }

    fn zenoh_publisher_entity() -> ZenohRosLivelinessEntity {
        ZenohRosLivelinessEntity {
            keyexpr: concat!(
                "@ros2_lv/0/zid/1/32/MP/%/%/talker/",
                "%chatter/std_msgs::msg::dds_::String_/RIHS01_abcd/2:1:1,7:5,7:60,3000:3,8,9"
            )
            .to_string(),
            kind: ZenohRosEntityKind::Publisher,
            node: ZenohRosNodeInfo {
                domain_id: 0,
                zid: "zid".to_string(),
                node_id: "1".to_string(),
                entity_id: "32".to_string(),
                enclave: "/".to_string(),
                namespace: "/".to_string(),
                node_name: "talker".to_string(),
            },
            topic: Some(ZenohRosTopicInfo {
                name: "/chatter".to_string(),
                type_name: "std_msgs::msg::dds_::String_".to_string(),
                type_hash: "RIHS01_abcd".to_string(),
                qos: "2:1:1,7:5,7:60,3000:3,8,9".to_string(),
                backends: Vec::new(),
            }),
        }
    }

    fn zenoh_sample_identity(source_gid: u8, sequence_number: i64) -> ZenohRosSampleIdentity {
        ZenohRosSampleIdentity {
            source_gid: TopicGid::new([source_gid; 16]),
            sequence_number,
        }
    }

    fn zenoh_flow(src_port: u16, dst_port: u16) -> RuntimeZenohFlowKey {
        RuntimeZenohFlowKey {
            protocol: TransportProtocol::Tcp,
            flow: ros2probe_common::FlowTuple::new(
                ros2probe_common::IpAddr::from_v4(u32::from_be_bytes([127, 0, 0, 1])),
                ros2probe_common::IpAddr::from_v4(u32::from_be_bytes([127, 0, 0, 1])),
                src_port,
                dst_port,
            ),
        }
    }
}
