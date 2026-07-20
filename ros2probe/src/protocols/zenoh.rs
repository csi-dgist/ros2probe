use std::{
    collections::{HashMap, VecDeque},
    fmt,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

use anyhow::Result;
use bytes::Bytes;
use ros2probe_common::{FlowTuple, TOPIC_GID_LEN, TopicGid};
use zenoh_buffers::{
    ZBuf, ZSlice,
    buffer::{Buffer, SplitBuffer},
    reader::HasReader,
};
use zenoh_codec::{RCodec, Zenoh080Reliability};
use zenoh_protocol::{
    core::{Bits, Field, Priority, Reliability, WireExpr},
    network::{DeclareBody, Mapping, NetworkBody, NetworkMessage},
    transport::{BatchSize, TransportBody, TransportMessage, TransportSn},
    zenoh::PushBody,
};
use zenoh_transport::common::batch::{BatchConfig, Decode, RBatch};

use crate::capture::{CapturedTransportPacket, PacketDirection, TcpSegmentInfo, TransportProtocol};

const TCP_BATCH_HEADER_LEN: usize = 2;
const TCP_FLAG_FIN: u16 = 0x001;
const TCP_FLAG_SYN: u16 = 0x002;
const TCP_FLAG_RST: u16 = 0x004;
const ZENOH_TCP_REASSEMBLY_CAPACITY: usize = 1024 * 1024;
const ZENOH_TCP_REASSEMBLY_MAX_SEGMENTS: usize = 4096;
const ROS2_LIVELINESS_PREFIX: &str = "@ros2_lv";
const ZENOH_FRAGMENT_DEFRAG_FLOW_CAPACITY: usize = 128 * 1024 * 1024;
pub(crate) const ZENOH_FRAGMENT_DEFRAG_TOTAL_CAPACITY: usize = 128 * 1024 * 1024;
const ZENOH_FRAGMENT_FLOW_TIMEOUT: Duration = Duration::from_secs(30);
type DecodeBatchFn = fn(&[u8]) -> Option<Vec<ZenohDecodedMessage>>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ZenohWireExpr {
    pub scope: u16,
    pub suffix: String,
    pub mapping: Mapping,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ZenohWireExprResolutionError {
    UnknownScope(u16),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ZenohRosEntityKind {
    Node,
    Publisher,
    Subscription,
    ServiceServer,
    ServiceClient,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ZenohRosNodeInfo {
    pub domain_id: u64,
    pub zid: String,
    pub node_id: String,
    pub entity_id: String,
    pub enclave: String,
    pub namespace: String,
    pub node_name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ZenohRosTopicInfo {
    pub name: String,
    pub type_name: String,
    pub type_hash: String,
    pub qos: String,
    pub backends: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ZenohRosLivelinessEntity {
    pub keyexpr: String,
    pub kind: ZenohRosEntityKind,
    pub node: ZenohRosNodeInfo,
    pub topic: Option<ZenohRosTopicInfo>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ZenohRosLivelinessParseError {
    MissingPrefix,
    TooFewParts { actual: usize, minimum: usize },
    EmptyPart,
    InvalidDomainId(String),
    InvalidEntityKind(String),
    MissingTopicInfo,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ZenohRosTopicSample {
    pub keyexpr: String,
    pub domain_id: u64,
    pub topic_name: String,
    pub type_name: String,
    pub type_hash: String,
    pub payload: Bytes,
    pub payload_len: usize,
    pub identity: Option<ZenohRosSampleIdentity>,
    pub attachment_len: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ZenohUnresolvedTopicSample {
    pub wire_expr: ZenohWireExpr,
    pub payload: Bytes,
    pub payload_len: usize,
    pub identity: ZenohRosSampleIdentity,
    pub attachment_len: Option<usize>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ZenohRosSampleIdentity {
    pub source_gid: TopicGid,
    pub sequence_number: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ZenohRosTopicKeyexprParseError {
    TooFewParts { actual: usize, minimum: usize },
    EmptyPart,
    InvalidDomainId(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ZenohSemanticEvent {
    RosEntityDiscovered(ZenohRosLivelinessEntity),
    RosEntityUndiscovered(ZenohRosLivelinessEntity),
    TopicSample(ZenohRosTopicSample),
    UnresolvedTopicSample(ZenohUnresolvedTopicSample),
    ShmTopicSample {
        topic_name: Option<String>,
        identity: Option<ZenohRosSampleIdentity>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ZenohDecodedMessage {
    Oam,
    InitSyn {
        frame_sn_resolution: Bits,
    },
    InitAck {
        frame_sn_resolution: Bits,
    },
    OpenSyn,
    OpenAck,
    Close,
    KeepAlive,
    Frame {
        reliability: String,
        sequence_number: Option<String>,
        messages: Vec<ZenohNetworkMessage>,
    },
    Fragment {
        reliability: Reliability,
        sequence_number: TransportSn,
        priority: Priority,
        more: bool,
        first: bool,
        drop: bool,
        payload: Bytes,
    },
    Join {
        frame_sn_resolution: Bits,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ZenohNetworkMessage {
    Oam,
    Push {
        reliability: String,
        wire_expr: ZenohWireExpr,
        body: ZenohPushBody,
    },
    Request {
        reliability: String,
        id: String,
        wire_expr: ZenohWireExpr,
        body: String,
    },
    Response {
        reliability: String,
        request_id: String,
        wire_expr: ZenohWireExpr,
        body: String,
    },
    ResponseFinal,
    Interest {
        reliability: String,
        id: String,
        wire_expr: Option<ZenohWireExpr>,
    },
    Declare {
        reliability: String,
        body: ZenohDeclareBody,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ZenohPushBody {
    Put {
        encoding: String,
        payload: Bytes,
        payload_len: usize,
        attachment: Option<Bytes>,
        attachment_len: Option<usize>,
        is_shm: bool,
    },
    Del {
        attachment_len: Option<usize>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ZenohDeclareBody {
    DeclareKeyExpr {
        id: u16,
        wire_expr: ZenohWireExpr,
    },
    UndeclareKeyExpr {
        id: u16,
    },
    DeclareSubscriber {
        id: u32,
        wire_expr: ZenohWireExpr,
    },
    UndeclareSubscriber {
        id: u32,
    },
    DeclareQueryable {
        id: u32,
        wire_expr: ZenohWireExpr,
        complete: bool,
        distance: u16,
    },
    UndeclareQueryable {
        id: u32,
    },
    DeclareToken {
        id: u32,
        wire_expr: ZenohWireExpr,
    },
    UndeclareToken {
        id: u32,
    },
    DeclareFinal,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ZenohFlowDirection {
    pub protocol: TransportProtocol,
    pub flow: FlowTuple,
}

#[derive(Clone, Debug, Default)]
pub struct ZenohKeyExprTable {
    entries: HashMap<u16, String>,
}

#[derive(Clone, Debug, Default)]
pub struct ZenohSemanticState {
    keyexpr_tables: HashMap<ZenohFlowDirection, ZenohKeyExprTable>,
    token_tables: HashMap<ZenohFlowDirection, ZenohTokenTable>,
    graph: ZenohRosGraph,
    flow_order: VecDeque<ZenohFlowDirection>,
    capacity: usize,
}

#[derive(Clone, Debug, Default)]
pub struct ZenohTokenTable {
    entries: HashMap<u32, ZenohTokenEntry>,
}

#[derive(Clone, Debug)]
struct ZenohTokenEntry {
    keyexpr: String,
    entity: Option<ZenohRosLivelinessEntity>,
}

#[derive(Clone, Debug, Default)]
pub struct ZenohRosGraph {
    entities: HashMap<String, ZenohRosLivelinessEntity>,
    ref_counts: HashMap<String, usize>,
}

#[derive(Clone, Debug)]
pub struct ZenohBatch {
    pub socket_timestamp: SystemTime,
    pub frame_len: usize,
    pub direction: PacketDirection,
    pub protocol: TransportProtocol,
    pub flow: FlowTuple,
    pub payload: Bytes,
    pub messages: Vec<ZenohDecodedMessage>,
    pub semantic_events: Vec<ZenohSemanticEvent>,
    pub ip_fragment_count: u32,
    pub was_ip_fragmented: bool,
}

#[derive(Clone, Debug)]
pub enum ZenohEvent {
    Batch(ZenohBatch),
}

pub struct ZenohProcessor {
    tcp_flows: HashMap<FlowTuple, ZenohTcpFlowState>,
    fragment_flows: HashMap<ZenohFragmentFlowKey, ZenohFragmentState>,
    transport_resolutions: HashMap<ZenohFlowDirection, ZenohTransportResolution>,
    fragment_budget: Arc<ZenohFragmentMemoryBudget>,
    fragment_timeout: Duration,
    semantic: ZenohSemanticState,
    order: VecDeque<FlowTuple>,
    capacity: usize,
    decode_batch: DecodeBatchFn,
}

struct ZenohTcpFlowState {
    aligned: bool,
    next_seq: Option<u32>,
    segments: Vec<TcpSegment>,
    queued_segment_bytes: usize,
    stream_buffer: Vec<u8>,
    ip_fragment_count: u32,
    was_ip_fragmented: bool,
}

struct TcpSegment {
    seq: u32,
    bytes: Bytes,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ZenohFragmentFlowKey {
    direction: ZenohFlowDirection,
    reliability: Reliability,
    priority: Priority,
}

#[derive(Clone, Debug)]
struct ZenohFragmentState {
    next_sn: Option<TransportSn>,
    sn_mask: TransportSn,
    payload: Vec<u8>,
    last_seen: Instant,
}

impl ZenohFragmentState {
    fn new(now: Instant, sn_mask: TransportSn) -> Self {
        Self {
            next_sn: None,
            sn_mask,
            payload: Vec::new(),
            last_seen: now,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct ZenohTransportResolution {
    frame_sn: Bits,
    last_seen: Instant,
}

#[derive(Debug)]
pub(crate) struct ZenohFragmentMemoryBudget {
    capacity: usize,
    used: AtomicUsize,
}

impl ZenohFragmentMemoryBudget {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            capacity,
            used: AtomicUsize::new(0),
        }
    }

    fn try_reserve(&self, bytes: usize) -> bool {
        self.used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes).filter(|new| *new <= self.capacity)
            })
            .is_ok()
    }

    fn release(&self, bytes: usize) {
        if bytes == 0 {
            return;
        }
        let previous = self.used.fetch_sub(bytes, Ordering::AcqRel);
        debug_assert!(
            previous >= bytes,
            "released more fragment bytes than reserved"
        );
    }

    #[cfg(test)]
    fn used(&self) -> usize {
        self.used.load(Ordering::Acquire)
    }
}

impl ZenohProcessor {
    pub fn new(capacity: usize) -> Self {
        Self::with_fragment_budget(
            capacity,
            Arc::new(ZenohFragmentMemoryBudget::new(
                ZENOH_FRAGMENT_DEFRAG_TOTAL_CAPACITY,
            )),
        )
    }

    pub(crate) fn with_fragment_budget(
        capacity: usize,
        fragment_budget: Arc<ZenohFragmentMemoryBudget>,
    ) -> Self {
        Self {
            tcp_flows: HashMap::with_capacity(capacity),
            fragment_flows: HashMap::with_capacity(capacity),
            transport_resolutions: HashMap::with_capacity(capacity),
            fragment_budget,
            fragment_timeout: ZENOH_FRAGMENT_FLOW_TIMEOUT,
            semantic: ZenohSemanticState::with_capacity(capacity),
            order: VecDeque::with_capacity(capacity),
            capacity,
            decode_batch: decode_zenoh_batch,
        }
    }

    #[cfg(test)]
    fn new_with_decoder(capacity: usize, decode_batch: DecodeBatchFn) -> Self {
        Self::new_with_decoder_and_fragment_limits(
            capacity,
            decode_batch,
            Arc::new(ZenohFragmentMemoryBudget::new(
                ZENOH_FRAGMENT_DEFRAG_TOTAL_CAPACITY,
            )),
            ZENOH_FRAGMENT_FLOW_TIMEOUT,
        )
    }

    #[cfg(test)]
    fn new_with_decoder_and_fragment_limits(
        capacity: usize,
        decode_batch: DecodeBatchFn,
        fragment_budget: Arc<ZenohFragmentMemoryBudget>,
        fragment_timeout: Duration,
    ) -> Self {
        Self {
            tcp_flows: HashMap::with_capacity(capacity),
            fragment_flows: HashMap::with_capacity(capacity),
            transport_resolutions: HashMap::with_capacity(capacity),
            fragment_budget,
            fragment_timeout,
            semantic: ZenohSemanticState::with_capacity(capacity),
            order: VecDeque::with_capacity(capacity),
            capacity,
            decode_batch,
        }
    }

    pub fn process_packet(&mut self, packet: CapturedTransportPacket) -> Result<Vec<ZenohEvent>> {
        self.expire_inactive_fragments();
        match packet.protocol {
            TransportProtocol::Udp => Ok(self.process_udp(packet)),
            TransportProtocol::Tcp => Ok(self.process_tcp(packet)),
        }
    }

    fn process_udp(&mut self, packet: CapturedTransportPacket) -> Vec<ZenohEvent> {
        if packet.payload.is_empty() {
            return Vec::new();
        }
        let Some(messages) = (self.decode_batch)(&packet.payload) else {
            return Vec::new();
        };
        let messages =
            self.expand_fragments(packet.protocol, packet.flow, messages, Instant::now());

        let semantic_events =
            self.semantic
                .process_messages(packet.protocol, packet.flow, &messages);

        vec![ZenohEvent::Batch(ZenohBatch {
            socket_timestamp: packet.socket_timestamp,
            frame_len: packet.frame_len,
            direction: packet.direction,
            protocol: packet.protocol,
            flow: packet.flow,
            payload: packet.payload,
            messages,
            semantic_events,
            ip_fragment_count: packet.ip_fragment_count,
            was_ip_fragmented: packet.was_ip_fragmented,
        })]
    }

    fn process_tcp(&mut self, packet: CapturedTransportPacket) -> Vec<ZenohEvent> {
        let Some(tcp) = packet.tcp else {
            return Vec::new();
        };

        if has_flag(tcp, TCP_FLAG_RST) {
            self.remove_tcp_flow(packet.flow);
            let semantic_events = self
                .semantic
                .remove_flow_direction(TransportProtocol::Tcp, packet.flow);
            return semantic_events_batch(&packet, semantic_events)
                .into_iter()
                .collect();
        }

        let flow = packet.flow;
        if packet.payload.is_empty() {
            if has_flag(tcp, TCP_FLAG_FIN) {
                self.remove_tcp_flow(flow);
                let semantic_events = self
                    .semantic
                    .remove_flow_direction(TransportProtocol::Tcp, flow);
                return semantic_events_batch(&packet, semantic_events)
                    .into_iter()
                    .collect();
            }
            return Vec::new();
        }

        let payload_seq = tcp_payload_sequence(tcp);
        let has_fin = has_flag(tcp, TCP_FLAG_FIN);
        let decode_batch = self.decode_batch;
        let mut events = {
            let state = self.tcp_flow_state(flow);
            if state.aligned {
                if !state.insert_segment(payload_seq, packet.payload.clone()) {
                    state.reset_alignment();
                    state.try_align_from_segment(&packet, payload_seq, decode_batch)
                } else {
                    state.flush_contiguous();
                    state.ip_fragment_count = state.ip_fragment_count.max(packet.ip_fragment_count);
                    state.was_ip_fragmented |= packet.was_ip_fragmented;

                    let events = frame_batches(
                        &mut state.stream_buffer,
                        &packet,
                        state.ip_fragment_count,
                        state.was_ip_fragmented,
                        decode_batch,
                    );
                    if events.decode_failed {
                        state.reset_alignment();
                    } else if !events.events.is_empty() && state.stream_buffer.is_empty() {
                        state.ip_fragment_count = 1;
                        state.was_ip_fragmented = false;
                    }
                    events.events
                }
            } else {
                state.try_align_from_segment(&packet, payload_seq, decode_batch)
            }
        };

        for event in &mut events {
            let ZenohEvent::Batch(batch) = event;
            let messages = std::mem::take(&mut batch.messages);
            batch.messages =
                self.expand_fragments(batch.protocol, batch.flow, messages, Instant::now());
            batch.semantic_events =
                self.semantic
                    .process_messages(batch.protocol, batch.flow, &batch.messages);
        }

        if has_fin {
            self.remove_tcp_flow(flow);
            let semantic_events = self
                .semantic
                .remove_flow_direction(TransportProtocol::Tcp, flow);
            if let Some(event) = semantic_events_batch(&packet, semantic_events) {
                events.push(event);
            }
        }

        events
    }

    fn tcp_flow_state(&mut self, flow: FlowTuple) -> &mut ZenohTcpFlowState {
        if !self.tcp_flows.contains_key(&flow) {
            if self.tcp_flows.len() == self.capacity {
                while let Some(oldest) = self.order.pop_front() {
                    if self.tcp_flows.remove(&oldest).is_some() {
                        let _ = self
                            .semantic
                            .remove_flow_direction(TransportProtocol::Tcp, oldest);
                        self.remove_fragment_flow_direction(TransportProtocol::Tcp, oldest);
                        break;
                    }
                }
            }
            self.order.push_back(flow);
        }

        self.tcp_flows
            .entry(flow)
            .or_insert_with(ZenohTcpFlowState::default)
    }

    fn remove_tcp_flow(&mut self, flow: FlowTuple) {
        self.tcp_flows.remove(&flow);
        if let Some(position) = self.order.iter().position(|queued| *queued == flow) {
            self.order.remove(position);
        }
        self.remove_fragment_flow_direction(TransportProtocol::Tcp, flow);
        self.transport_resolutions
            .remove(&ZenohFlowDirection::new(TransportProtocol::Tcp, flow));
    }

    fn remove_fragment_flow_direction(&mut self, protocol: TransportProtocol, flow: FlowTuple) {
        let keys = self
            .fragment_flows
            .keys()
            .filter(|key| key.direction.protocol == protocol && key.direction.flow == flow)
            .cloned()
            .collect::<Vec<_>>();
        for key in keys {
            self.remove_fragment_state(&key);
        }
    }

    pub fn expire_inactive_fragments(&mut self) -> usize {
        self.expire_inactive_fragments_at(Instant::now())
    }

    fn expire_inactive_fragments_at(&mut self, now: Instant) -> usize {
        let keys = self
            .fragment_flows
            .iter()
            .filter(|(_, state)| {
                now.saturating_duration_since(state.last_seen) >= self.fragment_timeout
            })
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        let expired = keys.len();
        for key in keys {
            self.remove_fragment_state(&key);
        }
        expired
    }

    fn take_fragment_state(&mut self, key: &ZenohFragmentFlowKey) -> Option<ZenohFragmentState> {
        self.fragment_flows.remove(key)
    }

    fn remove_fragment_state(&mut self, key: &ZenohFragmentFlowKey) {
        if let Some(state) = self.take_fragment_state(key) {
            self.fragment_budget.release(state.payload.len());
        }
    }

    fn evict_oldest_fragment_state(&mut self) {
        let oldest = self
            .fragment_flows
            .iter()
            .min_by_key(|(_, state)| state.last_seen)
            .map(|(key, _)| key.clone());
        if let Some(oldest) = oldest {
            self.remove_fragment_state(&oldest);
        }
    }

    fn remember_frame_sn_resolution(
        &mut self,
        direction: ZenohFlowDirection,
        frame_sn: Bits,
        bidirectional: bool,
        now: Instant,
    ) {
        self.remember_direction_resolution(direction, frame_sn, now);
        if bidirectional {
            self.remember_direction_resolution(direction.reverse(), frame_sn, now);
        }
    }

    fn remember_direction_resolution(
        &mut self,
        direction: ZenohFlowDirection,
        frame_sn: Bits,
        now: Instant,
    ) {
        if self.capacity == 0 {
            return;
        }
        if !self.transport_resolutions.contains_key(&direction)
            && self.transport_resolutions.len() >= self.capacity
        {
            let oldest = self
                .transport_resolutions
                .iter()
                .min_by_key(|(_, state)| state.last_seen)
                .map(|(direction, _)| *direction);
            if let Some(oldest) = oldest {
                self.transport_resolutions.remove(&oldest);
            }
        }
        self.transport_resolutions.insert(
            direction,
            ZenohTransportResolution {
                frame_sn,
                last_seen: now,
            },
        );
    }

    fn fragment_sn_mask(&mut self, direction: ZenohFlowDirection, now: Instant) -> TransportSn {
        self.transport_resolutions
            .get_mut(&direction)
            .map(|resolution| {
                resolution.last_seen = now;
                resolution.frame_sn.mask() as TransportSn
            })
            .unwrap_or(TransportSn::MAX)
    }

    fn expand_fragments(
        &mut self,
        protocol: TransportProtocol,
        flow: FlowTuple,
        messages: Vec<ZenohDecodedMessage>,
        now: Instant,
    ) -> Vec<ZenohDecodedMessage> {
        let mut expanded = Vec::with_capacity(messages.len());
        for message in messages {
            let complete = match &message {
                ZenohDecodedMessage::InitAck {
                    frame_sn_resolution,
                } => {
                    self.remember_frame_sn_resolution(
                        ZenohFlowDirection::new(protocol, flow),
                        *frame_sn_resolution,
                        true,
                        now,
                    );
                    None
                }
                ZenohDecodedMessage::Join {
                    frame_sn_resolution,
                } => {
                    self.remember_frame_sn_resolution(
                        ZenohFlowDirection::new(protocol, flow),
                        *frame_sn_resolution,
                        false,
                        now,
                    );
                    None
                }
                ZenohDecodedMessage::Fragment {
                    reliability,
                    sequence_number,
                    priority,
                    more,
                    first,
                    drop,
                    payload,
                } => self.accept_fragment(
                    protocol,
                    flow,
                    reliability.clone(),
                    *priority,
                    *sequence_number,
                    *more,
                    *first,
                    *drop,
                    payload,
                    now,
                ),
                _ => None,
            };
            expanded.push(message);
            if let Some((reliability, sequence_number, network_message)) = complete {
                expanded.push(ZenohDecodedMessage::Frame {
                    reliability: format!("{reliability:?}"),
                    sequence_number: Some(format!("{sequence_number:?}")),
                    messages: vec![decode_network_message(&network_message)],
                });
            }
        }
        expanded
    }

    #[allow(clippy::too_many_arguments)]
    fn accept_fragment(
        &mut self,
        protocol: TransportProtocol,
        flow: FlowTuple,
        reliability: Reliability,
        priority: Priority,
        sequence_number: TransportSn,
        more: bool,
        first: bool,
        drop: bool,
        payload: &Bytes,
        now: Instant,
    ) -> Option<(Reliability, TransportSn, NetworkMessage)> {
        let direction = ZenohFlowDirection::new(protocol, flow);
        let sn_mask = self.fragment_sn_mask(direction, now);
        let key = ZenohFragmentFlowKey {
            direction,
            reliability: reliability.clone(),
            priority,
        };

        if drop {
            self.remove_fragment_state(&key);
            return None;
        }

        if !self.fragment_flows.contains_key(&key) {
            if self.capacity == 0 {
                return None;
            }
            if self.fragment_flows.len() >= self.capacity {
                self.evict_oldest_fragment_state();
            }
        }

        let released_for_restart = {
            let state = self
                .fragment_flows
                .entry(key.clone())
                .or_insert_with(|| ZenohFragmentState::new(now, sn_mask));
            state.last_seen = now;
            if first {
                let released = state.payload.len();
                state.payload = Vec::new();
                state.next_sn = Some(sequence_number);
                state.sn_mask = sn_mask;
                released
            } else if state.next_sn.is_none() {
                state.next_sn = Some(sequence_number);
                0
            } else {
                0
            }
        };
        self.fragment_budget.release(released_for_restart);

        if self.fragment_flows.get(&key)?.next_sn != Some(sequence_number) {
            self.remove_fragment_state(&key);
            return None;
        }

        let new_len = self
            .fragment_flows
            .get(&key)?
            .payload
            .len()
            .checked_add(payload.len())?;
        if new_len > ZENOH_FRAGMENT_DEFRAG_FLOW_CAPACITY
            || !self.fragment_budget.try_reserve(payload.len())
        {
            self.remove_fragment_state(&key);
            return None;
        }

        let reserve_failed = self
            .fragment_flows
            .get_mut(&key)?
            .payload
            .try_reserve_exact(payload.len())
            .is_err();
        if reserve_failed {
            self.fragment_budget.release(payload.len());
            self.remove_fragment_state(&key);
            return None;
        }

        let should_decode = {
            let state = self.fragment_flows.get_mut(&key)?;
            state.payload.extend_from_slice(payload);
            state.next_sn = Some(sequence_number.wrapping_add(1) & state.sn_mask);
            !more
        };

        if !should_decode {
            return None;
        }

        let state = self.take_fragment_state(&key)?;
        let reserved_bytes = state.payload.len();
        let network_message = decode_fragmented_network_message(reliability.clone(), state.payload);
        self.fragment_budget.release(reserved_bytes);
        let network_message = network_message?;
        Some((reliability, sequence_number, network_message))
    }
}

impl Drop for ZenohProcessor {
    fn drop(&mut self) {
        let reserved_bytes = self
            .fragment_flows
            .values()
            .map(|state| state.payload.len())
            .sum();
        self.fragment_budget.release(reserved_bytes);
    }
}

fn semantic_events_batch(
    packet: &CapturedTransportPacket,
    semantic_events: Vec<ZenohSemanticEvent>,
) -> Option<ZenohEvent> {
    (!semantic_events.is_empty()).then(|| {
        ZenohEvent::Batch(ZenohBatch {
            socket_timestamp: packet.socket_timestamp,
            frame_len: packet.frame_len,
            direction: packet.direction,
            protocol: packet.protocol,
            flow: packet.flow,
            payload: Bytes::new(),
            messages: Vec::new(),
            semantic_events,
            ip_fragment_count: packet.ip_fragment_count,
            was_ip_fragmented: packet.was_ip_fragmented,
        })
    })
}

impl ZenohFlowDirection {
    const fn new(protocol: TransportProtocol, flow: FlowTuple) -> Self {
        Self { protocol, flow }
    }

    const fn reverse(self) -> Self {
        Self {
            protocol: self.protocol,
            flow: FlowTuple {
                src_ip: self.flow.dst_ip,
                dst_ip: self.flow.src_ip,
                src_port: self.flow.dst_port,
                dst_port: self.flow.src_port,
            },
        }
    }
}

impl ZenohSemanticState {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            keyexpr_tables: HashMap::with_capacity(capacity),
            token_tables: HashMap::with_capacity(capacity),
            graph: ZenohRosGraph::default(),
            flow_order: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    fn ensure_flow_direction(
        &mut self,
        direction: ZenohFlowDirection,
    ) -> Option<Vec<ZenohSemanticEvent>> {
        if self.keyexpr_tables.contains_key(&direction) {
            return Some(Vec::new());
        }
        if self.capacity == 0 {
            return None;
        }

        let mut events = Vec::new();
        while self.keyexpr_tables.len() >= self.capacity {
            let Some(oldest) = self.flow_order.pop_front() else {
                break;
            };
            events.extend(self.remove_flow_direction_state(oldest));
        }
        self.keyexpr_tables
            .insert(direction, ZenohKeyExprTable::default());
        self.flow_order.push_back(direction);
        Some(events)
    }

    fn ensure_keyexpr_table(
        &mut self,
        protocol: TransportProtocol,
        flow: FlowTuple,
    ) -> &mut ZenohKeyExprTable {
        self.keyexpr_tables
            .entry(ZenohFlowDirection::new(protocol, flow))
            .or_default()
    }

    fn ensure_token_table(
        &mut self,
        protocol: TransportProtocol,
        flow: FlowTuple,
    ) -> &mut ZenohTokenTable {
        self.token_tables
            .entry(ZenohFlowDirection::new(protocol, flow))
            .or_default()
    }

    fn remove_flow_direction(
        &mut self,
        protocol: TransportProtocol,
        flow: FlowTuple,
    ) -> Vec<ZenohSemanticEvent> {
        let direction = ZenohFlowDirection::new(protocol, flow);
        if let Some(position) = self
            .flow_order
            .iter()
            .position(|queued| *queued == direction)
        {
            self.flow_order.remove(position);
        }
        self.remove_flow_direction_state(direction)
    }

    fn remove_flow_direction_state(
        &mut self,
        direction: ZenohFlowDirection,
    ) -> Vec<ZenohSemanticEvent> {
        self.keyexpr_tables.remove(&direction);
        self.remove_token_table(direction)
    }

    fn process_messages(
        &mut self,
        protocol: TransportProtocol,
        flow: FlowTuple,
        messages: &[ZenohDecodedMessage],
    ) -> Vec<ZenohSemanticEvent> {
        let direction = ZenohFlowDirection::new(protocol, flow);
        let Some(mut semantic_events) = self.ensure_flow_direction(direction) else {
            return Vec::new();
        };

        for message in messages {
            if self.process_transport_message(direction, message) {
                semantic_events.extend(self.remove_flow_direction(protocol, flow));
                break;
            }
            self.process_transport_message_semantics(direction, message, &mut semantic_events);
        }

        semantic_events
    }

    fn process_transport_message(
        &self,
        _direction: ZenohFlowDirection,
        message: &ZenohDecodedMessage,
    ) -> bool {
        matches!(message, ZenohDecodedMessage::Close)
    }

    fn process_transport_message_semantics(
        &mut self,
        direction: ZenohFlowDirection,
        message: &ZenohDecodedMessage,
        semantic_events: &mut Vec<ZenohSemanticEvent>,
    ) {
        if let ZenohDecodedMessage::Frame { messages, .. } = message {
            for message in messages {
                self.process_network_message(direction, message, semantic_events);
            }
        }
    }

    fn process_network_message(
        &mut self,
        direction: ZenohFlowDirection,
        message: &ZenohNetworkMessage,
        semantic_events: &mut Vec<ZenohSemanticEvent>,
    ) {
        match message {
            ZenohNetworkMessage::Declare { body, .. } => {
                self.process_declare_body(direction, body, semantic_events);
            }
            ZenohNetworkMessage::Push {
                wire_expr,
                body:
                    ZenohPushBody::Put {
                        payload,
                        payload_len,
                        attachment,
                        attachment_len,
                        is_shm,
                        ..
                    },
                ..
            } => {
                let identity = attachment
                    .as_deref()
                    .and_then(parse_rmw_zenoh_attachment_identity);
                if *is_shm {
                    let topic_name = self
                        .resolve_wire_expr_by_direction(direction, wire_expr)
                        .ok()
                        .and_then(|keyexpr| {
                            parse_ros_topic_sample_keyexpr(
                                &keyexpr,
                                Bytes::new(),
                                0,
                                *attachment_len,
                            )
                            .ok()
                        })
                        .map(|sample| sample.topic_name);
                    semantic_events.push(ZenohSemanticEvent::ShmTopicSample {
                        topic_name,
                        identity,
                    });
                    return;
                }
                let mut resolved = false;
                if let Ok(keyexpr) = self.resolve_wire_expr_by_direction(direction, wire_expr) {
                    if let Ok(mut topic_sample) = parse_ros_topic_sample_keyexpr(
                        &keyexpr,
                        payload.clone(),
                        *payload_len,
                        *attachment_len,
                    ) {
                        topic_sample.identity = identity;
                        semantic_events.push(ZenohSemanticEvent::TopicSample(topic_sample));
                        resolved = true;
                    }
                }
                if !resolved && let Some(identity) = identity {
                    semantic_events.push(ZenohSemanticEvent::UnresolvedTopicSample(
                        ZenohUnresolvedTopicSample {
                            wire_expr: wire_expr.clone(),
                            payload: payload.clone(),
                            payload_len: *payload_len,
                            identity,
                            attachment_len: *attachment_len,
                        },
                    ));
                }
            }
            _ => {}
        }
    }

    fn process_declare_body(
        &mut self,
        direction: ZenohFlowDirection,
        body: &ZenohDeclareBody,
        semantic_events: &mut Vec<ZenohSemanticEvent>,
    ) {
        match body {
            ZenohDeclareBody::DeclareKeyExpr { id, wire_expr } => {
                if let Ok(expr) = self.resolve_wire_expr_by_direction(direction, wire_expr) {
                    self.ensure_keyexpr_table(direction.protocol, direction.flow)
                        .insert(*id, expr);
                }
            }
            ZenohDeclareBody::UndeclareKeyExpr { id } => {
                if let Some(table) = self.keyexpr_tables.get_mut(&direction) {
                    table.remove(*id);
                }
            }
            ZenohDeclareBody::DeclareToken { id, wire_expr } => {
                if let Ok(expr) = self.resolve_wire_expr_by_direction(direction, wire_expr) {
                    let entity = parse_ros_liveliness_keyexpr(&expr).ok();
                    let old_entry = self
                        .ensure_token_table(direction.protocol, direction.flow)
                        .insert(*id, expr, entity.clone());

                    let old_keyexpr = old_entry
                        .as_ref()
                        .and_then(|entry| entry.entity.as_ref())
                        .map(|entity| entity.keyexpr.as_str());
                    let new_keyexpr = entity.as_ref().map(|entity| entity.keyexpr.as_str());
                    if old_keyexpr == new_keyexpr {
                        return;
                    }

                    if let Some(old_entity) = old_entry.and_then(|entry| entry.entity) {
                        if let Some(entity) = self.graph.remove(&old_entity.keyexpr) {
                            semantic_events.push(ZenohSemanticEvent::RosEntityUndiscovered(entity));
                        }
                    }
                    if let Some(entity) = entity {
                        if self.graph.insert(entity.clone()) {
                            semantic_events.push(ZenohSemanticEvent::RosEntityDiscovered(entity));
                        }
                    }
                }
            }
            ZenohDeclareBody::UndeclareToken { id } => {
                if let Some(table) = self.token_tables.get_mut(&direction) {
                    if let Some(entry) = table.remove(*id) {
                        if let Some(entity) = entry.entity {
                            debug_assert_eq!(entry.keyexpr, entity.keyexpr);
                            if let Some(entity) = self.graph.remove(&entity.keyexpr) {
                                semantic_events
                                    .push(ZenohSemanticEvent::RosEntityUndiscovered(entity));
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn remove_token_table(&mut self, direction: ZenohFlowDirection) -> Vec<ZenohSemanticEvent> {
        let Some(table) = self.token_tables.remove(&direction) else {
            return Vec::new();
        };
        let mut events = Vec::new();
        for entry in table.entries.into_values() {
            if let Some(entity) = entry.entity {
                debug_assert_eq!(entry.keyexpr, entity.keyexpr);
                if let Some(entity) = self.graph.remove(&entity.keyexpr) {
                    events.push(ZenohSemanticEvent::RosEntityUndiscovered(entity));
                }
            }
        }
        events
    }

    pub fn resolve_wire_expr(
        &self,
        protocol: TransportProtocol,
        flow: FlowTuple,
        wire_expr: &ZenohWireExpr,
    ) -> std::result::Result<String, ZenohWireExprResolutionError> {
        self.resolve_wire_expr_by_direction(ZenohFlowDirection::new(protocol, flow), wire_expr)
    }

    fn resolve_wire_expr_by_direction(
        &self,
        direction: ZenohFlowDirection,
        wire_expr: &ZenohWireExpr,
    ) -> std::result::Result<String, ZenohWireExprResolutionError> {
        if wire_expr.scope == 0 {
            return Ok(wire_expr.suffix.clone());
        }
        let direction = match wire_expr.mapping {
            Mapping::Sender => direction,
            Mapping::Receiver => direction.reverse(),
        };
        self.keyexpr_tables
            .get(&direction)
            .and_then(|table| table.resolve(wire_expr))
            .ok_or(ZenohWireExprResolutionError::UnknownScope(wire_expr.scope))
    }

    #[cfg(test)]
    fn keyexpr_table_count(&self) -> usize {
        self.keyexpr_tables.len()
    }

    #[cfg(test)]
    fn token_table_count(&self) -> usize {
        self.token_tables.len()
    }

    #[cfg(test)]
    fn token_entry(&self, protocol: TransportProtocol, flow: FlowTuple, id: u32) -> Option<&str> {
        self.token_tables
            .get(&ZenohFlowDirection::new(protocol, flow))?
            .entries
            .get(&id)
            .map(|entry| entry.keyexpr.as_str())
    }

    #[cfg(test)]
    fn graph_len(&self) -> usize {
        self.graph.len()
    }
}

impl ZenohRosGraph {
    fn insert(&mut self, entity: ZenohRosLivelinessEntity) -> bool {
        let count = self.ref_counts.entry(entity.keyexpr.clone()).or_insert(0);
        *count += 1;
        if *count == 1 {
            self.entities.insert(entity.keyexpr.clone(), entity);
            true
        } else {
            false
        }
    }

    fn remove(&mut self, keyexpr: &str) -> Option<ZenohRosLivelinessEntity> {
        let count = self.ref_counts.get_mut(keyexpr)?;
        *count -= 1;
        if *count == 0 {
            self.ref_counts.remove(keyexpr);
            self.entities.remove(keyexpr)
        } else {
            None
        }
    }

    pub fn len(&self) -> usize {
        self.entities.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entities.is_empty()
    }
}

impl ZenohKeyExprTable {
    pub fn insert(&mut self, id: u16, expr: String) {
        self.entries.insert(id, expr);
    }

    pub fn remove(&mut self, id: u16) {
        self.entries.remove(&id);
    }

    pub fn resolve(&self, wire_expr: &ZenohWireExpr) -> Option<String> {
        if wire_expr.scope == 0 {
            return Some(wire_expr.suffix.clone());
        }

        let prefix = self.entries.get(&wire_expr.scope)?;
        Some(format!("{prefix}{}", wire_expr.suffix))
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

pub fn parse_ros_liveliness_keyexpr(
    keyexpr: &str,
) -> std::result::Result<ZenohRosLivelinessEntity, ZenohRosLivelinessParseError> {
    let parts = keyexpr.split('/').collect::<Vec<_>>();
    const NODE_PART_COUNT: usize = 9;
    const TOPIC_PART_COUNT: usize = 13;

    if parts.len() < NODE_PART_COUNT {
        return Err(ZenohRosLivelinessParseError::TooFewParts {
            actual: parts.len(),
            minimum: NODE_PART_COUNT,
        });
    }
    if parts.iter().any(|part| part.is_empty()) {
        return Err(ZenohRosLivelinessParseError::EmptyPart);
    }
    if parts[0] != ROS2_LIVELINESS_PREFIX {
        return Err(ZenohRosLivelinessParseError::MissingPrefix);
    }

    let domain_id = parts[1]
        .parse::<u64>()
        .map_err(|_| ZenohRosLivelinessParseError::InvalidDomainId(parts[1].to_string()))?;
    let kind = parse_ros_entity_kind(parts[5])?;

    let node = ZenohRosNodeInfo {
        domain_id,
        zid: parts[2].to_string(),
        node_id: parts[3].to_string(),
        entity_id: parts[4].to_string(),
        enclave: demangle_ros_name(parts[6]),
        namespace: demangle_ros_name(parts[7]),
        node_name: demangle_ros_name(parts[8]),
    };

    let topic = if kind == ZenohRosEntityKind::Node {
        None
    } else {
        if parts.len() < TOPIC_PART_COUNT {
            return Err(ZenohRosLivelinessParseError::MissingTopicInfo);
        }
        Some(ZenohRosTopicInfo {
            name: demangle_ros_name(parts[9]),
            type_name: demangle_ros_name(parts[10]),
            type_hash: demangle_ros_name(parts[11]),
            qos: parts[12].to_string(),
            backends: parts[13..]
                .iter()
                .map(|backend| demangle_ros_name(backend))
                .collect(),
        })
    };

    Ok(ZenohRosLivelinessEntity {
        keyexpr: keyexpr.to_string(),
        kind,
        node,
        topic,
    })
}

pub fn parse_ros_topic_sample_keyexpr(
    keyexpr: &str,
    payload: Bytes,
    payload_len: usize,
    attachment_len: Option<usize>,
) -> std::result::Result<ZenohRosTopicSample, ZenohRosTopicKeyexprParseError> {
    let parts = keyexpr.split('/').collect::<Vec<_>>();
    const TOPIC_SAMPLE_PART_COUNT: usize = 4;

    if parts.len() < TOPIC_SAMPLE_PART_COUNT {
        return Err(ZenohRosTopicKeyexprParseError::TooFewParts {
            actual: parts.len(),
            minimum: TOPIC_SAMPLE_PART_COUNT,
        });
    }
    if parts.iter().any(|part| part.is_empty()) {
        return Err(ZenohRosTopicKeyexprParseError::EmptyPart);
    }

    let domain_id = parts[0]
        .parse::<u64>()
        .map_err(|_| ZenohRosTopicKeyexprParseError::InvalidDomainId(parts[0].to_string()))?;
    let topic_path = demangle_ros_name(&parts[1..parts.len() - 2].join("/"));
    let topic_name = format!("/{}", topic_path.trim_start_matches('/'));

    Ok(ZenohRosTopicSample {
        keyexpr: keyexpr.to_string(),
        domain_id,
        topic_name,
        type_name: demangle_ros_name(parts[parts.len() - 2]),
        type_hash: demangle_ros_name(parts[parts.len() - 1]),
        payload,
        payload_len,
        identity: None,
        attachment_len,
    })
}

pub fn rmw_zenoh_topic_keyexpr(
    domain_id: u64,
    topic_name: &str,
    type_name: &str,
    type_hash: &str,
) -> String {
    let topic_key = topic_name.trim_matches('/').replace('/', "%");
    let type_key = type_name.replace('/', "%");
    let type_hash_key = type_hash.replace('/', "%");
    format!("{domain_id}/{topic_key}/{type_key}/{type_hash_key}")
}

pub fn parse_rmw_zenoh_attachment_identity(attachment: &[u8]) -> Option<ZenohRosSampleIdentity> {
    let mut reader = ZenohAttachmentReader::new(attachment);
    let sequence_number = reader.read_i64()?;
    if sequence_number < 0 {
        return None;
    }
    let _source_timestamp = reader.read_i64()?;
    let gid_len = reader.read_zint()?;
    if gid_len != TOPIC_GID_LEN as u64 {
        return None;
    }
    let source_gid = TopicGid::new(reader.read_array::<TOPIC_GID_LEN>()?);

    Some(ZenohRosSampleIdentity {
        source_gid,
        sequence_number,
    })
}

pub fn rmw_zenoh_entity_gid(entity: &ZenohRosLivelinessEntity) -> TopicGid {
    let backend_count = entity
        .topic
        .as_ref()
        .map_or(0, |topic| topic.backends.len());
    let mut base_keyexpr = entity.keyexpr.as_str();
    for _ in 0..backend_count {
        if let Some((prefix, _)) = base_keyexpr.rsplit_once('/') {
            base_keyexpr = prefix;
        }
    }
    rmw_zenoh_keyexpr_gid(base_keyexpr)
}

fn rmw_zenoh_keyexpr_gid(liveliness_keyexpr: &str) -> TopicGid {
    let hash = xxhash_rust::xxh3::xxh3_128(liveliness_keyexpr.as_bytes());
    let mut bytes = [0u8; TOPIC_GID_LEN];
    bytes[..8].copy_from_slice(&(hash as u64).to_ne_bytes());
    bytes[8..].copy_from_slice(&((hash >> 64) as u64).to_ne_bytes());
    TopicGid::new(bytes)
}

struct ZenohAttachmentReader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> ZenohAttachmentReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn read_i64(&mut self) -> Option<i64> {
        Some(i64::from_le_bytes(self.read_array::<8>()?))
    }

    fn read_array<const N: usize>(&mut self) -> Option<[u8; N]> {
        let end = self.offset.checked_add(N)?;
        let bytes = self.bytes.get(self.offset..end)?;
        self.offset = end;
        bytes.try_into().ok()
    }

    fn read_zint(&mut self) -> Option<u64> {
        let mut value = 0u64;
        let mut shift = 0u32;
        for index in 0..9 {
            let byte = *self.bytes.get(self.offset)?;
            self.offset += 1;
            if index == 8 {
                value |= u64::from(byte) << shift;
                return Some(value);
            }
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Some(value);
            }
            shift += 7;
        }
        None
    }
}

fn parse_ros_entity_kind(
    value: &str,
) -> std::result::Result<ZenohRosEntityKind, ZenohRosLivelinessParseError> {
    match value {
        "NN" => Ok(ZenohRosEntityKind::Node),
        "MP" => Ok(ZenohRosEntityKind::Publisher),
        "MS" => Ok(ZenohRosEntityKind::Subscription),
        "SS" => Ok(ZenohRosEntityKind::ServiceServer),
        "SC" => Ok(ZenohRosEntityKind::ServiceClient),
        other => Err(ZenohRosLivelinessParseError::InvalidEntityKind(
            other.to_string(),
        )),
    }
}

fn demangle_ros_name(value: &str) -> String {
    value.replace('%', "/")
}

impl ZenohTokenTable {
    fn insert(
        &mut self,
        id: u32,
        keyexpr: String,
        entity: Option<ZenohRosLivelinessEntity>,
    ) -> Option<ZenohTokenEntry> {
        self.entries.insert(id, ZenohTokenEntry { keyexpr, entity })
    }

    fn remove(&mut self, id: u32) -> Option<ZenohTokenEntry> {
        self.entries.remove(&id)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl Default for ZenohTcpFlowState {
    fn default() -> Self {
        Self {
            aligned: false,
            next_seq: None,
            segments: Vec::new(),
            queued_segment_bytes: 0,
            stream_buffer: Vec::new(),
            ip_fragment_count: 1,
            was_ip_fragmented: false,
        }
    }
}

impl ZenohTcpFlowState {
    fn try_align_from_segment(
        &mut self,
        packet: &CapturedTransportPacket,
        payload_seq: u32,
        decode_batch: DecodeBatchFn,
    ) -> Vec<ZenohEvent> {
        let mut buffer = packet.payload.to_vec();
        let events = frame_batches(
            &mut buffer,
            packet,
            packet.ip_fragment_count,
            packet.was_ip_fragmented,
            decode_batch,
        );
        if events.decode_failed || events.events.is_empty() {
            return Vec::new();
        }

        self.aligned = true;
        self.next_seq = Some(payload_seq.wrapping_add(packet.payload.len() as u32));
        self.segments.clear();
        self.queued_segment_bytes = 0;
        self.stream_buffer = buffer;
        if self.stream_buffer.is_empty() {
            self.ip_fragment_count = 1;
            self.was_ip_fragmented = false;
        } else {
            self.ip_fragment_count = packet.ip_fragment_count;
            self.was_ip_fragmented = packet.was_ip_fragmented;
        }

        events.events
    }

    fn reset_alignment(&mut self) {
        self.aligned = false;
        self.next_seq = None;
        self.segments.clear();
        self.queued_segment_bytes = 0;
        self.stream_buffer.clear();
        self.ip_fragment_count = 1;
        self.was_ip_fragmented = false;
    }

    fn insert_segment(&mut self, seq: u32, bytes: Bytes) -> bool {
        if bytes.is_empty() {
            return true;
        }

        if let Some(next_seq) = self.next_seq {
            let end = seq.wrapping_add(bytes.len() as u32);
            if seq_lte(end, next_seq) {
                return true;
            }
        }

        if self
            .segments
            .iter()
            .any(|segment| segment.seq == seq && segment.bytes.len() >= bytes.len())
        {
            return true;
        }

        let Some(queued_segment_bytes) = self.queued_segment_bytes.checked_add(bytes.len()) else {
            return false;
        };
        if queued_segment_bytes > ZENOH_TCP_REASSEMBLY_CAPACITY
            || self.segments.len() >= ZENOH_TCP_REASSEMBLY_MAX_SEGMENTS
        {
            return false;
        }

        let insert_at = self
            .segments
            .partition_point(|segment| seq_before_or_equal(segment.seq, seq));
        self.segments.insert(insert_at, TcpSegment { seq, bytes });
        self.queued_segment_bytes = queued_segment_bytes;
        true
    }

    fn flush_contiguous(&mut self) {
        loop {
            let Some(next_seq) = self.next_seq else {
                return;
            };
            let Some(first) = self.segments.first() else {
                return;
            };
            if seq_after(first.seq, next_seq) {
                return;
            }

            let segment = self.segments.remove(0);
            self.queued_segment_bytes = self
                .queued_segment_bytes
                .saturating_sub(segment.bytes.len());
            let segment_end = segment.seq.wrapping_add(segment.bytes.len() as u32);
            if seq_lte(segment_end, next_seq) {
                continue;
            }

            let offset = next_seq.wrapping_sub(segment.seq) as usize;
            if offset >= segment.bytes.len() {
                continue;
            }

            let contiguous = &segment.bytes[offset..];
            self.stream_buffer.extend_from_slice(contiguous);
            self.next_seq = Some(next_seq.wrapping_add(contiguous.len() as u32));
        }
    }
}

fn tcp_payload_sequence(tcp: TcpSegmentInfo) -> u32 {
    tcp.sequence
        .wrapping_add(if has_flag(tcp, TCP_FLAG_SYN) { 1 } else { 0 })
}

fn has_flag(tcp: TcpSegmentInfo, flag: u16) -> bool {
    (tcp.flags & flag) != 0
}

fn seq_before(left: u32, right: u32) -> bool {
    (left.wrapping_sub(right) as i32) < 0
}

fn seq_after(left: u32, right: u32) -> bool {
    seq_before(right, left)
}

fn seq_before_or_equal(left: u32, right: u32) -> bool {
    left == right || seq_before(left, right)
}

fn seq_lte(left: u32, right: u32) -> bool {
    seq_before_or_equal(left, right)
}

struct FrameBatchOutcome {
    events: Vec<ZenohEvent>,
    decode_failed: bool,
}

fn frame_batches(
    buffer: &mut Vec<u8>,
    packet: &CapturedTransportPacket,
    ip_fragment_count: u32,
    was_ip_fragmented: bool,
    decode_batch: DecodeBatchFn,
) -> FrameBatchOutcome {
    let mut events = Vec::new();
    loop {
        if buffer.len() < TCP_BATCH_HEADER_LEN {
            break;
        }

        let batch_len = usize::from(u16::from_le_bytes([buffer[0], buffer[1]]));
        let total_len = TCP_BATCH_HEADER_LEN + batch_len;
        if buffer.len() < total_len {
            break;
        }

        if batch_len > 0 {
            let batch_payload = &buffer[TCP_BATCH_HEADER_LEN..total_len];
            let Some(messages) = decode_batch(batch_payload) else {
                return FrameBatchOutcome {
                    events,
                    decode_failed: true,
                };
            };
            let payload = Bytes::copy_from_slice(&buffer[TCP_BATCH_HEADER_LEN..total_len]);
            events.push(ZenohEvent::Batch(ZenohBatch {
                socket_timestamp: packet.socket_timestamp,
                frame_len: packet.frame_len,
                direction: packet.direction,
                protocol: packet.protocol,
                flow: packet.flow,
                payload,
                messages,
                semantic_events: Vec::new(),
                ip_fragment_count,
                was_ip_fragmented,
            }));
        }
        buffer.drain(0..total_len);
    }

    FrameBatchOutcome {
        events,
        decode_failed: false,
    }
}

fn decode_zenoh_batch(payload: &[u8]) -> Option<Vec<ZenohDecodedMessage>> {
    let mut rbatch = new_rbatch(payload)?;
    let mut messages = Vec::new();
    while !rbatch.is_empty() {
        let Ok((msg, len)): Result<(TransportMessage, BatchSize), _> = rbatch.decode() else {
            return None;
        };
        if len == 0 {
            return None;
        }
        messages.push(decode_transport_message(&msg));
    }
    (!messages.is_empty()).then_some(messages)
}

fn new_rbatch(payload: &[u8]) -> Option<RBatch<ZSlice>> {
    let zslice = ZSlice::from(payload.to_vec());
    let config = BatchConfig {
        mtu: BatchSize::MAX,
        is_streamed: false,
        is_compression: false,
    };
    let mut rbatch = RBatch::new(config, zslice);
    rbatch.initialize(|| vec![0; config.mtu as usize]).ok()?;
    Some(rbatch)
}

fn decode_transport_message(msg: &TransportMessage) -> ZenohDecodedMessage {
    match &msg.body {
        TransportBody::OAM(_) => ZenohDecodedMessage::Oam,
        TransportBody::InitSyn(init) => ZenohDecodedMessage::InitSyn {
            frame_sn_resolution: init.resolution.get(Field::FrameSN),
        },
        TransportBody::InitAck(init) => ZenohDecodedMessage::InitAck {
            frame_sn_resolution: init.resolution.get(Field::FrameSN),
        },
        TransportBody::OpenSyn(_) => ZenohDecodedMessage::OpenSyn,
        TransportBody::OpenAck(_) => ZenohDecodedMessage::OpenAck,
        TransportBody::Close(_) => ZenohDecodedMessage::Close,
        TransportBody::KeepAlive(_) => ZenohDecodedMessage::KeepAlive,
        TransportBody::Frame(frame) => ZenohDecodedMessage::Frame {
            reliability: format!("{:?}", frame.reliability),
            sequence_number: Some(format!("{:?}", frame.sn)),
            messages: frame.payload.iter().map(decode_network_message).collect(),
        },
        TransportBody::Fragment(fragment) => ZenohDecodedMessage::Fragment {
            reliability: fragment.reliability.clone(),
            sequence_number: fragment.sn,
            priority: fragment.ext_qos.priority(),
            more: fragment.more,
            first: fragment.ext_first.is_some(),
            drop: fragment.ext_drop.is_some(),
            payload: Bytes::copy_from_slice(fragment.payload.as_ref()),
        },
        TransportBody::Join(join) => ZenohDecodedMessage::Join {
            frame_sn_resolution: join.resolution.get(Field::FrameSN),
        },
    }
}

fn decode_fragmented_network_message(
    reliability: Reliability,
    payload: Vec<u8>,
) -> Option<NetworkMessage> {
    let mut zbuf = ZBuf::empty();
    zbuf.push_zslice(ZSlice::from(payload));
    let mut reader = zbuf.reader();
    Zenoh080Reliability::new(reliability).read(&mut reader).ok()
}

fn decode_network_message(msg: &NetworkMessage) -> ZenohNetworkMessage {
    match &msg.body {
        NetworkBody::OAM(_) => ZenohNetworkMessage::Oam,
        NetworkBody::Push(push) => ZenohNetworkMessage::Push {
            reliability: format!("{:?}", msg.reliability),
            wire_expr: ZenohWireExpr::from(&push.wire_expr),
            body: decode_push_body(&push.payload),
        },
        NetworkBody::Request(request) => ZenohNetworkMessage::Request {
            reliability: format!("{:?}", msg.reliability),
            id: format!("{:?}", request.id),
            wire_expr: ZenohWireExpr::from(&request.wire_expr),
            body: format!("{:?}", request.payload),
        },
        NetworkBody::Response(response) => ZenohNetworkMessage::Response {
            reliability: format!("{:?}", msg.reliability),
            request_id: format!("{:?}", response.rid),
            wire_expr: ZenohWireExpr::from(&response.wire_expr),
            body: format!("{:?}", response.payload),
        },
        NetworkBody::ResponseFinal(_) => ZenohNetworkMessage::ResponseFinal,
        NetworkBody::Interest(interest) => ZenohNetworkMessage::Interest {
            reliability: format!("{:?}", msg.reliability),
            id: format!("{:?}", interest.id),
            wire_expr: interest.wire_expr.as_ref().map(ZenohWireExpr::from),
        },
        NetworkBody::Declare(declare) => ZenohNetworkMessage::Declare {
            reliability: format!("{:?}", msg.reliability),
            body: decode_declare_body(&declare.body),
        },
    }
}

fn decode_push_body(body: &PushBody) -> ZenohPushBody {
    match body {
        PushBody::Put(put) => ZenohPushBody::Put {
            encoding: format!("{:?}", put.encoding),
            payload: Bytes::copy_from_slice(put.payload.contiguous().as_ref()),
            payload_len: put.payload.len(),
            attachment: put
                .ext_attachment
                .as_ref()
                .map(|a| Bytes::copy_from_slice(a.buffer.contiguous().as_ref())),
            attachment_len: put.ext_attachment.as_ref().map(|a| a.buffer.len()),
            is_shm: put.ext_shm.is_some(),
        },
        PushBody::Del(del) => ZenohPushBody::Del {
            attachment_len: del.ext_attachment.as_ref().map(|a| a.buffer.len()),
        },
    }
}

fn decode_declare_body(body: &DeclareBody) -> ZenohDeclareBody {
    match body {
        DeclareBody::DeclareKeyExpr(decl) => ZenohDeclareBody::DeclareKeyExpr {
            id: decl.id,
            wire_expr: ZenohWireExpr::from(&decl.wire_expr),
        },
        DeclareBody::UndeclareKeyExpr(decl) => ZenohDeclareBody::UndeclareKeyExpr { id: decl.id },
        DeclareBody::DeclareSubscriber(decl) => ZenohDeclareBody::DeclareSubscriber {
            id: decl.id,
            wire_expr: ZenohWireExpr::from(&decl.wire_expr),
        },
        DeclareBody::UndeclareSubscriber(decl) => {
            ZenohDeclareBody::UndeclareSubscriber { id: decl.id }
        }
        DeclareBody::DeclareQueryable(decl) => ZenohDeclareBody::DeclareQueryable {
            id: decl.id,
            wire_expr: ZenohWireExpr::from(&decl.wire_expr),
            complete: decl.ext_info.complete,
            distance: decl.ext_info.distance,
        },
        DeclareBody::UndeclareQueryable(decl) => {
            ZenohDeclareBody::UndeclareQueryable { id: decl.id }
        }
        DeclareBody::DeclareToken(decl) => ZenohDeclareBody::DeclareToken {
            id: decl.id,
            wire_expr: ZenohWireExpr::from(&decl.wire_expr),
        },
        DeclareBody::UndeclareToken(decl) => ZenohDeclareBody::UndeclareToken { id: decl.id },
        DeclareBody::DeclareFinal(_) => ZenohDeclareBody::DeclareFinal,
    }
}

impl From<&WireExpr<'_>> for ZenohWireExpr {
    fn from(value: &WireExpr<'_>) -> Self {
        Self {
            scope: value.scope,
            suffix: value.suffix.to_string(),
            mapping: value.mapping,
        }
    }
}

impl fmt::Display for ZenohWireExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.scope == 0 {
            write!(f, "{}", self.suffix)
        } else {
            write!(f, "{}:{:?}:{}", self.scope, self.mapping, self.suffix)
        }
    }
}

impl fmt::Display for ZenohDecodedMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ZenohDecodedMessage::Oam => write!(f, "OAM"),
            ZenohDecodedMessage::InitSyn {
                frame_sn_resolution,
            } => write!(f, "InitSyn frame_sn_resolution={frame_sn_resolution:?}"),
            ZenohDecodedMessage::InitAck {
                frame_sn_resolution,
            } => write!(f, "InitAck frame_sn_resolution={frame_sn_resolution:?}"),
            ZenohDecodedMessage::OpenSyn => write!(f, "OpenSyn"),
            ZenohDecodedMessage::OpenAck => write!(f, "OpenAck"),
            ZenohDecodedMessage::Close => write!(f, "Close"),
            ZenohDecodedMessage::KeepAlive => write!(f, "KeepAlive"),
            ZenohDecodedMessage::Frame {
                reliability,
                sequence_number,
                messages,
            } => {
                let payload = messages
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; ");
                write!(
                    f,
                    "Frame reliability={} sn={} [{}]",
                    reliability,
                    sequence_number.as_deref().unwrap_or("None"),
                    payload
                )
            }
            ZenohDecodedMessage::Fragment {
                reliability,
                sequence_number,
                priority,
                more,
                first,
                drop,
                payload,
            } => write!(
                f,
                "Fragment reliability={reliability:?} sn={sequence_number:?} priority={priority:?} more={more} first={first} drop={drop} payload_len={}",
                payload.len()
            ),
            ZenohDecodedMessage::Join {
                frame_sn_resolution,
            } => write!(f, "Join frame_sn_resolution={frame_sn_resolution:?}"),
        }
    }
}

impl fmt::Display for ZenohNetworkMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ZenohNetworkMessage::Oam => write!(f, "OAM"),
            ZenohNetworkMessage::Push {
                reliability,
                wire_expr,
                body,
            } => write!(
                f,
                "Push reliability={} wire_expr={} {}",
                reliability, wire_expr, body
            ),
            ZenohNetworkMessage::Request {
                reliability,
                id,
                wire_expr,
                body,
            } => write!(
                f,
                "Request reliability={} id={} wire_expr={} body={}",
                reliability, id, wire_expr, body
            ),
            ZenohNetworkMessage::Response {
                reliability,
                request_id,
                wire_expr,
                body,
            } => write!(
                f,
                "Response reliability={} rid={} wire_expr={} body={}",
                reliability, request_id, wire_expr, body
            ),
            ZenohNetworkMessage::ResponseFinal => write!(f, "ResponseFinal"),
            ZenohNetworkMessage::Interest {
                reliability,
                id,
                wire_expr,
            } => write!(
                f,
                "Interest reliability={} id={} wire_expr={:?}",
                reliability, id, wire_expr
            ),
            ZenohNetworkMessage::Declare { reliability, body } => {
                write!(f, "Declare reliability={} {}", reliability, body)
            }
        }
    }
}

impl fmt::Display for ZenohPushBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ZenohPushBody::Put {
                encoding,
                payload: _,
                payload_len,
                attachment: _,
                attachment_len,
                is_shm,
            } => write!(
                f,
                "Put encoding={} payload_len={} attachment_len={} shm={}",
                encoding,
                payload_len,
                attachment_len.unwrap_or(0),
                is_shm
            ),
            ZenohPushBody::Del { attachment_len } => {
                write!(f, "Del attachment_len={}", attachment_len.unwrap_or(0))
            }
        }
    }
}

impl fmt::Display for ZenohDeclareBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ZenohDeclareBody::DeclareKeyExpr { id, wire_expr } => {
                write!(f, "DeclareKeyExpr id={} expr={}", id, wire_expr)
            }
            ZenohDeclareBody::UndeclareKeyExpr { id } => {
                write!(f, "UndeclareKeyExpr id={}", id)
            }
            ZenohDeclareBody::DeclareSubscriber { id, wire_expr } => {
                write!(f, "DeclareSubscriber id={} expr={}", id, wire_expr)
            }
            ZenohDeclareBody::UndeclareSubscriber { id } => {
                write!(f, "UndeclareSubscriber id={}", id)
            }
            ZenohDeclareBody::DeclareQueryable {
                id,
                wire_expr,
                complete,
                distance,
            } => write!(
                f,
                "DeclareQueryable id={} expr={} complete={} distance={}",
                id, wire_expr, complete, distance
            ),
            ZenohDeclareBody::UndeclareQueryable { id } => {
                write!(f, "UndeclareQueryable id={}", id)
            }
            ZenohDeclareBody::DeclareToken { id, wire_expr } => {
                write!(f, "DeclareToken id={} expr={}", id, wire_expr)
            }
            ZenohDeclareBody::UndeclareToken { id } => {
                write!(f, "UndeclareToken id={}", id)
            }
            ZenohDeclareBody::DeclareFinal => write!(f, "DeclareFinal"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use ros2probe_common::IpAddr;

    use super::*;

    #[test]
    fn fragment_memory_budget_is_shared_across_processors() {
        let budget = Arc::new(ZenohFragmentMemoryBudget::new(6));
        let timeout = Duration::from_secs(30);
        let mut first = ZenohProcessor::new_with_decoder_and_fragment_limits(
            4,
            decode_test_batch,
            Arc::clone(&budget),
            timeout,
        );
        let mut second = ZenohProcessor::new_with_decoder_and_fragment_limits(
            4,
            decode_test_batch,
            Arc::clone(&budget),
            timeout,
        );
        let now = Instant::now();

        assert!(
            first
                .accept_fragment(
                    TransportProtocol::Udp,
                    flow(32100, 7447),
                    Reliability::Reliable,
                    Priority::DEFAULT,
                    1,
                    true,
                    true,
                    false,
                    &Bytes::from_static(b"1234"),
                    now,
                )
                .is_none()
        );
        assert_eq!(budget.used(), 4);

        assert!(
            second
                .accept_fragment(
                    TransportProtocol::Udp,
                    flow(32101, 7447),
                    Reliability::Reliable,
                    Priority::DEFAULT,
                    1,
                    true,
                    true,
                    false,
                    &Bytes::from_static(b"5678"),
                    now,
                )
                .is_none()
        );
        assert!(second.fragment_flows.is_empty());
        assert_eq!(budget.used(), 4);

        drop(first);
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn incomplete_fragment_flow_releases_budget_after_timeout() {
        let budget = Arc::new(ZenohFragmentMemoryBudget::new(16));
        let timeout = Duration::from_secs(30);
        let mut processor = ZenohProcessor::new_with_decoder_and_fragment_limits(
            4,
            decode_test_batch,
            Arc::clone(&budget),
            timeout,
        );
        let now = Instant::now();

        assert!(
            processor
                .accept_fragment(
                    TransportProtocol::Udp,
                    flow(32100, 7447),
                    Reliability::Reliable,
                    Priority::DEFAULT,
                    1,
                    true,
                    true,
                    false,
                    &Bytes::from_static(b"fragment"),
                    now,
                )
                .is_none()
        );
        assert_eq!(budget.used(), 8);

        assert_eq!(processor.expire_inactive_fragments_at(now + timeout), 1);
        assert!(processor.fragment_flows.is_empty());
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn fragment_eviction_uses_active_state_age_after_flow_reuse() {
        fn add_fragment(processor: &mut ZenohProcessor, src_port: u16, now: Instant) {
            assert!(
                processor
                    .accept_fragment(
                        TransportProtocol::Udp,
                        flow(src_port, 7447),
                        Reliability::Reliable,
                        Priority::DEFAULT,
                        1,
                        true,
                        true,
                        false,
                        &Bytes::from_static(b"x"),
                        now,
                    )
                    .is_none()
            );
        }

        let budget = Arc::new(ZenohFragmentMemoryBudget::new(16));
        let mut processor = ZenohProcessor::new_with_decoder_and_fragment_limits(
            2,
            decode_test_batch,
            Arc::clone(&budget),
            Duration::from_secs(30),
        );
        let start = Instant::now();

        add_fragment(&mut processor, 32100, start);
        assert!(
            processor
                .accept_fragment(
                    TransportProtocol::Udp,
                    flow(32100, 7447),
                    Reliability::Reliable,
                    Priority::DEFAULT,
                    2,
                    true,
                    false,
                    true,
                    &Bytes::new(),
                    start,
                )
                .is_none()
        );
        add_fragment(&mut processor, 32101, start + Duration::from_secs(1));
        add_fragment(&mut processor, 32100, start + Duration::from_secs(2));
        add_fragment(&mut processor, 32102, start + Duration::from_secs(3));

        assert_eq!(processor.fragment_flows.len(), 2);
        assert!(processor.fragment_flows.keys().any(|key| {
            key.direction.flow.src_port == 32100 && key.direction.flow.dst_port == 7447
        }));
        assert!(processor.fragment_flows.keys().any(|key| {
            key.direction.flow.src_port == 32102 && key.direction.flow.dst_port == 7447
        }));
        assert!(
            !processor
                .fragment_flows
                .keys()
                .any(|key| key.direction.flow.src_port == 32101)
        );
        assert_eq!(budget.used(), 2);
    }

    #[test]
    fn fragment_sequence_wraps_at_negotiated_resolution() {
        let budget = Arc::new(ZenohFragmentMemoryBudget::new(16));
        let mut processor = ZenohProcessor::new_with_decoder_and_fragment_limits(
            4,
            decode_test_batch,
            Arc::clone(&budget),
            Duration::from_secs(30),
        );
        let now = Instant::now();
        let forward = flow(32100, 7447);
        let reverse = ZenohFlowDirection::new(TransportProtocol::Tcp, forward)
            .reverse()
            .flow;

        processor.expand_fragments(
            TransportProtocol::Tcp,
            forward,
            vec![ZenohDecodedMessage::InitAck {
                frame_sn_resolution: Bits::U8,
            }],
            now,
        );

        assert!(
            processor
                .accept_fragment(
                    TransportProtocol::Tcp,
                    reverse,
                    Reliability::Reliable,
                    Priority::DEFAULT,
                    u8::MAX.into(),
                    true,
                    true,
                    false,
                    &Bytes::from_static(b"a"),
                    now,
                )
                .is_none()
        );
        assert!(
            processor
                .accept_fragment(
                    TransportProtocol::Tcp,
                    reverse,
                    Reliability::Reliable,
                    Priority::DEFAULT,
                    0,
                    true,
                    false,
                    false,
                    &Bytes::from_static(b"b"),
                    now + Duration::from_millis(1),
                )
                .is_none()
        );

        let key = ZenohFragmentFlowKey {
            direction: ZenohFlowDirection::new(TransportProtocol::Tcp, reverse),
            reliability: Reliability::Reliable,
            priority: Priority::DEFAULT,
        };
        let state = processor.fragment_flows.get(&key).unwrap();
        assert_eq!(state.next_sn, Some(1));
        assert_eq!(state.payload, b"ab");
        assert_eq!(budget.used(), 2);
    }

    #[test]
    fn udp_payload_is_one_batch() {
        let mut processor = processor();
        let events = processor
            .process_packet(packet(TransportProtocol::Udp, 32100, 7447, b"udp-batch"))
            .unwrap();

        let [ZenohEvent::Batch(batch)] = events.as_slice() else {
            panic!("expected one UDP batch");
        };
        assert_eq!(batch.protocol, TransportProtocol::Udp);
        assert_eq!(batch.payload.as_ref(), b"udp-batch");
    }

    #[test]
    fn tcp_payload_is_framed_by_little_endian_batch_length() {
        let mut processor = processor();
        let events = processor
            .process_packet(tcp_packet(
                50123,
                7447,
                100,
                &[0x05, 0x00, b'h', b'e', b'l', b'l', b'o'],
            ))
            .unwrap();

        let [ZenohEvent::Batch(batch)] = events.as_slice() else {
            panic!("expected one TCP batch");
        };
        assert_eq!(batch.protocol, TransportProtocol::Tcp);
        assert_eq!(batch.payload.as_ref(), b"hello");
    }

    #[test]
    fn tcp_batch_can_span_multiple_packets_after_alignment() {
        let mut processor = processor();
        let events = processor
            .process_packet(tcp_packet(50123, 7447, 100, &[0x01, 0x00, b'a']))
            .unwrap();
        assert_eq!(events.len(), 1);

        let events = processor
            .process_packet(tcp_packet(50123, 7447, 103, &[0x05, 0x00, b'h']))
            .unwrap();
        assert!(events.is_empty());

        let events = processor
            .process_packet(tcp_packet(50123, 7447, 106, &[b'e', b'l', b'l', b'o']))
            .unwrap();
        let [ZenohEvent::Batch(batch)] = events.as_slice() else {
            panic!("expected completed TCP batch");
        };
        assert_eq!(batch.payload.as_ref(), b"hello");
    }

    #[test]
    fn tcp_packet_can_contain_multiple_batches() {
        let mut processor = processor();
        let events = processor
            .process_packet(tcp_packet(
                50123,
                7447,
                100,
                &[0x02, 0x00, b'o', b'k', 0x03, 0x00, b'y', b'e', b's'],
            ))
            .unwrap();

        assert_eq!(events.len(), 2);
        let ZenohEvent::Batch(first) = &events[0];
        let ZenohEvent::Batch(second) = &events[1];
        assert_eq!(first.payload.as_ref(), b"ok");
        assert_eq!(second.payload.as_ref(), b"yes");
    }

    #[test]
    fn tcp_alignment_keeps_trailing_partial_batch() {
        let mut processor = processor();
        let events = processor
            .process_packet(tcp_packet(
                50123,
                7447,
                100,
                &[0x01, 0x00, b'a', 0x05, 0x00, b'h'],
            ))
            .unwrap();
        assert_eq!(events.len(), 1);

        let events = processor
            .process_packet(tcp_packet(50123, 7447, 106, &[b'e', b'l', b'l', b'o']))
            .unwrap();
        let [ZenohEvent::Batch(batch)] = events.as_slice() else {
            panic!("expected completed trailing TCP batch");
        };
        assert_eq!(batch.payload.as_ref(), b"hello");
    }

    #[test]
    fn tcp_out_of_order_segments_wait_for_gap() {
        let mut processor = processor();
        let events = processor
            .process_packet(tcp_packet(50123, 7447, 100, &[0x01, 0x00, b'a']))
            .unwrap();
        assert_eq!(events.len(), 1);

        let events = processor
            .process_packet(tcp_packet(50123, 7447, 106, &[b'e', b'l', b'l', b'o']))
            .unwrap();
        assert!(events.is_empty());

        let events = processor
            .process_packet(tcp_packet(50123, 7447, 103, &[0x05, 0x00, b'h']))
            .unwrap();
        let [ZenohEvent::Batch(batch)] = events.as_slice() else {
            panic!("expected completed TCP batch");
        };
        assert_eq!(batch.payload.as_ref(), b"hello");
    }

    #[test]
    fn tcp_retransmission_duplicate_is_ignored() {
        let mut processor = processor();
        let events = processor
            .process_packet(tcp_packet(50123, 7447, 100, &[0x01, 0x00, b'a']))
            .unwrap();
        assert_eq!(events.len(), 1);

        let events = processor
            .process_packet(tcp_packet(50123, 7447, 103, &[0x05, 0x00, b'h']))
            .unwrap();
        assert!(events.is_empty());

        let events = processor
            .process_packet(tcp_packet(50123, 7447, 103, &[0x05, 0x00, b'h']))
            .unwrap();
        assert!(events.is_empty());

        let events = processor
            .process_packet(tcp_packet(50123, 7447, 106, &[b'e', b'l', b'l', b'o']))
            .unwrap();
        let [ZenohEvent::Batch(batch)] = events.as_slice() else {
            panic!("expected completed TCP batch");
        };
        assert_eq!(batch.payload.as_ref(), b"hello");
    }

    #[test]
    fn tcp_out_of_order_queue_has_a_memory_limit() {
        let mut state = ZenohTcpFlowState {
            aligned: true,
            next_seq: Some(100),
            ..Default::default()
        };

        assert!(
            !state.insert_segment(200, Bytes::from(vec![0; ZENOH_TCP_REASSEMBLY_CAPACITY + 1]),)
        );
        assert!(state.segments.is_empty());
        assert_eq!(state.queued_segment_bytes, 0);
    }

    #[test]
    fn tcp_unsynced_incomplete_segment_does_not_block_next_segment() {
        let mut processor = processor();
        let events = processor
            .process_packet(tcp_packet(50123, 7447, 100, &[0xff, 0x7f, b'x']))
            .unwrap();
        assert!(events.is_empty());

        let events = processor
            .process_packet(tcp_packet(50123, 7447, 103, &[0x02, 0x00, b'o', b'k']))
            .unwrap();
        let [ZenohEvent::Batch(batch)] = events.as_slice() else {
            panic!("expected next segment candidate to frame");
        };
        assert_eq!(batch.payload.as_ref(), b"ok");
    }

    #[test]
    fn empty_tcp_payload_is_ignored() {
        let mut processor = processor();
        let events = processor
            .process_packet(tcp_packet(50123, 7447, 100, b""))
            .unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn decoded_packets_create_protocol_scoped_keyexpr_tables() {
        let mut processor = processor();
        processor
            .process_packet(packet(TransportProtocol::Udp, 32100, 7447, b"udp-batch"))
            .unwrap();
        processor
            .process_packet(tcp_packet(32100, 7447, 100, &[0x01, 0x00, b'a']))
            .unwrap();

        assert_eq!(processor.semantic.keyexpr_table_count(), 2);
    }

    #[test]
    fn tcp_fin_removes_keyexpr_table_for_that_direction() {
        let mut processor = processor();
        processor
            .process_packet(tcp_packet(32100, 7447, 100, &[0x01, 0x00, b'a']))
            .unwrap();
        assert_eq!(processor.semantic.keyexpr_table_count(), 1);

        processor
            .process_packet(tcp_packet_with_flags(32100, 7447, 103, TCP_FLAG_FIN, b""))
            .unwrap();

        assert_eq!(processor.semantic.keyexpr_table_count(), 0);
        assert!(processor.tcp_flows.is_empty());
        assert!(processor.order.is_empty());
    }

    #[test]
    fn semantic_flow_state_is_bounded_for_udp_sessions() {
        let mut semantic = ZenohSemanticState::with_capacity(1);
        let first = flow(32100, 7447);
        let second = flow(32101, 7447);

        semantic.process_messages(
            TransportProtocol::Udp,
            first,
            &[declare(ZenohDeclareBody::DeclareToken {
                id: 1,
                wire_expr: wire_expr(0, "@ros2_lv/0/zid/1/1/NN/%/%/first"),
            })],
        );
        let events = semantic.process_messages(
            TransportProtocol::Udp,
            second,
            &[ZenohDecodedMessage::KeepAlive],
        );

        assert_eq!(semantic.keyexpr_table_count(), 1);
        assert_eq!(semantic.flow_order.len(), 1);
        assert_eq!(semantic.graph_len(), 0);
        assert!(matches!(
            events.as_slice(),
            [ZenohSemanticEvent::RosEntityUndiscovered(entity)]
                if entity.node.node_name == "first"
        ));
    }

    #[test]
    fn resolve_wire_expr_returns_global_suffix_without_table() {
        let processor = processor();
        let flow = flow(32100, 7447);
        let resolved = processor
            .semantic
            .resolve_wire_expr(TransportProtocol::Tcp, flow, &wire_expr(0, "@ros2_lv/0/a"))
            .unwrap();

        assert_eq!(resolved, "@ros2_lv/0/a");
    }

    #[test]
    fn resolve_wire_expr_joins_known_scope_and_suffix() {
        let mut processor = processor();
        let flow = flow(32100, 7447);
        let table = processor
            .semantic
            .ensure_keyexpr_table(TransportProtocol::Tcp, flow);
        table.insert(37, "@ros2_lv/0/session/".to_string());

        let resolved = processor
            .semantic
            .resolve_wire_expr(
                TransportProtocol::Tcp,
                flow,
                &wire_expr(37, "1/2/NN/%/%/talker"),
            )
            .unwrap();

        assert_eq!(resolved, "@ros2_lv/0/session/1/2/NN/%/%/talker");
    }

    #[test]
    fn resolve_wire_expr_receiver_mapping_uses_reverse_flow_table() {
        let mut processor = processor();
        let sender_to_receiver = flow(32100, 7447);
        let receiver_to_sender = flow(7447, 32100);
        let table = processor
            .semantic
            .ensure_keyexpr_table(TransportProtocol::Tcp, receiver_to_sender);
        table.insert(37, "0/chatter/".to_string());

        let resolved = processor
            .semantic
            .resolve_wire_expr(
                TransportProtocol::Tcp,
                sender_to_receiver,
                &wire_expr_with_mapping(
                    37,
                    "std_msgs::msg::dds_::String_/RIHS01_abcd",
                    Mapping::Receiver,
                ),
            )
            .unwrap();

        assert_eq!(
            resolved,
            "0/chatter/std_msgs::msg::dds_::String_/RIHS01_abcd"
        );
    }

    #[test]
    fn resolve_wire_expr_reports_unknown_scope() {
        let processor = processor();
        let flow = flow(32100, 7447);
        let err = processor
            .semantic
            .resolve_wire_expr(TransportProtocol::Tcp, flow, &wire_expr(37, "suffix"))
            .unwrap_err();

        assert_eq!(err, ZenohWireExprResolutionError::UnknownScope(37));
    }

    #[test]
    fn declare_keyexpr_updates_flow_table() {
        let mut processor = processor();
        let flow = flow(32100, 7447);
        processor.semantic.process_messages(
            TransportProtocol::Tcp,
            flow,
            &[declare(ZenohDeclareBody::DeclareKeyExpr {
                id: 37,
                wire_expr: wire_expr(0, "@ros2_lv/0/session/"),
            })],
        );

        let resolved = processor
            .semantic
            .resolve_wire_expr(TransportProtocol::Tcp, flow, &wire_expr(37, "node"))
            .unwrap();
        assert_eq!(resolved, "@ros2_lv/0/session/node");
    }

    #[test]
    fn declare_keyexpr_can_reference_existing_scope() {
        let mut processor = processor();
        let flow = flow(32100, 7447);
        processor.semantic.process_messages(
            TransportProtocol::Tcp,
            flow,
            &[declare(ZenohDeclareBody::DeclareKeyExpr {
                id: 10,
                wire_expr: wire_expr(0, "@ros2_lv/0/"),
            })],
        );
        processor.semantic.process_messages(
            TransportProtocol::Tcp,
            flow,
            &[declare(ZenohDeclareBody::DeclareKeyExpr {
                id: 37,
                wire_expr: wire_expr(10, "session/"),
            })],
        );

        let resolved = processor
            .semantic
            .resolve_wire_expr(TransportProtocol::Tcp, flow, &wire_expr(37, "node"))
            .unwrap();
        assert_eq!(resolved, "@ros2_lv/0/session/node");
    }

    #[test]
    fn undeclare_keyexpr_removes_flow_table_entry() {
        let mut processor = processor();
        let flow = flow(32100, 7447);
        processor.semantic.process_messages(
            TransportProtocol::Tcp,
            flow,
            &[declare(ZenohDeclareBody::DeclareKeyExpr {
                id: 37,
                wire_expr: wire_expr(0, "@ros2_lv/0/session/"),
            })],
        );
        processor.semantic.process_messages(
            TransportProtocol::Tcp,
            flow,
            &[declare(ZenohDeclareBody::UndeclareKeyExpr { id: 37 })],
        );

        let err = processor
            .semantic
            .resolve_wire_expr(TransportProtocol::Tcp, flow, &wire_expr(37, "node"))
            .unwrap_err();
        assert_eq!(err, ZenohWireExprResolutionError::UnknownScope(37));
    }

    #[test]
    fn declare_token_stores_resolved_keyexpr() {
        let mut processor = processor();
        let flow = flow(32100, 7447);
        processor.semantic.process_messages(
            TransportProtocol::Tcp,
            flow,
            &[declare(ZenohDeclareBody::DeclareKeyExpr {
                id: 37,
                wire_expr: wire_expr(0, "@ros2_lv/0/session/"),
            })],
        );
        let events = processor.semantic.process_messages(
            TransportProtocol::Tcp,
            flow,
            &[declare(ZenohDeclareBody::DeclareToken {
                id: 5,
                wire_expr: wire_expr(37, "1/2/NN/%/%/talker"),
            })],
        );

        assert_eq!(
            processor
                .semantic
                .token_entry(TransportProtocol::Tcp, flow, 5),
            Some("@ros2_lv/0/session/1/2/NN/%/%/talker")
        );
        assert_eq!(processor.semantic.graph_len(), 1);
        assert!(matches!(
            events.as_slice(),
            [ZenohSemanticEvent::RosEntityDiscovered(entity)]
                if entity.kind == ZenohRosEntityKind::Node
                    && entity.node.node_name == "talker"
        ));
    }

    #[test]
    fn undeclare_token_removes_token_entry() {
        let mut processor = processor();
        let flow = flow(32100, 7447);
        processor.semantic.process_messages(
            TransportProtocol::Tcp,
            flow,
            &[declare(ZenohDeclareBody::DeclareToken {
                id: 5,
                wire_expr: wire_expr(0, "@ros2_lv/0/session/1/2/NN/%/%/talker"),
            })],
        );
        let events = processor.semantic.process_messages(
            TransportProtocol::Tcp,
            flow,
            &[declare(ZenohDeclareBody::UndeclareToken { id: 5 })],
        );

        assert_eq!(
            processor
                .semantic
                .token_entry(TransportProtocol::Tcp, flow, 5),
            None
        );
        assert_eq!(processor.semantic.graph_len(), 0);
        assert!(matches!(
            events.as_slice(),
            [ZenohSemanticEvent::RosEntityUndiscovered(entity)]
                if entity.kind == ZenohRosEntityKind::Node
                    && entity.node.node_name == "talker"
        ));
    }

    #[test]
    fn close_removes_keyexpr_and_token_tables_for_flow() {
        let mut processor = processor();
        let flow = flow(32100, 7447);
        processor.semantic.process_messages(
            TransportProtocol::Tcp,
            flow,
            &[
                declare(ZenohDeclareBody::DeclareKeyExpr {
                    id: 37,
                    wire_expr: wire_expr(0, "@ros2_lv/0/session/"),
                }),
                declare(ZenohDeclareBody::DeclareToken {
                    id: 5,
                    wire_expr: wire_expr(37, "1/2/NN/%/%/talker"),
                }),
            ],
        );
        assert_eq!(processor.semantic.keyexpr_table_count(), 1);
        assert_eq!(processor.semantic.token_table_count(), 1);

        processor.semantic.process_messages(
            TransportProtocol::Tcp,
            flow,
            &[ZenohDecodedMessage::Close],
        );

        assert_eq!(processor.semantic.keyexpr_table_count(), 0);
        assert_eq!(processor.semantic.token_table_count(), 0);
        assert_eq!(processor.semantic.graph_len(), 0);
    }

    #[test]
    fn duplicate_ros_liveliness_tokens_are_ref_counted() {
        let mut processor = processor();
        let first_flow = flow(32100, 7447);
        let second_flow = flow(32101, 7447);
        let token = declare(ZenohDeclareBody::DeclareToken {
            id: 5,
            wire_expr: wire_expr(0, "@ros2_lv/0/session/1/2/NN/%/%/talker"),
        });

        let first_events = processor.semantic.process_messages(
            TransportProtocol::Tcp,
            first_flow,
            &[token.clone()],
        );
        let second_events =
            processor
                .semantic
                .process_messages(TransportProtocol::Tcp, second_flow, &[token]);
        assert_eq!(processor.semantic.graph_len(), 1);
        assert_eq!(first_events.len(), 1);
        assert!(second_events.is_empty());

        let first_close = processor.semantic.process_messages(
            TransportProtocol::Tcp,
            first_flow,
            &[ZenohDecodedMessage::Close],
        );
        assert!(first_close.is_empty());
        assert_eq!(processor.semantic.graph_len(), 1);

        let second_close = processor.semantic.process_messages(
            TransportProtocol::Tcp,
            second_flow,
            &[ZenohDecodedMessage::Close],
        );
        assert_eq!(processor.semantic.graph_len(), 0);
        assert!(matches!(
            second_close.as_slice(),
            [ZenohSemanticEvent::RosEntityUndiscovered(entity)]
                if entity.node.node_name == "talker"
        ));
    }

    #[test]
    fn parses_ros_liveliness_node_entity() {
        let entity =
            parse_ros_liveliness_keyexpr("@ros2_lv/0/zid/1/1/NN/%/%robot%ns/talker").unwrap();

        assert_eq!(entity.kind, ZenohRosEntityKind::Node);
        assert_eq!(entity.node.domain_id, 0);
        assert_eq!(entity.node.zid, "zid");
        assert_eq!(entity.node.node_id, "1");
        assert_eq!(entity.node.entity_id, "1");
        assert_eq!(entity.node.enclave, "/");
        assert_eq!(entity.node.namespace, "/robot/ns");
        assert_eq!(entity.node.node_name, "talker");
        assert!(entity.topic.is_none());
    }

    #[test]
    fn parses_ros_liveliness_publisher_entity() {
        let entity = parse_ros_liveliness_keyexpr(
            "@ros2_lv/2/zid/1/32/MP/%/%/talker/%chatter/std_msgs::msg::dds_::String_/RIHS01_abcd/1:2:3,10",
        )
        .unwrap();
        let topic = entity.topic.as_ref().unwrap();

        assert_eq!(entity.kind, ZenohRosEntityKind::Publisher);
        assert_eq!(entity.node.domain_id, 2);
        assert_eq!(entity.node.node_name, "talker");
        assert_eq!(topic.name, "/chatter");
        assert_eq!(topic.type_name, "std_msgs::msg::dds_::String_");
        assert_eq!(topic.type_hash, "RIHS01_abcd");
        assert_eq!(topic.qos, "1:2:3,10");
        assert!(topic.backends.is_empty());
    }

    #[test]
    fn parses_ros_liveliness_endpoint_backends() {
        let entity = parse_ros_liveliness_keyexpr(
            "@ros2_lv/0/zid/1/44/MS/%/%/listener/%chatter/std_msgs::msg::dds_::String_/RIHS01_abcd/1:2:3,10/zenoh/shm",
        )
        .unwrap();
        let topic = entity.topic.as_ref().unwrap();

        assert_eq!(entity.kind, ZenohRosEntityKind::Subscription);
        assert_eq!(topic.backends, vec!["zenoh".to_string(), "shm".to_string()]);
    }

    #[test]
    fn rejects_non_ros_liveliness_keyexpr() {
        let err = parse_ros_liveliness_keyexpr("0/chatter/std_msgs/RIHS01").unwrap_err();

        assert_eq!(
            err,
            ZenohRosLivelinessParseError::TooFewParts {
                actual: 4,
                minimum: 9
            }
        );
    }

    #[test]
    fn rejects_endpoint_without_topic_info() {
        let err = parse_ros_liveliness_keyexpr("@ros2_lv/0/zid/1/32/MP/%/%/talker").unwrap_err();

        assert_eq!(err, ZenohRosLivelinessParseError::MissingTopicInfo);
    }

    #[test]
    fn rejects_unknown_ros_liveliness_entity_kind() {
        let err = parse_ros_liveliness_keyexpr("@ros2_lv/0/zid/1/1/XX/%/%/talker").unwrap_err();

        assert_eq!(
            err,
            ZenohRosLivelinessParseError::InvalidEntityKind("XX".to_string())
        );
    }

    #[test]
    fn parses_ros_topic_sample_keyexpr() {
        let sample = parse_ros_topic_sample_keyexpr(
            "2/%robot%chatter/std_msgs::msg::dds_::String_/RIHS01_abcd",
            Bytes::from_static(b"hello"),
            128,
            Some(16),
        )
        .unwrap();

        assert_eq!(sample.domain_id, 2);
        assert_eq!(sample.topic_name, "/robot/chatter");
        assert_eq!(sample.type_name, "std_msgs::msg::dds_::String_");
        assert_eq!(sample.type_hash, "RIHS01_abcd");
        assert_eq!(sample.payload.as_ref(), b"hello");
        assert_eq!(sample.payload_len, 128);
        assert!(sample.identity.is_none());
        assert_eq!(sample.attachment_len, Some(16));
    }

    #[test]
    fn parses_rmw_zenoh_attachment_identity() {
        let source_gid = [7u8; TOPIC_GID_LEN];
        let attachment = rmw_zenoh_attachment(42, 1_234_567, source_gid);

        let identity = parse_rmw_zenoh_attachment_identity(&attachment).unwrap();

        assert_eq!(identity.sequence_number, 42);
        assert_eq!(identity.source_gid, TopicGid::new(source_gid));
    }

    #[test]
    fn computes_rmw_zenoh_entity_gid_from_liveliness_keyexpr() {
        let keyexpr = concat!(
            "@ros2_lv/0/zid/1/32/MP/%/%/talker/",
            "%chatter/std_msgs::msg::dds_::String_/RIHS01_abcd/2:1:1,7:5,7:60,3000:3,8,9"
        );
        let entity = parse_ros_liveliness_keyexpr(keyexpr).unwrap();

        assert_eq!(
            rmw_zenoh_entity_gid(&entity),
            TopicGid::new([
                0x6d, 0x4a, 0x45, 0x06, 0x84, 0x0b, 0xd5, 0x63, 0x9a, 0xfa, 0x54, 0x80, 0x7f, 0xd1,
                0x3b, 0x46,
            ])
        );
        assert_eq!(
            rmw_zenoh_keyexpr_gid(&"a".repeat(200)),
            TopicGid::new([
                0x08, 0x40, 0x03, 0x46, 0x5c, 0x4f, 0x65, 0xc8, 0xda, 0x79, 0xcc, 0x25, 0x8d, 0x96,
                0xfe, 0xcf,
            ])
        );
        assert_eq!(
            rmw_zenoh_keyexpr_gid(&"b".repeat(300)),
            TopicGid::new([
                0x72, 0x41, 0x08, 0x29, 0x57, 0x2c, 0xd1, 0xc2, 0x13, 0x6f, 0x9a, 0x36, 0x58, 0x51,
                0xf6, 0x2e,
            ])
        );
    }

    #[test]
    fn rmw_zenoh_entity_gid_excludes_backend_suffixes() {
        let base = parse_ros_liveliness_keyexpr(
            "@ros2_lv/0/zid/1/44/MS/%/%/listener/%chatter/std_msgs::msg::dds_::String_/RIHS01_abcd/1:2:3,10",
        )
        .unwrap();
        let with_backends = parse_ros_liveliness_keyexpr(
            "@ros2_lv/0/zid/1/44/MS/%/%/listener/%chatter/std_msgs::msg::dds_::String_/RIHS01_abcd/1:2:3,10/zenoh/shm",
        )
        .unwrap();

        assert_eq!(
            rmw_zenoh_entity_gid(&base),
            rmw_zenoh_entity_gid(&with_backends)
        );
    }

    #[test]
    fn push_put_emits_ros_topic_sample_event() {
        let mut processor = processor();
        let flow = flow(32100, 7447);
        let events = processor.semantic.process_messages(
            TransportProtocol::Tcp,
            flow,
            &[push_put(
                wire_expr(0, "0/%chatter/std_msgs::msg::dds_::String_/RIHS01_abcd"),
                42,
                None,
            )],
        );

        assert!(matches!(
            events.as_slice(),
            [ZenohSemanticEvent::TopicSample(sample)]
                if sample.domain_id == 0
                    && sample.topic_name == "/chatter"
                    && sample.type_name == "std_msgs::msg::dds_::String_"
                    && sample.type_hash == "RIHS01_abcd"
                    && sample.payload_len == 42
                    && sample.payload.len() == 42
                    && sample.identity.is_none()
                    && sample.attachment_len.is_none()
        ));
    }

    #[test]
    fn push_put_emits_ros_topic_sample_identity_from_attachment() {
        let mut processor = processor();
        let source_gid = [9u8; TOPIC_GID_LEN];
        let events = processor.semantic.process_messages(
            TransportProtocol::Tcp,
            flow(32100, 7447),
            &[push_put_with_attachment(
                wire_expr(0, "0/%chatter/std_msgs::msg::dds_::String_/RIHS01_abcd"),
                42,
                Some(Bytes::from(rmw_zenoh_attachment(77, 1_234_567, source_gid))),
            )],
        );

        assert!(matches!(
            events.as_slice(),
            [ZenohSemanticEvent::TopicSample(sample)]
                if sample.identity == Some(ZenohRosSampleIdentity {
                    source_gid: TopicGid::new(source_gid),
                    sequence_number: 77,
                })
        ));
    }

    #[test]
    fn push_put_with_unknown_scope_emits_unresolved_sample_identity() {
        let mut processor = processor();
        let source_gid = [11u8; TOPIC_GID_LEN];
        let events = processor.semantic.process_messages(
            TransportProtocol::Tcp,
            flow(32100, 7447),
            &[push_put_with_attachment(
                wire_expr(99, ""),
                42,
                Some(Bytes::from(rmw_zenoh_attachment(78, 1_234_567, source_gid))),
            )],
        );

        assert!(matches!(
            events.as_slice(),
            [ZenohSemanticEvent::UnresolvedTopicSample(sample)]
                if sample.wire_expr.scope == 99
                    && sample.payload_len == 42
                    && sample.payload.len() == 42
                    && sample.identity == ZenohRosSampleIdentity {
                        source_gid: TopicGid::new(source_gid),
                        sequence_number: 78,
                    }
        ));
    }

    #[test]
    fn shm_push_emits_descriptor_event_instead_of_topic_payload() {
        let mut processor = processor();
        let source_gid = [12u8; TOPIC_GID_LEN];
        let mut message = push_put_with_attachment(
            wire_expr(0, "0/%chatter/std_msgs::msg::dds_::String_/RIHS01_abcd"),
            24,
            Some(Bytes::from(rmw_zenoh_attachment(79, 1_234_567, source_gid))),
        );
        let ZenohDecodedMessage::Frame { messages, .. } = &mut message else {
            unreachable!();
        };
        let ZenohNetworkMessage::Push {
            body: ZenohPushBody::Put { is_shm, .. },
            ..
        } = &mut messages[0]
        else {
            unreachable!();
        };
        *is_shm = true;

        let events = processor.semantic.process_messages(
            TransportProtocol::Tcp,
            flow(32100, 7447),
            &[message],
        );

        assert!(matches!(
            events.as_slice(),
            [ZenohSemanticEvent::ShmTopicSample {
                topic_name: Some(topic_name),
                identity: Some(ZenohRosSampleIdentity {
                    source_gid: gid,
                    sequence_number: 79,
                }),
            }] if topic_name == "/chatter" && *gid == TopicGid::new(source_gid)
        ));
    }

    #[test]
    fn push_put_resolves_receiver_mapping_from_reverse_flow_declaration() {
        let mut processor = processor();
        let router_to_router = flow(32100, 7447);
        let reverse = flow(7447, 32100);
        processor.semantic.process_messages(
            TransportProtocol::Tcp,
            reverse,
            &[declare(ZenohDeclareBody::DeclareKeyExpr {
                id: 37,
                wire_expr: wire_expr(0, "0/chatter/"),
            })],
        );

        let events = processor.semantic.process_messages(
            TransportProtocol::Tcp,
            router_to_router,
            &[push_put(
                wire_expr_with_mapping(
                    37,
                    "std_msgs::msg::dds_::String_/RIHS01_abcd",
                    Mapping::Receiver,
                ),
                42,
                None,
            )],
        );

        assert!(matches!(
            events.as_slice(),
            [ZenohSemanticEvent::TopicSample(sample)]
                if sample.topic_name == "/chatter"
                    && sample.type_name == "std_msgs::msg::dds_::String_"
                    && sample.payload_len == 42
        ));
    }

    fn processor() -> ZenohProcessor {
        ZenohProcessor::new_with_decoder(4, decode_test_batch)
    }

    fn declare(body: ZenohDeclareBody) -> ZenohDecodedMessage {
        ZenohDecodedMessage::Frame {
            reliability: "Reliable".to_string(),
            sequence_number: None,
            messages: vec![ZenohNetworkMessage::Declare {
                reliability: "Reliable".to_string(),
                body,
            }],
        }
    }

    fn push_put(
        wire_expr: ZenohWireExpr,
        payload_len: usize,
        attachment_len: Option<usize>,
    ) -> ZenohDecodedMessage {
        let attachment = attachment_len.map(|len| Bytes::from(vec![0; len]));
        push_put_with_attachment(wire_expr, payload_len, attachment)
    }

    fn push_put_with_attachment(
        wire_expr: ZenohWireExpr,
        payload_len: usize,
        attachment: Option<Bytes>,
    ) -> ZenohDecodedMessage {
        let attachment_len = attachment.as_ref().map(Bytes::len);
        ZenohDecodedMessage::Frame {
            reliability: "Reliable".to_string(),
            sequence_number: None,
            messages: vec![ZenohNetworkMessage::Push {
                reliability: "Reliable".to_string(),
                wire_expr,
                body: ZenohPushBody::Put {
                    encoding: "AppOctetStream".to_string(),
                    payload: Bytes::from(vec![0; payload_len]),
                    payload_len,
                    attachment,
                    attachment_len,
                    is_shm: false,
                },
            }],
        }
    }

    fn rmw_zenoh_attachment(
        sequence_number: i64,
        source_timestamp: i64,
        source_gid: [u8; TOPIC_GID_LEN],
    ) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&sequence_number.to_le_bytes());
        bytes.extend_from_slice(&source_timestamp.to_le_bytes());
        bytes.push(TOPIC_GID_LEN as u8);
        bytes.extend_from_slice(&source_gid);
        bytes
    }

    fn wire_expr(scope: u16, suffix: &str) -> ZenohWireExpr {
        wire_expr_with_mapping(scope, suffix, Mapping::Sender)
    }

    fn wire_expr_with_mapping(scope: u16, suffix: &str, mapping: Mapping) -> ZenohWireExpr {
        ZenohWireExpr {
            scope,
            suffix: suffix.to_string(),
            mapping,
        }
    }

    fn decode_test_batch(payload: &[u8]) -> Option<Vec<ZenohDecodedMessage>> {
        if payload.is_empty() || payload == [0xff] {
            return None;
        }
        Some(vec![ZenohDecodedMessage::KeepAlive])
    }

    fn tcp_packet(
        src_port: u16,
        dst_port: u16,
        sequence: u32,
        payload: &'static [u8],
    ) -> CapturedTransportPacket {
        tcp_packet_with_flags(src_port, dst_port, sequence, 0, payload)
    }

    fn tcp_packet_with_flags(
        src_port: u16,
        dst_port: u16,
        sequence: u32,
        flags: u16,
        payload: &'static [u8],
    ) -> CapturedTransportPacket {
        let mut packet = packet(TransportProtocol::Tcp, src_port, dst_port, payload);
        packet.tcp = Some(TcpSegmentInfo { sequence, flags });
        packet
    }

    fn flow(src_port: u16, dst_port: u16) -> FlowTuple {
        FlowTuple::new(
            IpAddr::from_v4(u32::from_be_bytes([127, 0, 0, 1])),
            IpAddr::from_v4(u32::from_be_bytes([127, 0, 0, 1])),
            src_port,
            dst_port,
        )
    }

    fn packet(
        protocol: TransportProtocol,
        src_port: u16,
        dst_port: u16,
        payload: &'static [u8],
    ) -> CapturedTransportPacket {
        CapturedTransportPacket {
            socket_timestamp: SystemTime::UNIX_EPOCH,
            frame_len: payload.len(),
            direction: PacketDirection::Host,
            protocol,
            flow: flow(src_port, dst_port),
            tcp: (protocol == TransportProtocol::Tcp).then_some(TcpSegmentInfo {
                sequence: 0,
                flags: 0,
            }),
            ip_identification: 0,
            payload: Bytes::from_static(payload),
            ip_fragment_count: 1,
            was_ip_fragmented: false,
        }
    }
}
