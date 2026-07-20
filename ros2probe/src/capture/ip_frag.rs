use std::{
    collections::{HashMap, VecDeque},
    time::SystemTime,
};

use anyhow::{Result, bail};
use bytes::Bytes;
use ros2probe_common::{FlowTuple, IpAddr};

use crate::capture::PacketDirection;

const ETH_HDR_LEN: usize = 14;
const VLAN_HDR_LEN: usize = 4;
const ETH_P_IPV4: u16 = 0x0800;
const ETH_P_8021Q: u16 = 0x8100;
const ETH_P_8021AD: u16 = 0x88a8;
const ETH_P_8021QINQ: u16 = 0x9100;
const ETH_P_IPV6: u16 = 0x86dd;
const IPV4_FLAG_MORE_FRAGMENTS: u16 = 0x2000;
const IPV4_FRAGMENT_OFFSET_MASK: u16 = 0x1fff;
const IPV4_MIN_HDR_LEN: usize = 20;
const IPV6_HDR_LEN: usize = 40;
const TCP_PROTOCOL: u8 = 6;
const UDP_PROTOCOL: u8 = 17;
const TCP_MIN_HDR_LEN: usize = 20;
const UDP_HDR_LEN: usize = 8;
const IPV6_NEXT_HEADER_HOP_BY_HOP: u8 = 0;
const IPV6_NEXT_HEADER_ROUTING: u8 = 43;
const IPV6_NEXT_HEADER_FRAGMENT: u8 = 44;
const IPV6_NEXT_HEADER_ESP: u8 = 50;
const IPV6_NEXT_HEADER_AUTH: u8 = 51;
const IPV6_NEXT_HEADER_DESTINATION: u8 = 60;
const IPV6_NEXT_HEADER_NO_NEXT: u8 = 59;
const IPV6_FRAGMENT_OFFSET_MASK: u16 = 0xfff8;

#[derive(Clone)]
pub struct CapturedIpPacket {
    pub socket_timestamp: SystemTime,
    pub frame_len: usize,
    pub direction: PacketDirection,
    pub protocol: TransportProtocol,
    pub flow: FlowTuple,
    pub ip_identification: u16,
    /// Everything after the IP header, including the L4 header. Downstream
    /// conversion strips UDP/TCP headers with `Bytes::slice`, so packet
    /// ownership is lifted out of the netring buffer once and then shared
    /// zero-copy through protocol parsers.
    pub ip_payload: Bytes,
    pub fragment: Option<IpFragmentInfo>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum TransportProtocol {
    Tcp,
    Udp,
}

impl TransportProtocol {
    pub const fn number(self) -> u8 {
        match self {
            Self::Tcp => TCP_PROTOCOL,
            Self::Udp => UDP_PROTOCOL,
        }
    }

    fn from_number(protocol: u8) -> Result<Self> {
        match protocol {
            TCP_PROTOCOL => Ok(Self::Tcp),
            UDP_PROTOCOL => Ok(Self::Udp),
            _ => bail!("unsupported transport protocol {protocol}"),
        }
    }
}

#[derive(Clone, Copy)]
pub struct IpFragmentInfo {
    key: IpFragmentKey,
    offset_bytes: usize,
    more_fragments: bool,
}

impl IpFragmentInfo {
    pub fn key(&self) -> IpFragmentKey {
        self.key
    }

    pub fn offset_bytes(&self) -> usize {
        self.offset_bytes
    }

    pub fn more_fragments(&self) -> bool {
        self.more_fragments
    }
}

pub struct ReassembledIpDatagram {
    pub socket_timestamp: SystemTime,
    pub frame_len: usize,
    pub direction: PacketDirection,
    pub protocol: TransportProtocol,
    pub flow: FlowTuple,
    pub ip_identification: u16,
    pub ip_payload: Bytes,
    pub fragment_count: u32,
    pub was_fragmented: bool,
}

pub struct ReassembledUdpPayload {
    pub socket_timestamp: SystemTime,
    pub frame_len: usize,
    pub direction: PacketDirection,
    pub flow: FlowTuple,
    pub ip_identification: u16,
    pub udp_payload: Bytes,
    pub fragment_count: u32,
    pub was_fragmented: bool,
}

pub struct ReassembledTransportPayload {
    pub socket_timestamp: SystemTime,
    pub frame_len: usize,
    pub direction: PacketDirection,
    pub protocol: TransportProtocol,
    pub flow: FlowTuple,
    pub tcp: Option<TcpSegmentInfo>,
    pub ip_identification: u16,
    pub payload: Bytes,
    pub fragment_count: u32,
    pub was_fragmented: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TcpSegmentInfo {
    pub sequence: u32,
    pub flags: u16,
}

pub struct IpFragmentReassembler {
    flows: HashMap<IpFragmentKey, FragmentFlowState>,
    order: VecDeque<IpFragmentKey>,
    capacity: usize,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct IpFragmentKey {
    pub src_ip: IpAddr,
    pub dst_ip: IpAddr,
    pub identification: u32,
    pub protocol: TransportProtocol,
}

struct FragmentFlowState {
    socket_timestamp: SystemTime,
    frame_len: usize,
    direction: PacketDirection,
    flow: FlowTuple,
    ip_identification: u16,
    total_len: Option<usize>,
    ranges: Vec<(usize, usize)>,
    chunks: Vec<FragmentChunk>,
}

struct FragmentChunk {
    offset: usize,
    bytes: Bytes,
}

impl IpFragmentReassembler {
    pub fn new(capacity: usize) -> Self {
        Self {
            flows: HashMap::with_capacity(capacity),
            order: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    pub fn accept(&mut self, packet: CapturedIpPacket) -> Vec<ReassembledIpDatagram> {
        let Some(fragment) = packet.fragment else {
            return vec![ReassembledIpDatagram {
                socket_timestamp: packet.socket_timestamp,
                frame_len: packet.frame_len,
                direction: packet.direction,
                protocol: packet.protocol,
                flow: packet.flow,
                ip_identification: packet.ip_identification,
                ip_payload: packet.ip_payload,
                fragment_count: 1,
                was_fragmented: false,
            }];
        };

        let key = fragment.key();
        if self.capacity == 0 {
            return Vec::new();
        }
        if !self.flows.contains_key(&key) {
            if self.flows.len() >= self.capacity {
                while let Some(oldest) = self.order.pop_front() {
                    if self.flows.remove(&oldest).is_some() {
                        break;
                    }
                }
            }
            self.order.push_back(key);
        }

        let state = self.flows.entry(key).or_insert_with(|| FragmentFlowState {
            socket_timestamp: packet.socket_timestamp,
            frame_len: packet.frame_len,
            direction: packet.direction,
            flow: packet.flow,
            ip_identification: packet.ip_identification,
            total_len: None,
            ranges: Vec::new(),
            chunks: Vec::new(),
        });

        if packet.socket_timestamp < state.socket_timestamp {
            state.socket_timestamp = packet.socket_timestamp;
        }
        state.frame_len = state.frame_len.max(packet.frame_len);

        let fragment_end = fragment.offset_bytes() + packet.ip_payload.len();
        if !fragment.more_fragments() {
            state.total_len = Some(fragment_end);
        }

        insert_range(&mut state.ranges, fragment.offset_bytes(), fragment_end);
        if let Some(existing) = state
            .chunks
            .iter_mut()
            .find(|chunk| chunk.offset == fragment.offset_bytes())
        {
            existing.bytes = packet.ip_payload;
        } else {
            state.chunks.push(FragmentChunk {
                offset: fragment.offset_bytes(),
                bytes: packet.ip_payload,
            });
        }

        if !flow_is_complete(state) {
            return Vec::new();
        }

        let Some(mut flow) = self.flows.remove(&key) else {
            return Vec::new();
        };
        if let Some(position) = self.order.iter().position(|queued| *queued == key) {
            self.order.remove(position);
        }
        flow.chunks.sort_by_key(|chunk| chunk.offset);
        let fragment_count = u32::try_from(flow.chunks.len()).unwrap_or(u32::MAX);

        let total_len = flow.total_len.unwrap_or(0);
        let mut ip_payload = vec![0u8; total_len];
        for chunk in flow.chunks {
            let end = (chunk.offset + chunk.bytes.len()).min(ip_payload.len());
            if end > chunk.offset {
                ip_payload[chunk.offset..end].copy_from_slice(&chunk.bytes[..end - chunk.offset]);
            }
        }

        vec![ReassembledIpDatagram {
            socket_timestamp: flow.socket_timestamp,
            frame_len: flow.frame_len,
            direction: flow.direction,
            protocol: key.protocol,
            flow: flow.flow,
            ip_identification: flow.ip_identification,
            // `Bytes::from(Vec<u8>)` takes ownership of the Vec's buffer
            // without copying — the reassembled allocation becomes the
            // backing store of the Bytes.
            ip_payload: Bytes::from(ip_payload),
            fragment_count,
            was_fragmented: true,
        }]
    }
}

impl ReassembledIpDatagram {
    pub fn into_transport_payload(self) -> Result<ReassembledTransportPayload> {
        match self.protocol {
            TransportProtocol::Udp => self.into_udp_transport_payload(),
            TransportProtocol::Tcp => self.into_tcp_transport_payload(),
        }
    }

    pub fn into_udp_payload(self) -> Result<ReassembledUdpPayload> {
        let transport = self.into_transport_payload()?;
        if transport.protocol != TransportProtocol::Udp {
            bail!("reassembled IP datagram is not UDP");
        }

        Ok(ReassembledUdpPayload {
            socket_timestamp: transport.socket_timestamp,
            frame_len: transport.frame_len,
            direction: transport.direction,
            flow: transport.flow,
            ip_identification: transport.ip_identification,
            udp_payload: transport.payload,
            fragment_count: transport.fragment_count,
            was_fragmented: transport.was_fragmented,
        })
    }

    fn into_udp_transport_payload(self) -> Result<ReassembledTransportPayload> {
        if self.ip_payload.len() < UDP_HDR_LEN {
            bail!("reassembled IP payload shorter than UDP header");
        }

        let src_port = u16::from_be_bytes([self.ip_payload[0], self.ip_payload[1]]);
        let dst_port = u16::from_be_bytes([self.ip_payload[2], self.ip_payload[3]]);
        let udp_len = usize::from(u16::from_be_bytes([self.ip_payload[4], self.ip_payload[5]]));
        if udp_len < UDP_HDR_LEN {
            bail!("UDP length shorter than header");
        }
        let available = self.ip_payload.len().min(udp_len);
        if available < UDP_HDR_LEN {
            bail!("reassembled UDP datagram shorter than header");
        }

        let payload = self.ip_payload.slice(UDP_HDR_LEN..available);
        Ok(self.with_transport_payload(src_port, dst_port, None, payload))
    }

    fn into_tcp_transport_payload(self) -> Result<ReassembledTransportPayload> {
        if self.ip_payload.len() < TCP_MIN_HDR_LEN {
            bail!("reassembled IP payload shorter than TCP header");
        }

        let src_port = u16::from_be_bytes([self.ip_payload[0], self.ip_payload[1]]);
        let dst_port = u16::from_be_bytes([self.ip_payload[2], self.ip_payload[3]]);
        let sequence = u32::from_be_bytes([
            self.ip_payload[4],
            self.ip_payload[5],
            self.ip_payload[6],
            self.ip_payload[7],
        ]);
        let data_offset_words = self.ip_payload[12] >> 4;
        if data_offset_words < 5 {
            bail!("TCP data offset shorter than minimum header");
        }
        let flags = (u16::from(self.ip_payload[12] & 0x01) << 8) | u16::from(self.ip_payload[13]);
        let header_len = usize::from(data_offset_words) * 4;
        if header_len < TCP_MIN_HDR_LEN || self.ip_payload.len() < header_len {
            bail!("TCP header length exceeds reassembled payload");
        }

        let payload = self.ip_payload.slice(header_len..);
        Ok(self.with_transport_payload(
            src_port,
            dst_port,
            Some(TcpSegmentInfo { sequence, flags }),
            payload,
        ))
    }

    fn with_transport_payload(
        self,
        src_port: u16,
        dst_port: u16,
        tcp: Option<TcpSegmentInfo>,
        payload: Bytes,
    ) -> ReassembledTransportPayload {
        ReassembledTransportPayload {
            socket_timestamp: self.socket_timestamp,
            frame_len: self.frame_len,
            direction: self.direction,
            protocol: self.protocol,
            flow: FlowTuple::new(self.flow.src_ip, self.flow.dst_ip, src_port, dst_port),
            tcp,
            ip_identification: self.ip_identification,
            payload,
            fragment_count: self.fragment_count,
            was_fragmented: self.was_fragmented,
        }
    }
}

pub fn parse_ipv4_packet(
    frame: &[u8],
    frame_len: usize,
    direction: PacketDirection,
    socket_timestamp: SystemTime,
) -> Result<CapturedIpPacket> {
    let l3_offset = parse_ipv4_l3_offset(frame)?;
    if frame.len() < l3_offset + IPV4_MIN_HDR_LEN {
        bail!("packet shorter than Ethernet/VLAN + minimum IPv4 header");
    }

    let packet = &frame[l3_offset..];
    let version_ihl = packet[0];
    let version = version_ihl >> 4;
    if version != 4 {
        bail!("unexpected IPv4 version nibble {version}");
    }

    let ihl_words = usize::from(version_ihl & 0x0f);
    if ihl_words < 5 {
        bail!(
            "unexpected IPv4 header length nibble {}",
            version_ihl & 0x0f
        );
    }
    let ip_header_len = ihl_words * 4;
    if packet.len() < ip_header_len {
        bail!("packet shorter than declared IPv4 header length");
    }

    let protocol = TransportProtocol::from_number(packet[9])?;

    let total_len = usize::from(u16::from_be_bytes([packet[2], packet[3]]));
    let payload_end = total_len.min(packet.len());
    if payload_end < ip_header_len {
        bail!("IPv4 total length shorter than header");
    }

    let src_ip = u32::from_be_bytes([packet[12], packet[13], packet[14], packet[15]]);
    let dst_ip = u32::from_be_bytes([packet[16], packet[17], packet[18], packet[19]]);
    let identification = u16::from_be_bytes([packet[4], packet[5]]);
    let fragment_bits = u16::from_be_bytes([packet[6], packet[7]]);
    let more_fragments = (fragment_bits & IPV4_FLAG_MORE_FRAGMENTS) != 0;
    let fragment_offset_bytes = usize::from(fragment_bits & IPV4_FRAGMENT_OFFSET_MASK) * 8;
    // One-time copy out of the netring frame (whose lifetime ends with the
    // batch) into a heap-owned `Bytes`. All subsequent sub-slices share this
    // allocation.
    let ip_payload = Bytes::copy_from_slice(&packet[ip_header_len..payload_end]);

    let flow = FlowTuple::new(IpAddr::from_v4(src_ip), IpAddr::from_v4(dst_ip), 0, 0);
    let fragment = (more_fragments || fragment_offset_bytes != 0).then_some(IpFragmentInfo {
        key: IpFragmentKey {
            src_ip: IpAddr::from_v4(src_ip),
            dst_ip: IpAddr::from_v4(dst_ip),
            identification: identification.into(),
            protocol,
        },
        offset_bytes: fragment_offset_bytes,
        more_fragments,
    });

    Ok(CapturedIpPacket {
        socket_timestamp,
        frame_len,
        direction,
        protocol,
        flow,
        ip_identification: identification,
        ip_payload,
        fragment,
    })
}

pub fn parse_ipv6_packet(
    frame: &[u8],
    frame_len: usize,
    direction: PacketDirection,
    socket_timestamp: SystemTime,
) -> Result<CapturedIpPacket> {
    let l3_offset = parse_ipv6_l3_offset(frame)?;
    if frame.len() < l3_offset + IPV6_HDR_LEN {
        bail!("packet shorter than Ethernet/VLAN + IPv6 header");
    }

    let packet = &frame[l3_offset..];
    let version = packet[0] >> 4;
    if version != 6 {
        bail!("unexpected IPv6 version nibble {version}");
    }

    let payload_len = usize::from(u16::from_be_bytes([packet[4], packet[5]]));
    let packet_end = (l3_offset + IPV6_HDR_LEN + payload_len).min(frame.len());
    if packet_end < l3_offset + IPV6_HDR_LEN {
        bail!("IPv6 payload shorter than header");
    }

    let mut next_header = packet[6];
    let mut offset = IPV6_HDR_LEN;

    loop {
        match next_header {
            TCP_PROTOCOL | UDP_PROTOCOL => break,
            IPV6_NEXT_HEADER_HOP_BY_HOP
            | IPV6_NEXT_HEADER_ROUTING
            | IPV6_NEXT_HEADER_DESTINATION => {
                if l3_offset + offset + 2 > packet_end {
                    bail!("IPv6 extension header shorter than minimum length");
                }
                let ext = &packet[offset..];
                next_header = ext[0];
                let header_len = (usize::from(ext[1]) + 1) * 8;
                offset += header_len;
            }
            IPV6_NEXT_HEADER_FRAGMENT => {
                if l3_offset + offset + 8 > packet_end {
                    bail!("IPv6 fragment header shorter than minimum length");
                }
                let ext = &packet[offset..];
                let fragment_bits = u16::from_be_bytes([ext[2], ext[3]]);
                // RFC 2460 section 4.5: the encoded offset occupies bits 15..3 and is measured in
                // 8-byte units. Masking the low three flag bits therefore
                // already yields the byte offset; shifting again would make
                // every offset eight times too small.
                let fragment_offset_bytes = usize::from(fragment_bits & IPV6_FRAGMENT_OFFSET_MASK);
                let more_fragments = (fragment_bits & 0x1) != 0;
                let identification = u32::from_be_bytes([ext[4], ext[5], ext[6], ext[7]]);
                let fragment_payload_offset = offset + 8;
                let fragment_payload_end = packet_end - l3_offset;
                if fragment_payload_end < fragment_payload_offset {
                    bail!("IPv6 fragment payload shorter than header");
                }
                let ip_payload =
                    Bytes::copy_from_slice(&packet[fragment_payload_offset..fragment_payload_end]);
                let mut src_ip = [0u8; 16];
                src_ip.copy_from_slice(&packet[8..24]);
                let mut dst_ip = [0u8; 16];
                dst_ip.copy_from_slice(&packet[24..40]);
                let protocol = TransportProtocol::from_number(ext[0])?;
                let flow = FlowTuple::new(IpAddr::from_v6(src_ip), IpAddr::from_v6(dst_ip), 0, 0);

                return Ok(CapturedIpPacket {
                    socket_timestamp,
                    frame_len,
                    direction,
                    protocol,
                    flow,
                    ip_identification: identification as u16,
                    ip_payload,
                    fragment: Some(IpFragmentInfo {
                        key: IpFragmentKey {
                            src_ip: flow.src_ip,
                            dst_ip: flow.dst_ip,
                            identification,
                            protocol,
                        },
                        offset_bytes: fragment_offset_bytes,
                        more_fragments,
                    }),
                });
            }
            IPV6_NEXT_HEADER_AUTH => {
                if l3_offset + offset + 2 > packet_end {
                    bail!("IPv6 authentication header shorter than minimum length");
                }
                let ext = &packet[offset..];
                next_header = ext[0];
                let header_len = (usize::from(ext[1]) + 2) * 4;
                offset += header_len;
            }
            IPV6_NEXT_HEADER_ESP => bail!("IPv6 ESP payload is not supported"),
            IPV6_NEXT_HEADER_NO_NEXT => bail!("IPv6 packet has no next header"),
            other => bail!("unsupported IPv6 next header {other}"),
        }

        if l3_offset + offset > packet_end {
            bail!("IPv6 extension headers exceed payload length");
        }
    }

    let protocol = TransportProtocol::from_number(next_header)?;
    let transport_offset = l3_offset + offset;
    if transport_offset > packet_end {
        bail!("IPv6 transport payload offset exceeds packet length");
    }

    let mut src_ip = [0u8; 16];
    src_ip.copy_from_slice(&packet[8..24]);
    let mut dst_ip = [0u8; 16];
    dst_ip.copy_from_slice(&packet[24..40]);
    let flow = FlowTuple::new(IpAddr::from_v6(src_ip), IpAddr::from_v6(dst_ip), 0, 0);

    Ok(CapturedIpPacket {
        socket_timestamp,
        frame_len,
        direction,
        protocol,
        flow,
        ip_identification: 0,
        ip_payload: Bytes::copy_from_slice(&frame[transport_offset..packet_end]),
        fragment: None,
    })
}

fn parse_ipv4_l3_offset(frame: &[u8]) -> Result<usize> {
    let (ethertype, offset) = parse_l3_offset(frame)?;
    if ethertype != ETH_P_IPV4 {
        bail!("EtherType {ethertype:#06x} is not IPv4");
    }
    Ok(offset)
}

fn parse_ipv6_l3_offset(frame: &[u8]) -> Result<usize> {
    let (ethertype, offset) = parse_l3_offset(frame)?;
    if ethertype != ETH_P_IPV6 {
        bail!("EtherType {ethertype:#06x} is not IPv6");
    }
    Ok(offset)
}

fn parse_l3_offset(frame: &[u8]) -> Result<(u16, usize)> {
    if frame.len() < ETH_HDR_LEN {
        bail!("packet shorter than Ethernet header");
    }

    let mut offset = ETH_HDR_LEN;
    let mut ethertype = u16::from_be_bytes([frame[12], frame[13]]);

    let mut remaining_tags = 2;
    while remaining_tags > 0 && matches!(ethertype, ETH_P_8021Q | ETH_P_8021AD | ETH_P_8021QINQ) {
        if frame.len() < offset + VLAN_HDR_LEN {
            bail!("packet shorter than VLAN header");
        }
        ethertype = u16::from_be_bytes([frame[offset + 2], frame[offset + 3]]);
        offset += VLAN_HDR_LEN;
        remaining_tags -= 1;
    }

    Ok((ethertype, offset))
}

fn insert_range(ranges: &mut Vec<(usize, usize)>, start: usize, end: usize) {
    if start >= end {
        return;
    }

    // Insertion point: first range whose start is > `start`. Keep sorted by start.
    let insert_at = ranges.partition_point(|r| r.0 <= start);
    ranges.insert(insert_at, (start, end));

    // Merge forward: combine adjacent/overlapping ranges into `ranges[insert_at]`.
    let mut write = insert_at;
    // Extend left neighbor if it reaches or overlaps the newly-inserted range.
    if write > 0 && ranges[write - 1].1 >= ranges[write].0 {
        ranges[write - 1].1 = ranges[write - 1].1.max(ranges[write].1);
        ranges.remove(write);
        write -= 1;
    }
    // Absorb right neighbors as long as they touch or overlap.
    while write + 1 < ranges.len() && ranges[write].1 >= ranges[write + 1].0 {
        ranges[write].1 = ranges[write].1.max(ranges[write + 1].1);
        ranges.remove(write + 1);
    }
}

fn flow_is_complete(state: &FragmentFlowState) -> bool {
    let Some(total_len) = state.total_len else {
        return false;
    };
    let Some((start, end)) = state.ranges.first().copied() else {
        return false;
    };

    start == 0 && end >= total_len && state.ranges.len() == 1
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use super::*;

    #[test]
    fn ipv4_udp_transport_payload_strips_header_and_preserves_ports() {
        let frame = ipv4_udp_frame(32100, 7446, b"hello");
        let packet = parse_ipv4_packet(
            &frame,
            frame.len(),
            PacketDirection::Host,
            SystemTime::UNIX_EPOCH,
        )
        .unwrap();
        assert_eq!(packet.protocol, TransportProtocol::Udp);

        let datagram = IpFragmentReassembler::new(4).accept(packet).remove(0);
        let transport = datagram.into_transport_payload().unwrap();

        assert_eq!(transport.protocol, TransportProtocol::Udp);
        assert_eq!(transport.flow.src_port, 32100);
        assert_eq!(transport.flow.dst_port, 7446);
        assert_eq!(transport.payload.as_ref(), b"hello");
    }

    #[test]
    fn ipv4_tcp_transport_payload_strips_variable_header_and_preserves_ports() {
        let frame = ipv4_tcp_frame(50123, 7447, &[1, 1, 1, 1], b"zenoh");
        let packet = parse_ipv4_packet(
            &frame,
            frame.len(),
            PacketDirection::Host,
            SystemTime::UNIX_EPOCH,
        )
        .unwrap();
        assert_eq!(packet.protocol, TransportProtocol::Tcp);

        let datagram = IpFragmentReassembler::new(4).accept(packet).remove(0);
        let transport = datagram.into_transport_payload().unwrap();

        assert_eq!(transport.protocol, TransportProtocol::Tcp);
        assert_eq!(transport.flow.src_port, 50123);
        assert_eq!(transport.flow.dst_port, 7447);
        assert_eq!(transport.payload.as_ref(), b"zenoh");
    }

    #[test]
    fn ipv6_fragments_use_byte_offsets_and_clear_completed_flow_order() {
        let first_payload = [
            0x7d, 0x64, 0x1d, 0x1f, 0x00, 0x14, 0x00, 0x00, b'a', b'b', b'c', b'd', b'e', b'f',
            b'g', b'h',
        ];
        let second_payload = [b'i', b'j', b'k', b'l'];
        let first = parse_ipv6_packet(
            &ipv6_fragment_frame(77, 0, true, &first_payload),
            ETH_HDR_LEN + IPV6_HDR_LEN + 8 + first_payload.len(),
            PacketDirection::Host,
            SystemTime::UNIX_EPOCH,
        )
        .unwrap();
        let second = parse_ipv6_packet(
            &ipv6_fragment_frame(77, 16, false, &second_payload),
            ETH_HDR_LEN + IPV6_HDR_LEN + 8 + second_payload.len(),
            PacketDirection::Host,
            SystemTime::UNIX_EPOCH,
        )
        .unwrap();

        assert_eq!(second.fragment.as_ref().unwrap().offset_bytes(), 16);

        let mut reassembler = IpFragmentReassembler::new(4);
        assert!(reassembler.accept(first).is_empty());
        let datagrams = reassembler.accept(second);

        assert_eq!(datagrams.len(), 1);
        assert_eq!(
            datagrams[0].ip_payload.as_ref(),
            b"\x7d\x64\x1d\x1f\x00\x14\x00\x00abcdefghijkl"
        );
        assert!(reassembler.flows.is_empty());
        assert!(reassembler.order.is_empty());
    }

    fn ipv4_udp_frame(src_port: u16, dst_port: u16, payload: &[u8]) -> Vec<u8> {
        let mut l4 = Vec::new();
        l4.extend_from_slice(&src_port.to_be_bytes());
        l4.extend_from_slice(&dst_port.to_be_bytes());
        l4.extend_from_slice(&(UDP_HDR_LEN as u16 + payload.len() as u16).to_be_bytes());
        l4.extend_from_slice(&0u16.to_be_bytes());
        l4.extend_from_slice(payload);
        ipv4_frame(UDP_PROTOCOL, &l4)
    }

    fn ipv4_tcp_frame(src_port: u16, dst_port: u16, options: &[u8], payload: &[u8]) -> Vec<u8> {
        assert_eq!(options.len() % 4, 0);
        let tcp_header_len = TCP_MIN_HDR_LEN + options.len();
        let mut l4 = Vec::new();
        l4.extend_from_slice(&src_port.to_be_bytes());
        l4.extend_from_slice(&dst_port.to_be_bytes());
        l4.extend_from_slice(&0u32.to_be_bytes());
        l4.extend_from_slice(&0u32.to_be_bytes());
        l4.push(((tcp_header_len / 4) as u8) << 4);
        l4.push(0x18);
        l4.extend_from_slice(&1024u16.to_be_bytes());
        l4.extend_from_slice(&0u16.to_be_bytes());
        l4.extend_from_slice(&0u16.to_be_bytes());
        l4.extend_from_slice(options);
        l4.extend_from_slice(payload);
        ipv4_frame(TCP_PROTOCOL, &l4)
    }

    fn ipv4_frame(protocol: u8, l4_payload: &[u8]) -> Vec<u8> {
        let total_len = IPV4_MIN_HDR_LEN + l4_payload.len();
        let mut frame = vec![0u8; ETH_HDR_LEN + IPV4_MIN_HDR_LEN];
        frame[12..14].copy_from_slice(&ETH_P_IPV4.to_be_bytes());
        let ip = &mut frame[ETH_HDR_LEN..ETH_HDR_LEN + IPV4_MIN_HDR_LEN];
        ip[0] = 0x45;
        ip[2..4].copy_from_slice(&(total_len as u16).to_be_bytes());
        ip[4..6].copy_from_slice(&7u16.to_be_bytes());
        ip[8] = 64;
        ip[9] = protocol;
        ip[12..16].copy_from_slice(&[127, 0, 0, 1]);
        ip[16..20].copy_from_slice(&[127, 0, 0, 1]);
        frame.extend_from_slice(l4_payload);
        frame
    }

    fn ipv6_fragment_frame(
        identification: u32,
        offset_bytes: u16,
        more_fragments: bool,
        payload: &[u8],
    ) -> Vec<u8> {
        assert_eq!(offset_bytes % 8, 0);
        let payload_len = 8 + payload.len();
        let mut frame = vec![0u8; ETH_HDR_LEN + IPV6_HDR_LEN + 8];
        frame[12..14].copy_from_slice(&ETH_P_IPV6.to_be_bytes());
        let ipv6 = &mut frame[ETH_HDR_LEN..ETH_HDR_LEN + IPV6_HDR_LEN];
        ipv6[0] = 0x60;
        ipv6[4..6].copy_from_slice(&(payload_len as u16).to_be_bytes());
        ipv6[6] = IPV6_NEXT_HEADER_FRAGMENT;
        ipv6[7] = 64;
        ipv6[8..24].copy_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        ipv6[24..40].copy_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);

        let fragment = &mut frame[ETH_HDR_LEN + IPV6_HDR_LEN..];
        fragment[0] = UDP_PROTOCOL;
        let fragment_bits = offset_bytes | u16::from(more_fragments);
        fragment[2..4].copy_from_slice(&fragment_bits.to_be_bytes());
        fragment[4..8].copy_from_slice(&identification.to_be_bytes());
        frame.extend_from_slice(payload);
        frame
    }
}
