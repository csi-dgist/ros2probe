use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum CommandRequest {
    ActionInfo(ActionInfoRequest),
    ActionList(ActionListRequest),
    BagRecord(BagRecordRequest),
    BagStatus(BagStatusRequest),
    BagSetPaused(BagSetPausedRequest),
    BagStop(BagStopRequest),
    NodeInfo(NodeInfoRequest),
    NodeList(NodeListRequest),
    TopicBwStart(TopicBwStartRequest),
    TopicBwStatus(TopicBwStatusRequest),
    TopicBwStop(TopicBwStopRequest),
    TopicDelayStart(TopicDelayStartRequest),
    TopicDelayStatus(TopicDelayStatusRequest),
    TopicDelayStop(TopicDelayStopRequest),
    TopicEchoStart(TopicEchoStartRequest),
    TopicEchoStatus(TopicEchoStatusRequest),
    TopicEchoStop(TopicEchoStopRequest),
    TopicHzStart(TopicHzStartRequest),
    TopicHzBwStatus(TopicHzBwStatusRequest),
    TopicHzStatus(TopicHzStatusRequest),
    TopicHzStop(TopicHzStopRequest),
    ServiceList(ServiceListRequest),
    ServiceType(ServiceTypeRequest),
    Discover(DiscoverRequest),
    TopicInfo(TopicInfoRequest),
    TopicList(TopicListRequest),
    TopicType(TopicTypeRequest),
    /// Bundled snapshot of the full ROS graph (topics + per-topic details + nodes).
    /// Replaces the N+2 sequential TopicList + TopicInfo polling used by the GUI.
    TopicGraph(TopicGraphRequest),
}

#[derive(Clone, Debug, Deserialize, Serialize, Default)]
pub struct NodeListRequest {
    #[serde(default)]
    pub count_only: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, Default)]
pub struct ActionListRequest {
    #[serde(default)]
    pub show_types: bool,
    #[serde(default)]
    pub count_only: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ActionInfoRequest {
    pub action_name: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct NodeInfoRequest {
    pub node_name: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TopicListRequest {
    #[serde(default)]
    pub show_types: bool,
    #[serde(default)]
    pub count_only: bool,
    #[serde(default)]
    pub verbose: bool,
    #[serde(default)]
    pub include_hidden: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ServiceListRequest {
    #[serde(default)]
    pub show_types: bool,
    #[serde(default)]
    pub count_only: bool,
    #[serde(default)]
    pub include_hidden: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ServiceTypeRequest {
    pub service_name: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TopicTypeRequest {
    pub topic_name: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TopicInfoRequest {
    pub topic_name: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TopicBwStartRequest {
    pub topic_name: String,
    pub window_size: usize,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct TopicBwStatusRequest;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct TopicBwStopRequest;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TopicDelayStartRequest {
    pub topic_name: String,
    pub window_size: usize,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct TopicDelayStatusRequest;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct TopicDelayStopRequest;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TopicEchoStartRequest {
    pub topic_name: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct TopicEchoStatusRequest;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct TopicEchoStopRequest;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TopicHzStartRequest {
    pub topic_name: String,
    pub window_size: usize,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct TopicHzStatusRequest;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct TopicHzStopRequest;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BagRecordRequest {
    #[serde(default)]
    pub all: bool,
    #[serde(default)]
    pub topics: Vec<String>,
    pub output: Option<String>,
    pub compression_format: Option<CompressionFormat>,
    #[serde(default)]
    pub no_discovery: bool,
    #[serde(default)]
    pub start_paused: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct BagStatusRequest;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BagSetPausedRequest {
    pub paused: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct BagStopRequest;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum CompressionFormat {
    None,
    Zstd,
    Lz4,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CommandResponse {
    ActionInfo(ActionInfoResponse),
    ActionList(ActionListResponse),
    BagRecord(BagRecordResponse),
    BagStatus(BagStatusResponse),
    BagSetPaused(BagSetPausedResponse),
    BagStop(BagStopResponse),
    NodeInfo(NodeInfoResponse),
    NodeList(NodeListResponse),
    TopicBwStart(TopicBwStartResponse),
    TopicBwStatus(TopicBwStatusResponse),
    TopicBwStop(TopicBwStopResponse),
    TopicDelayStart(TopicDelayStartResponse),
    TopicDelayStatus(TopicDelayStatusResponse),
    TopicDelayStop(TopicDelayStopResponse),
    TopicEchoStart(TopicEchoStartResponse),
    TopicEchoStatus(TopicEchoStatusResponse),
    TopicEchoStop(TopicEchoStopResponse),
    TopicHzStart(TopicHzStartResponse),
    TopicHzBwStatus(TopicHzBwStatusResponse),
    TopicHzStatus(TopicHzStatusResponse),
    TopicHzStop(TopicHzStopResponse),
    ServiceList(ServiceListResponse),
    ServiceType(ServiceTypeResponse),
    Discover(DiscoverResponse),
    TopicInfo(TopicInfoResponse),
    TopicList(TopicListResponse),
    TopicType(TopicTypeResponse),
    TopicGraph(TopicGraphResponse),
    Error(ErrorResponse),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct NodeListResponse {
    pub nodes: Vec<NodeInfo>,
    pub total_count: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ActionListResponse {
    pub actions: Vec<ActionInfo>,
    pub total_count: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ActionInfoResponse {
    pub action: Option<ActionDetails>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct NodeInfoResponse {
    pub node: Option<NodeDetails>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TopicListResponse {
    pub topics: Vec<TopicInfo>,
    pub total_count: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ServiceListResponse {
    pub services: Vec<ServiceInfo>,
    pub total_count: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ServiceTypeResponse {
    #[serde(default)]
    pub type_names: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TopicTypeResponse {
    #[serde(default)]
    pub type_names: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TopicInfoResponse {
    pub topic: Option<TopicDetails>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct TopicGraphRequest {
    #[serde(default)]
    pub include_hidden: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TopicGraphResponse {
    pub topics: Vec<TopicDetails>,
    pub nodes: Vec<NodeDetails>,
    /// Unfiltered counts for the dashboard headline.
    #[serde(default)]
    pub total_topics_count: usize,
    #[serde(default)]
    pub total_nodes_count: usize,
    #[serde(default)]
    pub total_actions_count: usize,
    #[serde(default)]
    pub total_services_count: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TopicBwStartResponse {
    pub topic_name: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TopicBwStatusResponse {
    pub active: bool,
    pub topic_name: Option<String>,
    pub bytes_per_second: Option<f64>,
    pub message_count: usize,
    pub window_size: usize,
    pub mean_size_bytes: Option<f64>,
    pub min_size_bytes: Option<usize>,
    pub max_size_bytes: Option<usize>,
    pub last_message_secs_ago: Option<f64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TopicBwStopResponse {
    pub stopped: bool,
    pub topic_name: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TopicDelayStartResponse {
    pub topic_name: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TopicDelayStatusResponse {
    pub active: bool,
    pub topic_name: Option<String>,
    #[serde(default)]
    pub messages: Vec<TopicDelayMessage>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TopicDelayStopResponse {
    pub stopped: bool,
    pub topic_name: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TopicDelayMessage {
    pub type_name: Option<String>,
    pub payload_base64: String,
    pub received_at_nanos: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TopicEchoStartResponse {
    pub topic_name: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TopicEchoStatusResponse {
    pub active: bool,
    pub topic_name: Option<String>,
    #[serde(default)]
    pub messages: Vec<TopicEchoMessage>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TopicEchoStopResponse {
    pub stopped: bool,
    pub topic_name: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TopicEchoMessage {
    pub type_name: Option<String>,
    pub payload_base64: String,
    /// Number of messages from this writer that were not received before this one.
    pub lost_before: usize,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct TopicHzBwStatusRequest;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TopicDelayStats {
    pub avg_ms: f64,
    pub min_ms: f64,
    pub max_ms: f64,
    pub std_dev_ms: f64,
    pub window: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TopicHzBwStatusResponse {
    pub hz: TopicHzStatusResponse,
    pub bw: TopicBwStatusResponse,
    pub delay: Option<TopicDelayStats>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TopicHzStartResponse {
    pub topic_name: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TopicHzStatusResponse {
    pub active: bool,
    pub topic_name: Option<String>,
    pub average_rate_hz: Option<f64>,
    pub min_period_seconds: Option<f64>,
    pub max_period_seconds: Option<f64>,
    pub std_dev_seconds: Option<f64>,
    pub window: usize,
    pub last_message_secs_ago: Option<f64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TopicHzStopResponse {
    pub stopped: bool,
    pub topic_name: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BagRecordResponse {
    pub output: String,
    pub paused: bool,
    #[serde(default)]
    pub topics: Vec<String>,
    pub all_topics: bool,
    pub no_discovery: bool,
    pub compression_format: CompressionFormat,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BagStatusResponse {
    pub active: bool,
    pub session: Option<BagSessionInfo>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BagStopResponse {
    pub stopped: bool,
    pub output: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BagSetPausedResponse {
    pub active: bool,
    pub paused: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BagSessionInfo {
    pub output: String,
    pub paused: bool,
    #[serde(default)]
    pub topics: Vec<String>,
    pub all_topics: bool,
    pub no_discovery: bool,
    pub compression_format: CompressionFormat,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ActionInfo {
    pub name: String,
    pub type_name: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ActionDetails {
    pub name: String,
    pub type_name: Option<String>,
    #[serde(default)]
    pub clients: Vec<String>,
    #[serde(default)]
    pub servers: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TopicInfo {
    pub name: String,
    #[serde(default)]
    pub type_names: Vec<String>,
    pub publisher_count: usize,
    pub subscription_count: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct NodeInfo {
    pub name: String,
    pub namespace: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ServiceInfo {
    pub name: String,
    pub type_name: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct NodeDetails {
    pub name: String,
    pub namespace: String,
    #[serde(default)]
    pub subscribers: Vec<NodeEndpointSummary>,
    #[serde(default)]
    pub publishers: Vec<NodeEndpointSummary>,
    #[serde(default)]
    pub service_servers: Vec<NodeServiceInfo>,
    #[serde(default)]
    pub service_clients: Vec<NodeServiceInfo>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct NodeEndpointSummary {
    pub name: String,
    pub type_name: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct NodeServiceInfo {
    pub name: String,
    pub type_name: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TopicDetails {
    pub name: String,
    #[serde(default)]
    pub type_names: Vec<String>,
    pub publisher_count: usize,
    pub subscription_count: usize,
    #[serde(default)]
    pub publishers: Vec<TopicEndpointInfo>,
    #[serde(default)]
    pub subscriptions: Vec<TopicEndpointInfo>,
    /// True when no publisher for this topic has sent DATA on a non-loopback interface.
    #[serde(default)]
    pub local_only: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TopicEndpointInfo {
    pub gid: String,
    pub node_name: Option<String>,
    pub node_namespace: Option<String>,
    pub topic_type: Option<String>,
    pub endpoint_type: String,
    pub reliability: Option<String>,
    pub durability: Option<String>,
    pub deadline: Option<String>,
    pub lifespan: Option<String>,
    pub liveliness: Option<String>,
    pub liveliness_lease_duration: Option<String>,
    /// True if this endpoint advertises only SHM (shared-memory) locators in SEDP.
    #[serde(default)]
    pub shm: bool,
    /// True if this publisher has never been observed sending DATA on a non-local interface.
    #[serde(default)]
    pub local_only: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct DiscoverRequest;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DiscoverResponse {
    pub triggered: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ErrorResponse {
    pub message: String,
}
