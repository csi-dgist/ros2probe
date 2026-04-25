use std::collections::VecDeque;
use std::time::Duration;

use anyhow::Context;
use bytes::Bytes;
use ros2probe_common::FlowTuple;
use crate::capture::{
    ip_frag::{parse_ipv4_packet, parse_ipv6_packet, IpFragmentReassembler},
    socket::{CaptureSocket, PacketDirection},
    ReassembledUdpPayload,
};

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

pub struct CaptureBuffer {
    packets: VecDeque<CapturedUdpPacket>,
    max_depth: usize,
}

impl CaptureBuffer {
    pub fn new(max_depth: usize) -> Self {
        Self {
            packets: VecDeque::with_capacity(max_depth),
            max_depth,
        }
    }

    pub fn packets(&self) -> &VecDeque<CapturedUdpPacket> {
        &self.packets
    }

    pub fn packets_mut(&mut self) -> &mut VecDeque<CapturedUdpPacket> {
        &mut self.packets
    }

    pub fn pop(&mut self) -> Option<CapturedUdpPacket> {
        self.packets.pop_front()
    }

    fn push(&mut self, packet: CapturedUdpPacket) {
        push_bounded(&mut self.packets, packet, self.max_depth);
    }
}

pub struct CaptureEngine {
    socket: CaptureSocket,
    ip_frag: IpFragmentReassembler,
}

impl CaptureEngine {
    pub fn open(interface: &str, fragment_capacity: usize) -> anyhow::Result<Self> {
        Ok(Self {
            socket: CaptureSocket::open(interface)?,
            ip_frag: IpFragmentReassembler::new(fragment_capacity),
        })
    }

    pub fn socket_mut(&mut self) -> &mut CaptureSocket {
        &mut self.socket
    }

    pub fn pump_once_blocking(&mut self, buffer: &mut CaptureBuffer) -> anyhow::Result<()> {
        let (socket, ip_frag) = (&mut self.socket, &mut self.ip_frag);
        let batch = socket.next_batch_blocking(Duration::from_millis(100))?;
        pump_from_batch(ip_frag, batch, buffer)
    }

    pub async fn pump_once(&mut self, buffer: &mut CaptureBuffer) -> anyhow::Result<()> {
        let (socket, ip_frag) = (&mut self.socket, &mut self.ip_frag);
        let batch = socket.next_batch().await?;
        pump_from_batch(ip_frag, batch, buffer)
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
                match datagram.into_udp_payload() {
                    Ok(payload) => packets.push(CapturedUdpPacket::from(payload)),
                    Err(_) => {}
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
            if let Ok(udp_payload) = datagram.into_udp_payload() {
                buffer.push(CapturedUdpPacket::from(udp_payload));
            }
        }
    }

    Ok(())
}

fn push_bounded(queue: &mut VecDeque<CapturedUdpPacket>, packet: CapturedUdpPacket, max_depth: usize) {
    if queue.len() == max_depth {
        queue.pop_front();
    }
    queue.push_back(packet);
}
