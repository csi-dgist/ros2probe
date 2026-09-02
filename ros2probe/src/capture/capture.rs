use std::collections::{BTreeSet, VecDeque};
use std::time::Duration;

use anyhow::Context;
use bytes::Bytes;
use ros2probe_common::{FlowTuple, RTPS_SIGNATURE, ZENOH_TRANSPORT_PORT};

use crate::capture::{
    ReassembledUdpPayload, TcpSegmentInfo,
    ip_frag::{
        IpFragmentReassembler, ReassembledTransportPayload, TransportProtocol, parse_ipv4_packet,
        parse_ipv6_packet,
    },
    socket::{CaptureSocket, PacketDirection},
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ZenohCapturePorts {
    udp_ports: BTreeSet<u16>,
    tcp_ports: BTreeSet<u16>,
}

impl Default for ZenohCapturePorts {
    fn default() -> Self {
        Self::from_transport_ports([ZENOH_TRANSPORT_PORT])
    }
}

impl ZenohCapturePorts {
    pub fn from_transport_ports(transport_ports: impl IntoIterator<Item = u16>) -> Self {
        let transport_ports = transport_ports.into_iter().collect::<BTreeSet<_>>();

        Self {
            udp_ports: transport_ports.clone(),
            tcp_ports: transport_ports,
        }
    }

    pub fn udp_ports(&self) -> &BTreeSet<u16> {
        &self.udp_ports
    }

    pub fn tcp_ports(&self) -> &BTreeSet<u16> {
        &self.tcp_ports
    }

    fn is_udp_port(&self, port: u16) -> bool {
        self.udp_ports.contains(&port)
    }

    fn is_tcp_port(&self, port: u16) -> bool {
        self.tcp_ports.contains(&port)
    }
}

#[derive(Clone)]
pub struct CapturedUdpPacket {
    pub socket_timestamp: std::time::SystemTime,
    pub frame_len: usize,
    pub direction: PacketDirection,
    pub flow: FlowTuple,
    pub ip_identification: u16,
    /// UDP payload bytes. Carries the original frame-allocation ownership
    /// forward so RTPS parsing can sub-slice submessage and DATA payloads
    /// with zero copy.
    pub payload: Bytes,
    pub ip_fragment_count: u32,
    pub was_ip_fragmented: bool,
}

impl From<ReassembledUdpPayload> for CapturedUdpPacket {
    fn from(value: ReassembledUdpPayload) -> Self {
        Self {
            socket_timestamp: value.socket_timestamp,
            frame_len: value.frame_len,
            direction: value.direction,
            flow: value.flow,
            ip_identification: value.ip_identification,
            payload: value.udp_payload,
            ip_fragment_count: value.fragment_count,
            was_ip_fragmented: value.was_fragmented,
        }
    }
}

#[derive(Clone)]
pub struct CapturedTransportPacket {
    pub socket_timestamp: std::time::SystemTime,
    pub frame_len: usize,
    pub direction: PacketDirection,
    pub protocol: TransportProtocol,
    pub flow: FlowTuple,
    pub tcp: Option<TcpSegmentInfo>,
    pub ip_identification: u16,
    /// L4 payload bytes. For UDP this excludes the UDP header; for TCP this
    /// excludes the TCP header and may be empty for pure ACK/control packets.
    pub payload: Bytes,
    pub ip_fragment_count: u32,
    pub was_ip_fragmented: bool,
}

impl From<ReassembledTransportPayload> for CapturedTransportPacket {
    fn from(value: ReassembledTransportPayload) -> Self {
        Self {
            socket_timestamp: value.socket_timestamp,
            frame_len: value.frame_len,
            direction: value.direction,
            protocol: value.protocol,
            flow: value.flow,
            tcp: value.tcp,
            ip_identification: value.ip_identification,
            payload: value.payload,
            ip_fragment_count: value.fragment_count,
            was_ip_fragmented: value.was_fragmented,
        }
    }
}

impl CapturedUdpPacket {
    fn from_transport(value: CapturedTransportPacket) -> Option<Self> {
        (value.protocol == TransportProtocol::Udp).then_some(Self {
            socket_timestamp: value.socket_timestamp,
            frame_len: value.frame_len,
            direction: value.direction,
            flow: value.flow,
            ip_identification: value.ip_identification,
            payload: value.payload,
            ip_fragment_count: value.ip_fragment_count,
            was_ip_fragmented: value.was_ip_fragmented,
        })
    }
}

pub struct CaptureBuffer {
    packets: VecDeque<CapturedUdpPacket>,
    zenoh_packets: VecDeque<CapturedTransportPacket>,
    max_depth: usize,
}

enum CapturedPacket {
    Rtps(CapturedUdpPacket),
    Zenoh(CapturedTransportPacket),
}

impl CaptureBuffer {
    pub fn new(max_depth: usize) -> Self {
        Self {
            packets: VecDeque::with_capacity(max_depth),
            zenoh_packets: VecDeque::with_capacity(max_depth),
            max_depth,
        }
    }

    pub fn packets(&self) -> &VecDeque<CapturedUdpPacket> {
        &self.packets
    }

    pub fn packets_mut(&mut self) -> &mut VecDeque<CapturedUdpPacket> {
        &mut self.packets
    }

    pub fn zenoh_packets(&self) -> &VecDeque<CapturedTransportPacket> {
        &self.zenoh_packets
    }

    pub fn pop(&mut self) -> Option<CapturedUdpPacket> {
        self.packets.pop_front()
    }

    pub fn pop_zenoh(&mut self) -> Option<CapturedTransportPacket> {
        self.zenoh_packets.pop_front()
    }

    fn push(&mut self, packet: CapturedUdpPacket) {
        push_bounded(&mut self.packets, packet, self.max_depth);
    }

    fn push_zenoh(&mut self, packet: CapturedTransportPacket) {
        push_bounded(&mut self.zenoh_packets, packet, self.max_depth);
    }
}

pub struct CaptureEngine {
    socket: CaptureSocket,
    ip_frag: IpFragmentReassembler,
    zenoh_ports: ZenohCapturePorts,
}

impl CaptureEngine {
    pub fn open(
        interface: &str,
        fragment_capacity: usize,
        zenoh_ports: ZenohCapturePorts,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            socket: CaptureSocket::open(interface)?,
            ip_frag: IpFragmentReassembler::new(fragment_capacity),
            zenoh_ports,
        })
    }

    pub fn socket_mut(&mut self) -> &mut CaptureSocket {
        &mut self.socket
    }

    pub fn socket_stats(&self) -> anyhow::Result<netring::CaptureStats> {
        self.socket.stats()
    }

    pub fn pump_once_blocking(&mut self, buffer: &mut CaptureBuffer) -> anyhow::Result<()> {
        let (socket, ip_frag) = (&mut self.socket, &mut self.ip_frag);
        let batch = socket.next_batch_blocking(Duration::from_millis(100))?;
        pump_from_batch(ip_frag, batch, buffer, &self.zenoh_ports)
    }

    pub async fn pump_once(&mut self, buffer: &mut CaptureBuffer) -> anyhow::Result<()> {
        let (socket, ip_frag) = (&mut self.socket, &mut self.ip_frag);
        let batch = socket.next_batch().await?;
        pump_from_batch(ip_frag, batch, buffer, &self.zenoh_ports)
    }

    pub async fn capture_once(&mut self) -> anyhow::Result<Vec<CapturedUdpPacket>> {
        let mut packets = Vec::new();
        let Some(batch) = self
            .socket
            .next_batch()
            .await
            .context("read next AF_PACKET batch")?
        else {
            return Ok(packets);
        };

        for frame in batch.frames() {
            let parsed = match parse_ipv4_packet(
                frame.data(),
                frame.original_len(),
                frame.direction(),
                frame.socket_timestamp(),
            ) {
                Ok(packet) => packet,
                Err(_) => continue,
            };

            for datagram in self.ip_frag.accept(parsed) {
                if let Ok(payload) = datagram.into_udp_payload() {
                    packets.push(CapturedUdpPacket::from(payload));
                }
            }
        }

        Ok(packets)
    }
}

fn pump_from_batch(
    ip_frag: &mut IpFragmentReassembler,
    batch: Option<crate::capture::socket::PacketBatch<'_>>,
    buffer: &mut CaptureBuffer,
    zenoh_ports: &ZenohCapturePorts,
) -> anyhow::Result<()> {
    let Some(batch) = batch else {
        return Ok(());
    };

    for frame in batch.frames() {
        let parsed = match parse_ipv4_packet(
            frame.data(),
            frame.original_len(),
            frame.direction(),
            frame.socket_timestamp(),
        ) {
            Ok(packet) => packet,
            Err(_) => match parse_ipv6_packet(
                frame.data(),
                frame.original_len(),
                frame.direction(),
                frame.socket_timestamp(),
            ) {
                Ok(packet) => packet,
                Err(_) => continue,
            },
        };

        for datagram in ip_frag.accept(parsed) {
            let Ok(transport_payload) = datagram.into_transport_payload() else {
                continue;
            };
            let packet = CapturedTransportPacket::from(transport_payload);
            match classify_transport_packet(packet, zenoh_ports) {
                Some(CapturedPacket::Rtps(packet)) => buffer.push(packet),
                Some(CapturedPacket::Zenoh(packet)) => buffer.push_zenoh(packet),
                None => {}
            }
        }
    }

    Ok(())
}

fn classify_transport_packet(
    packet: CapturedTransportPacket,
    zenoh_ports: &ZenohCapturePorts,
) -> Option<CapturedPacket> {
    match packet.protocol {
        TransportProtocol::Tcp => {
            is_zenoh_packet(&packet, zenoh_ports).then_some(CapturedPacket::Zenoh(packet))
        }
        TransportProtocol::Udp => {
            if is_rtps_payload(&packet.payload) {
                CapturedUdpPacket::from_transport(packet).map(CapturedPacket::Rtps)
            } else if is_zenoh_packet(&packet, zenoh_ports) {
                Some(CapturedPacket::Zenoh(packet))
            } else {
                None
            }
        }
    }
}

fn is_rtps_payload(payload: &[u8]) -> bool {
    payload.starts_with(&RTPS_SIGNATURE)
}

fn is_zenoh_packet(packet: &CapturedTransportPacket, zenoh_ports: &ZenohCapturePorts) -> bool {
    match packet.protocol {
        TransportProtocol::Udp => {
            zenoh_ports.is_udp_port(packet.flow.src_port)
                || zenoh_ports.is_udp_port(packet.flow.dst_port)
        }
        TransportProtocol::Tcp => {
            zenoh_ports.is_tcp_port(packet.flow.src_port)
                || zenoh_ports.is_tcp_port(packet.flow.dst_port)
        }
    }
}

fn push_bounded<T>(queue: &mut VecDeque<T>, packet: T, max_depth: usize) {
    if max_depth == 0 {
        return;
    }
    if queue.len() >= max_depth {
        queue.pop_front();
    }
    queue.push_back(packet);
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use ros2probe_common::IpAddr;

    use super::*;

    #[test]
    fn udp_rtps_magic_takes_precedence_over_zenoh_port() {
        let packet = transport_packet(TransportProtocol::Udp, 32100, 7447, b"RTPS payload");

        match classify_transport_packet(packet, &ZenohCapturePorts::default()) {
            Some(CapturedPacket::Rtps(packet)) => assert_eq!(packet.flow.dst_port, 7447),
            _ => panic!("expected RTPS packet"),
        }
    }

    #[test]
    fn udp_non_rtps_on_zenoh_port_goes_to_zenoh() {
        let packet = transport_packet(TransportProtocol::Udp, 32100, 7447, b"zenoh");

        match classify_transport_packet(packet, &ZenohCapturePorts::default()) {
            Some(CapturedPacket::Zenoh(packet)) => assert_eq!(packet.flow.dst_port, 7447),
            _ => panic!("expected Zenoh packet"),
        }
    }

    #[test]
    fn tcp_on_zenoh_port_goes_to_zenoh() {
        let packet = transport_packet(TransportProtocol::Tcp, 50123, 7447, b"zenoh");

        match classify_transport_packet(packet, &ZenohCapturePorts::default()) {
            Some(CapturedPacket::Zenoh(packet)) => {
                assert_eq!(packet.protocol, TransportProtocol::Tcp)
            }
            _ => panic!("expected Zenoh packet"),
        }
    }

    #[test]
    fn custom_transport_port_replaces_default_transport_port() {
        let ports = ZenohCapturePorts::from_transport_ports([8447]);

        let old_transport = transport_packet(TransportProtocol::Tcp, 50123, 7447, b"zenoh");
        assert!(classify_transport_packet(old_transport, &ports).is_none());

        let custom_transport = transport_packet(TransportProtocol::Tcp, 50123, 8447, b"zenoh");
        assert!(matches!(
            classify_transport_packet(custom_transport, &ports),
            Some(CapturedPacket::Zenoh(_))
        ));

        let old_udp_default = transport_packet(TransportProtocol::Udp, 50123, 7446, b"zenoh");
        assert!(classify_transport_packet(old_udp_default, &ports).is_none());

        let custom_udp_transport = transport_packet(TransportProtocol::Udp, 50123, 8447, b"zenoh");
        assert!(matches!(
            classify_transport_packet(custom_udp_transport, &ports),
            Some(CapturedPacket::Zenoh(_))
        ));
    }

    fn transport_packet(
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
            flow: FlowTuple::new(
                IpAddr::from_v4(u32::from_be_bytes([127, 0, 0, 1])),
                IpAddr::from_v4(u32::from_be_bytes([127, 0, 0, 1])),
                src_port,
                dst_port,
            ),
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
