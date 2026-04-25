use std::{
    collections::{HashMap, VecDeque},
    time::SystemTime,
};

use anyhow::{bail, Result};
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
const UDP_PROTOCOL: u8 = 17;
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
    pub flow: FlowTuple,
    pub ip_identification: u16,
    /// Everything after the IP header — **including the 8-byte UDP header**.
    /// `into_udp_payload` strips the UDP header by sub-slicing
    /// `[UDP_HDR_LEN..udp_len]`, so do not strip it upstream. `Bytes` so that
    /// downstream sub-slicing (UDP header strip, RTPS submessage slicing) is
    /// zero-copy — they share the one allocation created when the frame is
    /// lifted out of the netring buffer.
    pub ip_payload: Bytes,
    pub fragment: Option<IpFragmentInfo>,
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
    pub protocol: u8,
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
                flow: packet.flow,
                ip_identification: packet.ip_identification,
                ip_payload: packet.ip_payload,
                fragment_count: 1,
                was_fragmented: false,
            }];
        };

        let key = fragment.key();
        if !self.flows.contains_key(&key) {
            if self.flows.len() == self.capacity {
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
    pub fn into_udp_payload(self) -> Result<ReassembledUdpPayload> {
        if self.ip_payload.len() < UDP_HDR_LEN {
            bail!("reassembled IP payload shorter than UDP header");
        }

        let udp_len = usize::from(u16::from_be_bytes([self.ip_payload[4], self.ip_payload[5]]));
        if udp_len < UDP_HDR_LEN {
            bail!("UDP length shorter than header");
        }
        let available = self.ip_payload.len().min(udp_len);
        if available < UDP_HDR_LEN {
            bail!("reassembled UDP datagram shorter than header");
        }

        // Zero-copy: `Bytes::slice` just bumps a refcount and adjusts the
        // offset/length view into the same allocation. Previously this used
        // `Vec::truncate` + `Vec::drain(..UDP_HDR_LEN)` which memmoved the
        // entire payload (~1400 bytes for MTU-sized datagrams) down by 8.
        let udp_payload = self.ip_payload.slice(UDP_HDR_LEN..available);

        Ok(ReassembledUdpPayload {
            socket_timestamp: self.socket_timestamp,
            frame_len: self.frame_len,
            direction: self.direction,
            flow: self.flow,
            ip_identification: self.ip_identification,
            udp_payload,
            fragment_count: self.fragment_count,
            was_fragmented: self.was_fragmented,
        })
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
        bail!("unexpected IPv4 header length nibble {}", version_ihl & 0x0f);
    }
    let ip_header_len = ihl_words * 4;
    if packet.len() < ip_header_len {
        bail!("packet shorter than declared IPv4 header length");
    }

    if packet[9] != UDP_PROTOCOL {
        bail!("IPv4 packet is not UDP");
    }

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
            protocol: packet[9],
        },
        offset_bytes: fragment_offset_bytes,
        more_fragments,
    });

    Ok(CapturedIpPacket {
        socket_timestamp,
        frame_len,
        direction,
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
            UDP_PROTOCOL => break,
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
                // RFC 2460 §4.5: offset field is in 8-octet units (bits 15–3), must shift right by 3.
                let fragment_offset_bytes =
                    usize::from(fragment_bits & IPV6_FRAGMENT_OFFSET_MASK) >> 3;
                let more_fragments = (fragment_bits & 0x1) != 0;
                let identification =
                    u32::from_be_bytes([ext[4], ext[5], ext[6], ext[7]]);
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
                let flow = FlowTuple::new(IpAddr::from_v6(src_ip), IpAddr::from_v6(dst_ip), 0, 0);

                return Ok(CapturedIpPacket {
                    socket_timestamp,
                    frame_len,
                    direction,
                    flow,
                    ip_identification: identification as u16,
                    ip_payload,
                    fragment: Some(IpFragmentInfo {
                        key: IpFragmentKey {
                            src_ip: flow.src_ip,
                            dst_ip: flow.dst_ip,
                            identification,
                            protocol: ext[0],
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

    let udp_offset = l3_offset + offset;
    if udp_offset + UDP_HDR_LEN > packet_end {
        bail!("IPv6 UDP datagram shorter than UDP header");
    }
    let udp = &frame[udp_offset..packet_end];
    let udp_len = usize::from(u16::from_be_bytes([udp[4], udp[5]]));
    if udp_len < UDP_HDR_LEN {
        bail!("IPv6 UDP length shorter than header");
    }
    let available = udp.len().min(udp_len);
    if available < UDP_HDR_LEN {
        bail!("IPv6 UDP payload shorter than header");
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
        flow,
        ip_identification: 0,
        ip_payload: Bytes::copy_from_slice(&udp[..available]),
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
    while remaining_tags > 0
        && matches!(ethertype, ETH_P_8021Q | ETH_P_8021AD | ETH_P_8021QINQ)
    {
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
