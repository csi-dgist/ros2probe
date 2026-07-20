use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use ros2probe_common::TopicGid;

/// Shared, read-mostly snapshot of the ROS graph state.
///
/// Readers (command server handlers, GUI pollers) acquire a snapshot via
/// `load()` which is a single atomic pointer read — no lock is taken and no
/// reader can stall a writer. Writers build a fresh `CommandState` locally
/// and publish it atomically via `store(Arc::new(new_state))`, so a reader
/// either observes the fully old snapshot or the fully new one.
pub type SharedState = Arc<ArcSwap<CommandState>>;

use crate::{
    command::protocol::{
        ActionDetails, ActionInfo, MiddlewareStatus, NodeDetails, NodeEndpointSummary, NodeInfo,
        NodeServiceInfo, ServiceInfo, TopicDetails, TopicEndpointInfo, TopicInfo,
    },
    discovery::{
        DiscoveryTable, DurationValue, EndpointEntry, Locator, Middleware, NodeEntry, NodeTable,
        ParticipantId, TopicView,
    },
};

// ── Local-only endpoint detection ────────────────────────────────────────────

const LOCATOR_KIND_UDPV4: i32 = 0x0000_0001;
const LOCATOR_KIND_UDPV6: i32 = 0x0000_0002;
const LOCATOR_KIND_TCPV4: i32 = 0x0000_0004;
const LOCATOR_KIND_TCPV6: i32 = 0x0000_0008;
/// FastDDS shared-memory transport.
const LOCATOR_KIND_SHM_FASTDDS: i32 = 0x0100_0000;
/// CycloneDDS iceoryx shared-memory transport.
const LOCATOR_KIND_SHEM_CYCLONE: i32 = 0x0100_0003;

/// Returns true if the locator address is the IPv4 loopback (127.x.x.x).
/// RTPS packs IPv4 into the last 4 bytes of the 16-byte address field.
fn is_loopback_v4(addr: &[u8; 16]) -> bool {
    addr[12] == 127
}

/// Returns true if the locator address is the IPv6 loopback (::1).
fn is_loopback_v6(addr: &[u8; 16]) -> bool {
    *addr == [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]
}

/// A locator is local if it is SHM-kind OR a loopback-addressed UDP/TCP locator.
fn locator_is_local(l: &Locator) -> bool {
    match l.kind {
        LOCATOR_KIND_SHM_FASTDDS | LOCATOR_KIND_SHEM_CYCLONE => true,
        LOCATOR_KIND_UDPV4 | LOCATOR_KIND_TCPV4 => is_loopback_v4(&l.address),
        LOCATOR_KIND_UDPV6 | LOCATOR_KIND_TCPV6 => is_loopback_v6(&l.address),
        _ => false,
    }
}

/// An endpoint is local-only if it has at least one locator and ALL locators are local.
fn is_local_endpoint(locators: &[Locator]) -> bool {
    !locators.is_empty() && locators.iter().all(locator_is_local)
}

#[derive(Clone, Debug, Default)]
pub struct CommandState {
    pub actions: Vec<ActionInfo>,
    pub action_details: Vec<ActionDetails>,
    pub nodes: Vec<NodeInfo>,
    pub node_details: Vec<NodeDetails>,
    pub services: Vec<ServiceInfo>,
    pub topics: Vec<TopicInfo>,
    pub topic_details: Vec<TopicDetails>,
    pub middleware: MiddlewareStatus,
}

pub fn shared_state() -> SharedState {
    Arc::new(ArcSwap::from_pointee(CommandState::default()))
}

pub fn refresh_from_discovery(
    state: &SharedState,
    discovery_table: &DiscoveryTable,
    node_table: &NodeTable,
    remote_participants: &Arc<Mutex<HashSet<ParticipantId>>>,
) {
    let rp = remote_participants
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let topic_views = discovery_table.topics();
    let nodes = node_table
        .nodes()
        .values()
        .map(node_info_from_entry)
        .collect::<Vec<_>>();
    let actions = build_actions(discovery_table);
    let action_details = build_action_details(discovery_table, node_table);
    let node_details = build_node_details(discovery_table, node_table);
    let services = build_services(&node_details);
    let topic_details = topic_views
        .into_iter()
        .filter_map(|topic| topic_details_from_view(discovery_table, node_table, topic, &rp))
        .collect::<Vec<_>>();
    let topics = topic_details
        .iter()
        .map(topic_info_from_details)
        .collect::<Vec<_>>();
    let middleware = middleware_status_from_discovery(discovery_table, node_table);
    // Build the new snapshot locally, then publish atomically. Concurrent
    // readers that loaded the old Arc continue to hold it until they drop.
    state.store(Arc::new(CommandState {
        actions,
        action_details,
        nodes,
        node_details,
        services,
        topics,
        topic_details,
        middleware,
    }));
}

pub fn middleware_status_from_discovery(
    discovery_table: &DiscoveryTable,
    node_table: &NodeTable,
) -> MiddlewareStatus {
    let mut status = MiddlewareStatus::default();

    for endpoint in discovery_table
        .publications()
        .values()
        .chain(discovery_table.subscriptions().values())
    {
        mark_middleware(&mut status, endpoint.endpoint_id.middleware());
        if let Some(participant_id) = &endpoint.participant_id {
            mark_middleware(&mut status, participant_id.middleware());
        }
    }

    for node in node_table.nodes().values() {
        mark_middleware(&mut status, node.participant_id.middleware());
        for endpoint_id in node
            .writer_endpoint_ids
            .iter()
            .chain(&node.reader_endpoint_ids)
        {
            mark_middleware(&mut status, endpoint_id.middleware());
        }
    }

    status
}

fn mark_middleware(status: &mut MiddlewareStatus, middleware: Middleware) {
    match middleware {
        Middleware::Rtps => status.dds = true,
        Middleware::Zenoh => status.zenoh = true,
    }
}

fn node_info_from_entry(node: &NodeEntry) -> NodeInfo {
    NodeInfo {
        name: node.key.node_name.clone(),
        namespace: node.key.node_namespace.clone(),
    }
}

fn full_node_name(namespace: &str, name: &str) -> String {
    if namespace == "/" {
        format!("/{name}")
    } else {
        format!("{namespace}/{name}")
    }
}

fn topic_info_from_details(details: &TopicDetails) -> TopicInfo {
    TopicInfo {
        name: details.name.clone(),
        type_names: details.type_names.clone(),
        publisher_count: details.publisher_count,
        subscription_count: details.subscription_count,
        local_only: details.local_only,
    }
}

fn topic_details_from_view(
    discovery_table: &DiscoveryTable,
    node_table: &NodeTable,
    topic: TopicView,
    remote_participants: &HashSet<ParticipantId>,
) -> Option<TopicDetails> {
    if topic.topic_name.starts_with("rq/") || topic.topic_name.starts_with("rr/") {
        return None;
    }
    let name = normalize_topic_name(&topic.topic_name)?;
    let publishers = topic
        .publisher_ids
        .iter()
        .filter_map(|id| {
            let endpoint = discovery_table.publication_by_id(id)?;
            let type_name = endpoint
                .type_name
                .as_deref()
                .map(normalize_type_name)
                .unwrap_or_else(|| String::from("-"));
            if !is_topic_endpoint(&name, &type_name, false) {
                return None;
            }
            let participant_locators = endpoint
                .participant_gid
                .and_then(|pgid| discovery_table.participant(&pgid))
                .map(|p| p.default_unicast_locators.as_slice())
                .unwrap_or(&[]);
            let mut info = publication_endpoint_info(node_table, endpoint, participant_locators);
            // Endpoint is local if its participant has not been seen from a remote IP.
            info.local_only = endpoint
                .participant_id
                .as_ref()
                .map_or(true, |id| !remote_participants.contains(id));
            Some(info)
        })
        .collect::<Vec<_>>();
    let subscriptions = topic
        .subscription_ids
        .iter()
        .filter_map(|id| {
            let endpoint = discovery_table.subscription_by_id(id)?;
            let type_name = endpoint
                .type_name
                .as_deref()
                .map(normalize_type_name)
                .unwrap_or_else(|| String::from("-"));
            if !is_topic_endpoint(&name, &type_name, true) {
                return None;
            }
            let participant_locators = endpoint
                .participant_gid
                .and_then(|pgid| discovery_table.participant(&pgid))
                .map(|p| p.default_unicast_locators.as_slice())
                .unwrap_or(&[]);
            Some(subscription_endpoint_info(
                node_table,
                endpoint,
                participant_locators,
            ))
        })
        .collect::<Vec<_>>();

    if publishers.is_empty() && subscriptions.is_empty() {
        return None;
    }

    let type_names = publishers
        .iter()
        .chain(subscriptions.iter())
        .filter_map(|endpoint| endpoint.topic_type.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();

    // local_only: no participant on either side (publisher or subscriber) was seen
    // from a remote IP. If any remote participant exists, network traffic must be
    // present and the topic is recordable.
    let local_only = !topic.publisher_ids.iter().any(|id| {
        discovery_table
            .publication_by_id(id)
            .filter(|endpoint| {
                let type_name = endpoint
                    .type_name
                    .as_deref()
                    .map(normalize_type_name)
                    .unwrap_or_else(|| String::from("-"));
                is_topic_endpoint(&name, &type_name, false)
            })
            .and_then(|e| e.participant_id.as_ref())
            .is_some_and(|id| remote_participants.contains(id))
    }) && !topic.subscription_ids.iter().any(|id| {
        discovery_table
            .subscription_by_id(id)
            .filter(|endpoint| {
                let type_name = endpoint
                    .type_name
                    .as_deref()
                    .map(normalize_type_name)
                    .unwrap_or_else(|| String::from("-"));
                is_topic_endpoint(&name, &type_name, true)
            })
            .and_then(|e| e.participant_id.as_ref())
            .is_some_and(|id| remote_participants.contains(id))
    });

    let publisher_count = publishers.len();
    let subscription_count = subscriptions.len();

    Some(TopicDetails {
        name,
        type_names,
        publisher_count,
        subscription_count,
        publishers,
        subscriptions,
        local_only,
    })
}

fn is_topic_endpoint(topic_name: &str, type_name: &str, is_reader: bool) -> bool {
    classify_action_endpoint(topic_name, type_name, is_reader).is_none()
        && classify_service_endpoint(topic_name, type_name, is_reader).is_none()
}

fn build_node_details(
    discovery_table: &DiscoveryTable,
    node_table: &NodeTable,
) -> Vec<NodeDetails> {
    let mut by_node = BTreeMap::<(String, String), NodeDetailsAccumulator>::new();

    for node in node_table.nodes().values() {
        let key = (node.key.node_namespace.clone(), node.key.node_name.clone());
        let acc = by_node.entry(key).or_default();

        for id in &node.reader_endpoint_ids {
            if let Some(endpoint) = discovery_table.subscription_by_id(id) {
                let Some(name) = normalize_topic_name(endpoint.topic_name.as_deref().unwrap_or(""))
                else {
                    continue;
                };
                let type_name = endpoint
                    .type_name
                    .as_deref()
                    .map(normalize_type_name)
                    .unwrap_or_else(|| String::from("-"));
                if classify_action_endpoint(&name, &type_name, true).is_some() {
                    continue;
                }
                if let Some((service_name, service_type, service_role)) =
                    classify_service_endpoint(&name, &type_name, true)
                {
                    match service_role {
                        ServiceRole::Server => {
                            acc.service_servers.insert(service_name, service_type);
                        }
                        ServiceRole::Client => {
                            acc.service_clients.insert(service_name, service_type);
                        }
                    }
                } else {
                    acc.subscribers.insert(name, type_name);
                }
            }
        }

        for id in &node.writer_endpoint_ids {
            if let Some(endpoint) = discovery_table.publication_by_id(id) {
                let Some(name) = normalize_topic_name(endpoint.topic_name.as_deref().unwrap_or(""))
                else {
                    continue;
                };
                let type_name = endpoint
                    .type_name
                    .as_deref()
                    .map(normalize_type_name)
                    .unwrap_or_else(|| String::from("-"));
                if classify_action_endpoint(&name, &type_name, false).is_some() {
                    continue;
                }
                if let Some((service_name, service_type, service_role)) =
                    classify_service_endpoint(&name, &type_name, false)
                {
                    match service_role {
                        ServiceRole::Server => {
                            acc.service_servers.insert(service_name, service_type);
                        }
                        ServiceRole::Client => {
                            acc.service_clients.insert(service_name, service_type);
                        }
                    }
                } else {
                    acc.publishers.insert(name, type_name);
                }
            }
        }
    }

    by_node
        .into_iter()
        .map(|((namespace, name), acc)| NodeDetails {
            name,
            namespace,
            subscribers: acc
                .subscribers
                .into_iter()
                .map(|(name, type_name)| NodeEndpointSummary { name, type_name })
                .collect(),
            publishers: acc
                .publishers
                .into_iter()
                .map(|(name, type_name)| NodeEndpointSummary { name, type_name })
                .collect(),
            service_servers: acc
                .service_servers
                .into_iter()
                .map(|(name, type_name)| NodeServiceInfo { name, type_name })
                .collect(),
            service_clients: acc
                .service_clients
                .into_iter()
                .map(|(name, type_name)| NodeServiceInfo { name, type_name })
                .collect(),
        })
        .collect()
}

fn build_actions(discovery_table: &DiscoveryTable) -> Vec<ActionInfo> {
    let mut actions = BTreeMap::<String, Option<String>>::new();

    for endpoint in discovery_table.publications().values() {
        collect_action(endpoint, &mut actions);
    }
    for endpoint in discovery_table.subscriptions().values() {
        collect_action(endpoint, &mut actions);
    }

    actions
        .into_iter()
        .map(|(name, type_name)| ActionInfo { name, type_name })
        .collect()
}

fn build_action_details(
    discovery_table: &DiscoveryTable,
    node_table: &NodeTable,
) -> Vec<ActionDetails> {
    let mut actions = BTreeMap::<String, ActionDetailsAccumulator>::new();

    for endpoint in discovery_table.publications().values() {
        collect_action_detail(endpoint, node_table, false, &mut actions);
    }
    for endpoint in discovery_table.subscriptions().values() {
        collect_action_detail(endpoint, node_table, true, &mut actions);
    }

    actions
        .into_iter()
        .map(|(name, acc)| ActionDetails {
            name,
            type_name: acc.type_name,
            clients: acc.clients.into_iter().collect(),
            servers: acc.servers.into_iter().collect(),
        })
        .collect()
}

fn collect_action(endpoint: &EndpointEntry, actions: &mut BTreeMap<String, Option<String>>) {
    let Some(name) = normalize_topic_name(endpoint.topic_name.as_deref().unwrap_or("")) else {
        return;
    };
    let type_name = endpoint.type_name.as_deref().map(normalize_type_name);
    let Some((action_name, action_type, _)) =
        classify_action_endpoint(&name, type_name.as_deref().unwrap_or("-"), false)
    else {
        return;
    };

    let entry = actions.entry(action_name).or_insert(None);
    if entry.is_none() && action_type.is_some() {
        *entry = action_type;
    }
}

fn collect_action_detail(
    endpoint: &EndpointEntry,
    node_table: &NodeTable,
    is_reader: bool,
    actions: &mut BTreeMap<String, ActionDetailsAccumulator>,
) {
    let Some(name) = normalize_topic_name(endpoint.topic_name.as_deref().unwrap_or("")) else {
        return;
    };
    let type_name = endpoint.type_name.as_deref().map(normalize_type_name);
    let Some((action_name, action_type, role)) =
        classify_action_endpoint(&name, type_name.as_deref().unwrap_or("-"), is_reader)
    else {
        return;
    };

    let acc = actions.entry(action_name).or_default();
    if acc.type_name.is_none() && action_type.is_some() {
        acc.type_name = action_type;
    }
    let Some(node) = endpoint_node(node_table, endpoint, is_reader) else {
        return;
    };
    let node_name = full_node_name(
        node.key.node_namespace.as_str(),
        node.key.node_name.as_str(),
    );
    match role {
        ActionRole::Client => {
            acc.clients.insert(node_name);
        }
        ActionRole::Server => {
            acc.servers.insert(node_name);
        }
    }
}

fn build_services(node_details: &[NodeDetails]) -> Vec<ServiceInfo> {
    let mut services = BTreeMap::<String, String>::new();

    for node in node_details {
        for service in &node.service_servers {
            services
                .entry(service.name.clone())
                .or_insert_with(|| service.type_name.clone());
        }
        for service in &node.service_clients {
            services
                .entry(service.name.clone())
                .or_insert_with(|| service.type_name.clone());
        }
    }

    services
        .into_iter()
        .map(|(name, type_name)| ServiceInfo { name, type_name })
        .collect()
}

fn publication_endpoint_info(
    node_table: &NodeTable,
    endpoint: &EndpointEntry,
    participant_locators: &[Locator],
) -> TopicEndpointInfo {
    endpoint_info(
        endpoint,
        "PUBLISHER",
        endpoint_node(node_table, endpoint, false),
        participant_locators,
    )
}

fn subscription_endpoint_info(
    node_table: &NodeTable,
    endpoint: &EndpointEntry,
    participant_locators: &[Locator],
) -> TopicEndpointInfo {
    endpoint_info(
        endpoint,
        "SUBSCRIPTION",
        endpoint_node(node_table, endpoint, true),
        participant_locators,
    )
}

fn endpoint_info(
    endpoint: &EndpointEntry,
    endpoint_type: &str,
    node: Option<&NodeEntry>,
    participant_locators: &[Locator],
) -> TopicEndpointInfo {
    // Use endpoint's own locators if present; fall back to participant default locators.
    let effective_locators = if endpoint.unicast_locators.is_empty() {
        participant_locators
    } else {
        &endpoint.unicast_locators
    };
    TopicEndpointInfo {
        gid: endpoint
            .endpoint_gid
            .as_ref()
            .map(format_gid)
            .unwrap_or_else(|| format!("{:?}", endpoint.endpoint_id)),
        node_name: node.map(|node| node.key.node_name.clone()),
        node_namespace: node.map(|node| node.key.node_namespace.clone()),
        topic_type: endpoint.type_name.as_deref().map(normalize_type_name),
        endpoint_type: endpoint_type.to_string(),
        reliability: endpoint.reliability.map(format_reliability),
        history: endpoint.history.map(format_history),
        durability: endpoint.durability.map(format_durability),
        deadline: endpoint.deadline.map(format_duration),
        lifespan: endpoint.lifespan.map(format_duration),
        liveliness: endpoint.liveliness.map(format_liveliness),
        liveliness_lease_duration: endpoint.liveliness_lease_duration.map(format_duration),
        shm: is_local_endpoint(effective_locators),
        local_only: false, // overridden by caller for publisher endpoints
    }
}

fn endpoint_node<'a>(
    node_table: &'a NodeTable,
    endpoint: &EndpointEntry,
    is_reader: bool,
) -> Option<&'a NodeEntry> {
    let direct = if is_reader {
        node_table
            .node_for_reader_id(&endpoint.endpoint_id)
            .or_else(|| {
                endpoint
                    .endpoint_gid
                    .as_ref()
                    .and_then(|gid| node_table.node_for_reader(gid))
            })
    } else {
        node_table
            .node_for_writer_id(&endpoint.endpoint_id)
            .or_else(|| {
                endpoint
                    .endpoint_gid
                    .as_ref()
                    .and_then(|gid| node_table.node_for_writer(gid))
            })
    };
    if direct.is_some() {
        return direct;
    }

    let nodes = endpoint
        .participant_id
        .as_ref()
        .map(|id| node_table.nodes_for_participant_id(id))
        .or_else(|| {
            endpoint
                .participant_gid
                .map(|gid| node_table.nodes_for_participant(&gid))
        })?;
    if nodes.len() == 1 {
        return nodes.into_iter().next();
    }

    None
}

fn normalize_topic_name(topic_name: &str) -> Option<String> {
    if let Some(stripped) = topic_name.strip_prefix("rt/") {
        return Some(format!("/{stripped}"));
    }
    if let Some(stripped) = topic_name.strip_prefix("rq/") {
        return Some(format!("/{stripped}"));
    }
    if let Some(stripped) = topic_name.strip_prefix("rr/") {
        return Some(format!("/{stripped}"));
    }
    if topic_name.starts_with('/') {
        return Some(topic_name.to_string());
    }
    None
}

fn normalize_type_name(type_name: &str) -> String {
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

fn format_gid(gid: &TopicGid) -> String {
    gid.bytes
        .iter()
        .copied()
        .chain(std::iter::repeat_n(0u8, 8))
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(".")
}

fn format_history(qos: crate::discovery::HistoryQos) -> String {
    match qos.kind {
        1 => format!("KEEP_LAST ({})", qos.depth),
        2 => String::from("KEEP_ALL"),
        kind => format!("UNKNOWN ({kind})"),
    }
}

fn format_reliability(qos: crate::discovery::ReliabilityQos) -> String {
    match qos.kind {
        1 => String::from("BEST_EFFORT"),
        2 => String::from("RELIABLE"),
        kind => format!("UNKNOWN ({kind})"),
    }
}

fn format_durability(qos: crate::discovery::DurabilityQos) -> String {
    match qos.kind {
        0 => String::from("VOLATILE"),
        1 => String::from("TRANSIENT_LOCAL"),
        2 => String::from("TRANSIENT"),
        3 => String::from("PERSISTENT"),
        kind => format!("UNKNOWN ({kind})"),
    }
}

fn format_liveliness(qos: crate::discovery::LivelinessQos) -> String {
    match qos.kind {
        0 => String::from("AUTOMATIC"),
        1 => String::from("MANUAL_BY_PARTICIPANT"),
        2 => String::from("MANUAL_BY_TOPIC"),
        kind => format!("UNKNOWN ({kind})"),
    }
}

fn format_duration(value: DurationValue) -> String {
    if value.seconds < 0 || (value.seconds == i32::MAX && value.fraction == u32::MAX) {
        String::from("Infinite")
    } else if value.fraction == 0 {
        format!("{}s", value.seconds)
    } else {
        format!("{}.{:09}s", value.seconds, value.fraction)
    }
}

#[derive(Default)]
struct NodeDetailsAccumulator {
    subscribers: BTreeMap<String, String>,
    publishers: BTreeMap<String, String>,
    service_servers: BTreeMap<String, String>,
    service_clients: BTreeMap<String, String>,
}

#[derive(Default)]
struct ActionDetailsAccumulator {
    type_name: Option<String>,
    clients: BTreeSet<String>,
    servers: BTreeSet<String>,
}

#[derive(Clone, Copy)]
enum ServiceRole {
    Server,
    Client,
}

#[derive(Clone, Copy)]
enum ActionRole {
    Server,
    Client,
}

fn classify_service_endpoint(
    topic_name: &str,
    type_name: &str,
    is_reader: bool,
) -> Option<(String, String, ServiceRole)> {
    let (service_type, is_request) = if let Some(stripped) = type_name.strip_suffix("_Request") {
        (stripped.to_string(), true)
    } else if let Some(stripped) = type_name.strip_suffix("_Response") {
        (stripped.to_string(), false)
    } else if type_name.contains("/srv/") {
        let role = if is_reader {
            ServiceRole::Server
        } else {
            ServiceRole::Client
        };
        return Some((topic_name.to_string(), type_name.to_string(), role));
    } else {
        return None;
    };

    if !service_type.contains("/srv/") {
        return None;
    }

    let service_name = normalize_service_name(topic_name, is_request);

    let role = match (is_reader, is_request) {
        (true, true) => ServiceRole::Server,
        (false, false) => ServiceRole::Server,
        (false, true) => ServiceRole::Client,
        (true, false) => ServiceRole::Client,
    };

    Some((service_name, service_type, role))
}

fn classify_action_endpoint(
    topic_name: &str,
    type_name: &str,
    is_reader: bool,
) -> Option<(String, Option<String>, ActionRole)> {
    let stripped_topic_name = topic_name
        .strip_suffix("Request")
        .or_else(|| topic_name.strip_suffix("Reply"))
        .or_else(|| topic_name.strip_suffix("Response"))
        .unwrap_or(topic_name);
    let action_name = normalize_action_name(topic_name)?;
    let action_type = normalize_action_type(type_name);
    let role = if stripped_topic_name.ends_with("/_action/feedback")
        || stripped_topic_name.ends_with("/_action/status")
    {
        if is_reader {
            ActionRole::Client
        } else {
            ActionRole::Server
        }
    } else {
        let is_request = topic_name.ends_with("Request") || type_name.ends_with("_Request");
        match (is_reader, is_request) {
            (true, true) => ActionRole::Server,
            (false, false) => ActionRole::Server,
            (false, true) => ActionRole::Client,
            (true, false) => ActionRole::Client,
        }
    };
    Some((action_name, action_type, role))
}

fn normalize_service_name(topic_name: &str, is_request: bool) -> String {
    if is_request {
        if let Some(stripped) = topic_name.strip_suffix("Request") {
            return stripped.to_string();
        }
    } else {
        if let Some(stripped) = topic_name.strip_suffix("Reply") {
            return stripped.to_string();
        }
        if let Some(stripped) = topic_name.strip_suffix("Response") {
            return stripped.to_string();
        }
    }

    topic_name.to_string()
}

fn normalize_action_name(topic_name: &str) -> Option<String> {
    let topic_name = topic_name
        .strip_suffix("Request")
        .or_else(|| topic_name.strip_suffix("Reply"))
        .or_else(|| topic_name.strip_suffix("Response"))
        .unwrap_or(topic_name);

    for suffix in [
        "/_action/send_goal",
        "/_action/get_result",
        "/_action/cancel_goal",
        "/_action/feedback",
        "/_action/status",
    ] {
        if let Some(stripped) = topic_name.strip_suffix(suffix) {
            return Some(stripped.to_string());
        }
    }
    None
}

fn normalize_action_type(type_name: &str) -> Option<String> {
    if !type_name.contains("/action/") || type_name.ends_with("/action/GoalStatusArray") {
        return None;
    }

    for suffix in [
        "_SendGoal_Request",
        "_SendGoal_Response",
        "_GetResult_Request",
        "_GetResult_Response",
        "_FeedbackMessage",
        "_Goal",
        "_Result",
        "_Feedback",
    ] {
        if let Some(stripped) = type_name.strip_suffix(suffix) {
            return Some(stripped.to_string());
        }
    }

    Some(type_name.to_string())
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashSet,
        sync::{Arc, Mutex},
        time::SystemTime,
    };

    use crate::discovery::{
        DiscoveredEndpoint, DiscoveredParticipant, DiscoverySample, NodeSample,
    };

    use super::*;

    #[test]
    fn zenoh_base_service_type_is_service_not_topic() {
        let participant_gid = gid(1);
        let service_gid = gid(2);
        let mut discovery_table = DiscoveryTable::default();
        let mut node_table = NodeTable::default();
        let observed_at = SystemTime::UNIX_EPOCH;

        discovery_table.apply_sample(
            DiscoverySample::Participant(DiscoveredParticipant {
                guid: Some(participant_gid),
                ..DiscoveredParticipant::default()
            }),
            observed_at,
        );
        discovery_table.apply_sample(
            DiscoverySample::Subscription(DiscoveredEndpoint {
                endpoint_gid: Some(service_gid),
                participant_gid: Some(participant_gid),
                topic_name: Some("/talker/describe_parameters".to_string()),
                type_name: Some("rcl_interfaces::srv::dds_::DescribeParameters_".to_string()),
                ..DiscoveredEndpoint::default()
            }),
            observed_at,
        );
        node_table.upsert_sample(
            NodeSample {
                participant_gid: Some(participant_gid),
                participant_id: None,
                node_namespace: "/".to_string(),
                node_name: "talker".to_string(),
                writer_gids: Vec::new(),
                reader_gids: vec![service_gid],
                writer_endpoint_ids: Vec::new(),
                reader_endpoint_ids: Vec::new(),
            },
            observed_at,
        );

        let state = build_state(discovery_table, node_table);

        assert!(state.topic_details.is_empty());
        assert_eq!(state.services.len(), 1);
        assert_eq!(state.services[0].name, "/talker/describe_parameters");
        assert_eq!(
            state.services[0].type_name,
            "rcl_interfaces/srv/DescribeParameters"
        );
        assert_eq!(state.node_details.len(), 1);
        assert!(state.node_details[0].publishers.is_empty());
        assert!(state.node_details[0].subscribers.is_empty());
        assert_eq!(state.node_details[0].service_servers.len(), 1);
        assert_eq!(
            state.node_details[0].service_servers[0].name,
            "/talker/describe_parameters"
        );
    }

    #[test]
    fn action_endpoint_is_action_not_topic() {
        let participant_gid = gid(3);
        let action_gid = gid(4);
        let mut discovery_table = DiscoveryTable::default();
        let mut node_table = NodeTable::default();
        let observed_at = SystemTime::UNIX_EPOCH;

        discovery_table.apply_sample(
            DiscoverySample::Participant(DiscoveredParticipant {
                guid: Some(participant_gid),
                ..DiscoveredParticipant::default()
            }),
            observed_at,
        );
        discovery_table.apply_sample(
            DiscoverySample::Publication(DiscoveredEndpoint {
                endpoint_gid: Some(action_gid),
                participant_gid: Some(participant_gid),
                topic_name: Some("/navigate/_action/feedback".to_string()),
                type_name: Some(
                    "nav2_msgs::action::dds_::NavigateToPose_FeedbackMessage_".to_string(),
                ),
                ..DiscoveredEndpoint::default()
            }),
            observed_at,
        );
        node_table.upsert_sample(
            NodeSample {
                participant_gid: Some(participant_gid),
                participant_id: None,
                node_namespace: "/".to_string(),
                node_name: "navigator".to_string(),
                writer_gids: vec![action_gid],
                reader_gids: Vec::new(),
                writer_endpoint_ids: Vec::new(),
                reader_endpoint_ids: Vec::new(),
            },
            observed_at,
        );

        let state = build_state(discovery_table, node_table);

        assert!(state.topic_details.is_empty());
        assert_eq!(state.actions.len(), 1);
        assert_eq!(state.actions[0].name, "/navigate");
        assert_eq!(
            state.actions[0].type_name.as_deref(),
            Some("nav2_msgs/action/NavigateToPose")
        );
        assert_eq!(state.action_details.len(), 1);
        assert_eq!(state.action_details[0].servers, vec!["/navigator"]);
        assert!(state.node_details[0].publishers.is_empty());
    }

    fn build_state(discovery_table: DiscoveryTable, node_table: NodeTable) -> CommandState {
        let state = shared_state();
        refresh_from_discovery(
            &state,
            &discovery_table,
            &node_table,
            &Arc::new(Mutex::new(HashSet::new())),
        );
        state.load_full().as_ref().clone()
    }

    fn gid(value: u8) -> TopicGid {
        TopicGid::new([value; 16])
    }
}
