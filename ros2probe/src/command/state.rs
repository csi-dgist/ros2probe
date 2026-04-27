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
        ActionDetails, ActionInfo, NodeDetails, NodeEndpointSummary, NodeInfo, NodeServiceInfo,
        ServiceInfo, TopicDetails, TopicEndpointInfo, TopicInfo,
    },
    discovery::{DiscoveryTable, DurationValue, EndpointEntry, Locator, NodeEntry, NodeTable, TopicView},
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
}

pub fn shared_state() -> SharedState {
    Arc::new(ArcSwap::from_pointee(CommandState::default()))
}

pub fn refresh_from_discovery(
    state: &SharedState,
    discovery_table: &DiscoveryTable,
    node_table: &NodeTable,
    remote_participants: &Arc<Mutex<HashSet<TopicGid>>>,
) {
    let rp = remote_participants.lock().unwrap_or_else(|e| e.into_inner()).clone();
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
    let topics = topic_details.iter().map(topic_info_from_details).collect::<Vec<_>>();
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
    }));
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
    remote_participants: &HashSet<TopicGid>,
) -> Option<TopicDetails> {
    if topic.topic_name.starts_with("rq/") || topic.topic_name.starts_with("rr/") {
        return None;
    }
    let name = normalize_topic_name(&topic.topic_name)?;
    let publishers = topic
        .publisher_gids
        .iter()
        .filter_map(|gid| {
            let endpoint = discovery_table.publication(gid)?;
            let participant_locators = endpoint.participant_gid
                .and_then(|pgid| discovery_table.participant(&pgid))
                .map(|p| p.default_unicast_locators.as_slice())
                .unwrap_or(&[]);
            let mut info = publication_endpoint_info(node_table, endpoint, participant_locators);
            // Endpoint is local if its participant has not been seen from a remote IP.
            info.local_only = endpoint.participant_gid
                .map_or(true, |pgid| !remote_participants.contains(&pgid));
            Some(info)
        })
        .collect::<Vec<_>>();
    let subscriptions = topic
        .subscription_gids
        .iter()
        .filter_map(|gid| {
            let endpoint = discovery_table.subscription(gid)?;
            let participant_locators = endpoint.participant_gid
                .and_then(|pgid| discovery_table.participant(&pgid))
                .map(|p| p.default_unicast_locators.as_slice())
                .unwrap_or(&[]);
            Some(subscription_endpoint_info(node_table, endpoint, participant_locators))
        })
        .collect::<Vec<_>>();

    // local_only: no participant on either side (publisher or subscriber) was seen
    // from a remote IP. If any remote participant exists, network traffic must be
    // present and the topic is recordable.
    let local_only = !topic.publisher_gids.iter().any(|gid| {
        discovery_table.publication(gid)
            .and_then(|e| e.participant_gid)
            .map_or(false, |pgid| remote_participants.contains(&pgid))
    }) && !topic.subscription_gids.iter().any(|gid| {
        discovery_table.subscription(gid)
            .and_then(|e| e.participant_gid)
            .map_or(false, |pgid| remote_participants.contains(&pgid))
    });

    Some(TopicDetails {
        name,
        type_names: topic
            .type_names
            .into_iter()
            .map(|type_name| normalize_type_name(&type_name))
            .collect(),
        publisher_count: topic.publisher_count,
        subscription_count: topic.subscription_count,
        publishers,
        subscriptions,
        local_only,
    })
}

fn build_node_details(discovery_table: &DiscoveryTable, node_table: &NodeTable) -> Vec<NodeDetails> {
    let mut by_node = BTreeMap::<(String, String), NodeDetailsAccumulator>::new();

    for node in node_table.nodes().values() {
        let key = (node.key.node_namespace.clone(), node.key.node_name.clone());
        let acc = by_node.entry(key).or_default();

        for gid in &node.reader_gids {
            if let Some(endpoint) = discovery_table.subscription(gid) {
                let Some(name) = normalize_topic_name(endpoint.topic_name.as_deref().unwrap_or("")) else {
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

        for gid in &node.writer_gids {
            if let Some(endpoint) = discovery_table.publication(gid) {
                let Some(name) = normalize_topic_name(endpoint.topic_name.as_deref().unwrap_or("")) else {
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
    let node_name = full_node_name(node.key.node_namespace.as_str(), node.key.node_name.as_str());
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
    endpoint_info(endpoint, "PUBLISHER", endpoint_node(node_table, endpoint, false), participant_locators)
}

fn subscription_endpoint_info(
    node_table: &NodeTable,
    endpoint: &EndpointEntry,
    participant_locators: &[Locator],
) -> TopicEndpointInfo {
    endpoint_info(endpoint, "SUBSCRIPTION", endpoint_node(node_table, endpoint, true), participant_locators)
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
        gid: format_gid(&endpoint.endpoint_gid),
        node_name: node.map(|node| node.key.node_name.clone()),
        node_namespace: node.map(|node| node.key.node_namespace.clone()),
        topic_type: endpoint.type_name.as_deref().map(normalize_type_name),
        endpoint_type: endpoint_type.to_string(),
        reliability: endpoint.reliability.map(format_reliability),
        durability: endpoint.durability.map(format_durability),
        deadline: endpoint.deadline.map(format_duration),
        lifespan: endpoint.lifespan.map(format_duration),
        liveliness: endpoint.liveliness.map(format_liveliness),
        liveliness_lease_duration: endpoint
            .liveliness
            .map(|qos| format_duration(qos.lease_duration)),
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
        node_table.node_for_reader(&endpoint.endpoint_gid)
    } else {
        node_table.node_for_writer(&endpoint.endpoint_gid)
    };
    if direct.is_some() {
        return direct;
    }

    let participant_gid = endpoint.participant_gid?;
    let nodes = node_table.nodes_for_participant(&participant_gid);
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
    if value.seconds < 0
        || (value.seconds == i32::MAX && value.fraction == u32::MAX)
    {
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
