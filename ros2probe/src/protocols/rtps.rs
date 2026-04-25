use std::collections::{HashMap, VecDeque};
use std::time::SystemTime;

use anyhow::Result;
use bytes::Bytes;
use ros2probe_common::{
    IpAddr, RTPS_GUID_PREFIX_LEN, RTPS_GUID_PREFIX_OFFSET, RTPS_HEADER_LEN, RTPS_SIGNATURE,
    RTPS_WRITER_ENTITY_ID_SEDP_PUBLICATIONS, RTPS_WRITER_ENTITY_ID_SEDP_SUBSCRIPTIONS,
    RTPS_WRITER_ENTITY_ID_SPDP_PARTICIPANT, TopicGid,
};

use crate::capture::{CapturedUdpPacket, PacketDirection};

const PID_SENTINEL: u16 = 0x0001;
const PID_STATUS_INFO: u16 = 0x0071;
const RTPS_DATA_FLAG_INLINE_QOS: u8 = 0x02;
const RTPS_DATA_FLAG_DATA: u8 = 0x04;
const STATUS_INFO_DISPOSED_OR_UNREGISTERED: u32 = 0x0000_0003;
const RTPS_DATA_SUBMESSAGE_ID: u8 = 0x15;
const RTPS_DATA_FRAG_SUBMESSAGE_ID: u8 = 0x16;
const RTPS_SUBMESSAGE_HEADER_LEN: usize = 4;
const RTPS_SUBMESSAGE_PREFIX_LEN: usize = RTPS_SUBMESSAGE_HEADER_LEN + 4;
const RTPS_SUBMESSAGE_READER_ID_OFFSET: usize = 8;
const RTPS_SUBMESSAGE_WRITER_ID_OFFSET: usize = 12;
const RTPS_SUBMESSAGE_SEQUENCE_NUMBER_OFFSET: usize = 16;
const RTPS_DATA_FRAG_STARTING_NUM_OFFSET: usize = 24;
const RTPS_DATA_FRAG_FRAGS_IN_SUBMESSAGE_OFFSET: usize = 28;
const RTPS_DATA_FRAG_FRAGMENT_SIZE_OFFSET: usize = 30;
const RTPS_DATA_FRAG_SAMPLE_SIZE_OFFSET: usize = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RtpsMessageKind {
    UserData,
    Discovery(DiscoveryKind),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiscoveryKind {
    Participant,
    Publication,
    Subscription,
    UnknownBuiltin,
}

#[derive(Clone, Debug)]
pub struct RtpsMessage {
    pub captured_at: SystemTime,
    pub socket_timestamp: SystemTime,
    pub frame_len: usize,
    pub direction: PacketDirection,
    pub src_ip: IpAddr,
    pub dst_ip: IpAddr,
    pub udp_payload_len: usize,
    pub ip_fragment_count: u32,
    pub reader_gid: TopicGid,
    pub writer_gid: TopicGid,
    pub sequence_number: [u8; 8],
    pub writer_entity_id: [u8; 4],
    /// Payload bytes. `Bytes` shares the original packet allocation via
    /// refcount so forwarding to the recorder / observers costs just an
    /// atomic refcount bump, and sub-slicing (e.g. DATA submessage payload
    /// from the UDP payload buffer) is zero-copy.
    pub payload: Bytes,
    pub kind: RtpsMessageKind,
    pub disposed: bool,
}

#[derive(Clone, Debug)]
pub struct RtpsDataMessage {
    pub captured_at: SystemTime,
    pub socket_timestamp: SystemTime,
    pub frame_len: usize,
    pub direction: PacketDirection,
    pub src_ip: IpAddr,
    pub dst_ip: IpAddr,
    pub udp_payload_len: usize,
    pub ip_fragment_count: u32,
    pub reader_gid: TopicGid,
    pub writer_gid: TopicGid,
    pub sequence_number: [u8; 8],
    /// See `RtpsMessage::payload`.
    pub payload: Bytes,
}

#[derive(Clone, Debug)]
pub enum RtpsEvent {
    Discovery(RtpsMessage),
    Message(RtpsDataMessage),
}

pub struct RtpsProcessor {
    flows: HashMap<RtpsSampleKey, RtpsFragState>,
    order: VecDeque<RtpsSampleKey>,
    capacity: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
struct RtpsSampleKey {
    writer_gid: TopicGid,
    sequence_number: [u8; 8],
}

struct RtpsFragState {
    captured_at: SystemTime,
    socket_timestamp: SystemTime,
    ip_fragment_count: u32,
    sample_size: usize,
    ranges: Vec<(usize, usize)>,
    /// Pre-allocated reassembly buffer of length `sample_size`. Each fragment
    /// is written directly into its target offset, so arrivals cost one memcpy
    /// instead of one alloc + one final pass.
    reassembled: Vec<u8>,
}

impl RtpsProcessor {
    pub fn new(capacity: usize) -> Self {
        Self {
            flows: HashMap::with_capacity(capacity),
            order: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    pub fn process_packet(&mut self, packet: CapturedUdpPacket) -> Result<Vec<RtpsEvent>> {
        let udp_payload: &[u8] = &packet.payload;
        if udp_payload.len() < RTPS_HEADER_LEN || !udp_payload.starts_with(&RTPS_SIGNATURE) {
            return Ok(Vec::new());
        }

        let mut guid_prefix = [0u8; RTPS_GUID_PREFIX_LEN];
        guid_prefix.copy_from_slice(
            udp_payload
                .get(RTPS_GUID_PREFIX_OFFSET..RTPS_GUID_PREFIX_OFFSET + RTPS_GUID_PREFIX_LEN)
                .ok_or_else(|| anyhow::anyhow!("RTPS guid prefix out of bounds"))?,
        );

        let mut events = Vec::new();
        let mut submessage_offset = RTPS_HEADER_LEN;
        while submessage_offset + RTPS_SUBMESSAGE_HEADER_LEN <= udp_payload.len() {
            let submessage_id = udp_payload[submessage_offset];
            let flags = udp_payload[submessage_offset + 1];
            let octets_to_next_header = read_u16(
                &udp_payload[submessage_offset + 2..submessage_offset + 4],
                flags,
            ) as usize;
            let submessage_end = if octets_to_next_header == 0 {
                udp_payload.len()
            } else {
                (submessage_offset + RTPS_SUBMESSAGE_HEADER_LEN + octets_to_next_header)
                    .min(udp_payload.len())
            };
            if submessage_end <= submessage_offset {
                break;
            }

            match submessage_id {
                RTPS_DATA_SUBMESSAGE_ID => {
                    if let Some(message) = parse_data_submessage(
                        &udp_payload[submessage_offset..submessage_end],
                        submessage_offset,
                        submessage_end,
                        guid_prefix,
                        &packet,
                    )? {
                        events.push(message_to_event(message));
                    }
                }
                RTPS_DATA_FRAG_SUBMESSAGE_ID => {
                    if let Some(message) = self.accept_data_frag(
                        &udp_payload[submessage_offset..submessage_end],
                        guid_prefix,
                        &packet,
                    )? {
                        events.push(message_to_event(message));
                    }
                }
                _ => {}
            }

            submessage_offset = submessage_end;
        }

        Ok(events)
    }

    fn accept_data_frag(
        &mut self,
        submessage: &[u8],
        guid_prefix: [u8; RTPS_GUID_PREFIX_LEN],
        packet: &CapturedUdpPacket,
    ) -> Result<Option<RtpsMessage>> {
        let flags = *submessage
            .get(1)
            .ok_or_else(|| anyhow::anyhow!("DATA_FRAG flags out of bounds"))?;

        let writer_entity_id = read_array::<4>(submessage, RTPS_SUBMESSAGE_WRITER_ID_OFFSET)?;
        let reader_entity_id = read_array::<4>(submessage, RTPS_SUBMESSAGE_READER_ID_OFFSET)?;
        let sequence_number =
            read_array::<8>(submessage, RTPS_SUBMESSAGE_SEQUENCE_NUMBER_OFFSET)?;
        let writer_gid = TopicGid::from_rtps_parts(guid_prefix, writer_entity_id);
        let key = RtpsSampleKey {
            writer_gid,
            sequence_number,
        };

        let fragment_starting_num = usize::try_from(read_u32(
            submessage,
            RTPS_DATA_FRAG_STARTING_NUM_OFFSET,
            flags,
        ))?;
        let fragments_in_submessage = usize::from(read_u16_at(
            submessage,
            RTPS_DATA_FRAG_FRAGS_IN_SUBMESSAGE_OFFSET,
            flags,
        )?);
        let fragment_size = usize::from(read_u16_at(
            submessage,
            RTPS_DATA_FRAG_FRAGMENT_SIZE_OFFSET,
            flags,
        )?);
        let sample_size = usize::try_from(read_u32(
            submessage,
            RTPS_DATA_FRAG_SAMPLE_SIZE_OFFSET,
            flags,
        ))?;
        let payload_start = data_payload_start(submessage, flags)?;
        if payload_start >= submessage.len() {
            return Ok(None);
        }

        let payload = &submessage[payload_start..];
        let offset = fragment_starting_num
            .checked_sub(1)
            .and_then(|n| n.checked_mul(fragment_size))
            .ok_or_else(|| anyhow::anyhow!("DATA_FRAG fragment offset overflow"))?;

        // RTPS does not permit sampleSize to change mid-sequence. If an
        // existing flow reports a different sample_size, discard it rather
        // than silently resizing (which would corrupt already-written ranges).
        if let Some(existing) = self.flows.get(&key) {
            if existing.sample_size != sample_size {
                self.flows.remove(&key);
                return Ok(None);
            }
        } else {
            if self.flows.len() == self.capacity {
                while let Some(oldest) = self.order.pop_front() {
                    if self.flows.remove(&oldest).is_some() {
                        break;
                    }
                }
            }
            self.order.push_back(key);
        }

        let state = self.flows.entry(key).or_insert_with(|| RtpsFragState {
            captured_at: packet.socket_timestamp,
            socket_timestamp: packet.socket_timestamp,
            ip_fragment_count: packet.ip_fragment_count,
            sample_size,
            ranges: Vec::new(),
            reassembled: vec![0u8; sample_size],
        });
        if packet.socket_timestamp < state.captured_at {
            state.captured_at = packet.socket_timestamp;
        }
        if packet.socket_timestamp < state.socket_timestamp {
            state.socket_timestamp = packet.socket_timestamp;
        }
        state.ip_fragment_count = state.ip_fragment_count.max(packet.ip_fragment_count);

        let expected_max_len = fragments_in_submessage
            .checked_mul(fragment_size)
            .ok_or_else(|| anyhow::anyhow!("DATA_FRAG expected length overflow"))?;
        let payload_len = payload.len().min(expected_max_len);
        let end = (offset + payload_len).min(state.sample_size);
        if end <= offset {
            return Ok(None);
        }

        insert_range(&mut state.ranges, offset, end);
        state.reassembled[offset..end].copy_from_slice(&payload[..end - offset]);

        if !frag_flow_is_complete(state) {
            return Ok(None);
        }

        let Some(flow) = self.flows.remove(&key) else {
            return Ok(None);
        };

        Ok(Some(RtpsMessage {
            captured_at: flow.captured_at,
            socket_timestamp: flow.socket_timestamp,
            frame_len: packet.frame_len,
            direction: packet.direction,
            src_ip: packet.flow.src_ip,
            dst_ip: packet.flow.dst_ip,
            udp_payload_len: packet.payload.len(),
            ip_fragment_count: flow.ip_fragment_count,
            reader_gid: TopicGid::from_rtps_parts(guid_prefix, reader_entity_id),
            writer_gid: key.writer_gid,
            sequence_number: key.sequence_number,
            writer_entity_id,
            // `Bytes::from(Vec<u8>)` takes ownership of the Vec's buffer
            // without copying — the reassembly allocation becomes the
            // backing store of the Bytes, shared by refcount onwards.
            payload: Bytes::from(flow.reassembled),
            kind: classify_writer(writer_entity_id),
            disposed: false,
        }))
    }
}

fn parse_data_submessage(
    submessage: &[u8],
    submessage_offset: usize,
    submessage_end: usize,
    guid_prefix: [u8; RTPS_GUID_PREFIX_LEN],
    packet: &CapturedUdpPacket,
) -> Result<Option<RtpsMessage>> {
    let flags = *submessage
        .get(1)
        .ok_or_else(|| anyhow::anyhow!("DATA flags out of bounds"))?;
    let reader_entity_id = read_array::<4>(submessage, RTPS_SUBMESSAGE_READER_ID_OFFSET)?;
    let writer_entity_id = read_array::<4>(submessage, RTPS_SUBMESSAGE_WRITER_ID_OFFSET)?;
    let sequence_number = read_array::<8>(submessage, RTPS_SUBMESSAGE_SEQUENCE_NUMBER_OFFSET)?;

    // Check inline QoS for PID_STATUS_INFO to detect disposal.
    let disposed = if (flags & RTPS_DATA_FLAG_INLINE_QOS) != 0 {
        let inline_qos_offset = RTPS_SUBMESSAGE_PREFIX_LEN
            + usize::from(read_u16_at(submessage, 6, flags)?);
        inline_qos_has_disposal(submessage, inline_qos_offset, flags)
    } else {
        false
    };

    let has_data = (flags & RTPS_DATA_FLAG_DATA) != 0;
    let payload_start = data_payload_start(submessage, flags)?;

    // Normal message with no payload — skip unless it's a disposal notification.
    if payload_start >= submessage.len() && !disposed {
        return Ok(None);
    }

    // Zero-copy: slice the packet's Bytes at the submessage payload range.
    // Bounds: `submessage_offset + payload_start` is the absolute start of
    // this DATA's payload within `packet.payload`; `submessage_end` is the
    // absolute end (already clamped to `packet.payload.len()` at the call
    // site). Sharing the underlying allocation avoids the previous
    // `Arc::from(&submessage[..])` alloc + memcpy.
    let payload: Bytes = if payload_start < submessage.len() {
        packet
            .payload
            .slice(submessage_offset + payload_start..submessage_end)
    } else {
        Bytes::new()
    };

    // Skip normal DATA with no data payload (and no disposal).
    if !has_data && !disposed && payload.is_empty() {
        return Ok(None);
    }

    Ok(Some(RtpsMessage {
        captured_at: packet.socket_timestamp,
        socket_timestamp: packet.socket_timestamp,
        frame_len: packet.frame_len,
        direction: packet.direction,
        src_ip: packet.flow.src_ip,
        dst_ip: packet.flow.dst_ip,
        udp_payload_len: packet.payload.len(),
        ip_fragment_count: packet.ip_fragment_count,
        reader_gid: TopicGid::from_rtps_parts(guid_prefix, reader_entity_id),
        writer_gid: TopicGid::from_rtps_parts(guid_prefix, writer_entity_id),
        sequence_number,
        writer_entity_id,
        payload,
        kind: classify_writer(writer_entity_id),
        disposed,
    }))
}

fn message_to_event(message: RtpsMessage) -> RtpsEvent {
    match message.kind {
        RtpsMessageKind::Discovery(_) => RtpsEvent::Discovery(message),
        RtpsMessageKind::UserData => RtpsEvent::Message(RtpsDataMessage {
            captured_at: message.captured_at,
            socket_timestamp: message.socket_timestamp,
            frame_len: message.frame_len,
            direction: message.direction,
            src_ip: message.src_ip,
            dst_ip: message.dst_ip,
            udp_payload_len: message.udp_payload_len,
            ip_fragment_count: message.ip_fragment_count,
            reader_gid: message.reader_gid,
            writer_gid: message.writer_gid,
            sequence_number: message.sequence_number,
            payload: message.payload,
        }),
    }
}

fn inline_qos_has_disposal(submessage: &[u8], mut offset: usize, flags: u8) -> bool {
    while offset + 4 <= submessage.len() {
        let pid = read_u16(&submessage[offset..offset + 2], flags);
        let length = usize::from(read_u16(&submessage[offset + 2..offset + 4], flags));
        offset += 4;
        if pid == PID_SENTINEL {
            break;
        }
        if pid == PID_STATUS_INFO && length >= 4 && offset + 4 <= submessage.len() {
            let status_info = read_u32(submessage, offset, flags);
            return (status_info & STATUS_INFO_DISPOSED_OR_UNREGISTERED) != 0;
        }
        offset = match offset.checked_add(length) {
            Some(next) => next,
            None => break,
        };
    }
    false
}

fn data_payload_start(submessage: &[u8], flags: u8) -> Result<usize> {
    let octets_to_inline_qos = usize::from(read_u16_at(submessage, 6, flags)?);
    let inline_qos_offset = RTPS_SUBMESSAGE_PREFIX_LEN
        .checked_add(octets_to_inline_qos)
        .ok_or_else(|| anyhow::anyhow!("octetsToInlineQos overflow"))?;

    if (flags & RTPS_DATA_FLAG_INLINE_QOS) != 0 {
        skip_parameter_list(submessage, inline_qos_offset, flags)
    } else {
        Ok(inline_qos_offset)
    }
}

fn skip_parameter_list(payload: &[u8], mut offset: usize, flags: u8) -> Result<usize> {
    loop {
        let header = payload
            .get(offset..offset + 4)
            .ok_or_else(|| anyhow::anyhow!("inline QoS parameter header out of bounds"))?;
        let pid = read_u16(&header[..2], flags);
        let length = usize::from(read_u16(&header[2..4], flags));
        offset += 4;
        if pid == PID_SENTINEL {
            return Ok(offset);
        }
        offset = offset
            .checked_add(length)
            .ok_or_else(|| anyhow::anyhow!("inline QoS parameter length overflow"))?;
    }
}

fn insert_range(ranges: &mut Vec<(usize, usize)>, start: usize, end: usize) {
    if start >= end {
        return;
    }

    let insert_at = ranges.partition_point(|r| r.0 <= start);
    ranges.insert(insert_at, (start, end));

    let mut write = insert_at;
    if write > 0 && ranges[write - 1].1 >= ranges[write].0 {
        ranges[write - 1].1 = ranges[write - 1].1.max(ranges[write].1);
        ranges.remove(write);
        write -= 1;
    }
    while write + 1 < ranges.len() && ranges[write].1 >= ranges[write + 1].0 {
        ranges[write].1 = ranges[write].1.max(ranges[write + 1].1);
        ranges.remove(write + 1);
    }
}

fn frag_flow_is_complete(state: &RtpsFragState) -> bool {
    let Some((start, end)) = state.ranges.first().copied() else {
        return false;
    };

    start == 0 && end >= state.sample_size && state.ranges.len() == 1
}

fn read_array<const N: usize>(bytes: &[u8], offset: usize) -> Result<[u8; N]> {
    bytes[offset..offset + N]
        .try_into()
        .map_err(|_| anyhow::anyhow!("array slice out of bounds"))
}

fn read_u16_at(bytes: &[u8], offset: usize, flags: u8) -> Result<u16> {
    Ok(read_u16(
        bytes.get(offset..offset + 2)
            .ok_or_else(|| anyhow::anyhow!("u16 read out of bounds"))?,
        flags,
    ))
}

fn read_u32(bytes: &[u8], offset: usize, flags: u8) -> u32 {
    let bytes = [
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ];
    if (flags & 0x01) != 0 {
        u32::from_le_bytes(bytes)
    } else {
        u32::from_be_bytes(bytes)
    }
}

fn read_u16(bytes: &[u8], flags: u8) -> u16 {
    if (flags & 0x01) != 0 {
        u16::from_le_bytes([bytes[0], bytes[1]])
    } else {
        u16::from_be_bytes([bytes[0], bytes[1]])
    }
}

fn classify_writer(writer_entity_id: [u8; 4]) -> RtpsMessageKind {
    match writer_entity_id {
        RTPS_WRITER_ENTITY_ID_SPDP_PARTICIPANT => RtpsMessageKind::Discovery(DiscoveryKind::Participant),
        RTPS_WRITER_ENTITY_ID_SEDP_PUBLICATIONS => RtpsMessageKind::Discovery(DiscoveryKind::Publication),
        RTPS_WRITER_ENTITY_ID_SEDP_SUBSCRIPTIONS => RtpsMessageKind::Discovery(DiscoveryKind::Subscription),
        [_, _, _, kind] if (kind & 0xc0) == 0xc0 => {
            RtpsMessageKind::Discovery(DiscoveryKind::UnknownBuiltin)
        }
        _ => RtpsMessageKind::UserData,
    }
}
