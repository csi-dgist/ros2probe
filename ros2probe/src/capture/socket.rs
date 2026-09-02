use std::time::Duration;

use anyhow::Context;
use netring::{
    AfPacketRx, AfPacketRxBuilder, CaptureStats, Packet as NetringPacket,
    PacketDirection as NetringPacketDirection, PacketSource, TimestampSource,
};

const CAPTURE_RING_BLOCK_SIZE: usize = 256 * 1024;
const CAPTURE_RING_BLOCK_COUNT: usize = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PacketDirection {
    Host,
    Broadcast,
    Multicast,
    OtherHost,
    Outgoing,
    Unknown(u8),
}

impl From<NetringPacketDirection> for PacketDirection {
    fn from(value: NetringPacketDirection) -> Self {
        match value {
            NetringPacketDirection::Host => Self::Host,
            NetringPacketDirection::Broadcast => Self::Broadcast,
            NetringPacketDirection::Multicast => Self::Multicast,
            NetringPacketDirection::OtherHost => Self::OtherHost,
            NetringPacketDirection::Outgoing => Self::Outgoing,
            NetringPacketDirection::Unknown(v) => Self::Unknown(v),
        }
    }
}

pub struct CaptureSocket {
    inner: AfPacketRx,
}

impl CaptureSocket {
    pub fn open(interface: &str) -> anyhow::Result<Self> {
        let is_loopback = interface == "lo";
        let inner = AfPacketRxBuilder::default()
            .interface(interface)
            .promiscuous(true)
            // Discovery storms can deliver hundreds of endpoint announcements
            // in a fraction of a second. Keep enough kernel-side buffering for
            // those bursts while preserving the existing block granularity.
            .block_size(CAPTURE_RING_BLOCK_SIZE)
            .block_count(CAPTURE_RING_BLOCK_COUNT)
            // AF_PACKET exposes both the outgoing and host copy on loopback.
            // The host copy contains the same RTPS datagram, so discarding the
            // outgoing duplicate halves burst pressure without losing local
            // discovery traffic. Physical interfaces must retain outgoing
            // traffic so locally-originated discovery remains observable.
            .ignore_outgoing(is_loopback)
            .timestamp_source(TimestampSource::Software)
            .build()
            .with_context(|| format!("create AF_PACKET socket for interface {interface}"))?;
        Ok(Self { inner })
    }

    pub fn as_mut_inner(&mut self) -> &mut AfPacketRx {
        &mut self.inner
    }

    /// Return and reset the kernel's packet counters for this socket.
    pub fn stats(&self) -> anyhow::Result<CaptureStats> {
        self.inner
            .stats()
            .context("read AF_PACKET socket statistics")
    }

    pub fn next_batch_blocking(
        &mut self,
        timeout: Duration,
    ) -> anyhow::Result<Option<PacketBatch<'_>>> {
        self.inner
            .next_batch_blocking(timeout)
            .context("recv from AF_PACKET socket")
            .map(|batch| batch.map(PacketBatch::new))
    }

    pub async fn next_batch(&mut self) -> anyhow::Result<Option<PacketBatch<'_>>> {
        tokio::task::yield_now().await;
        self.next_batch_blocking(Duration::from_millis(100))
    }
}

pub struct PacketBatch<'a> {
    inner: netring::PacketBatch<'a>,
}

impl<'a> PacketBatch<'a> {
    fn new(inner: netring::PacketBatch<'a>) -> Self {
        Self { inner }
    }

    pub fn frames(&'a self) -> impl Iterator<Item = PacketFrame<'a>> + 'a {
        self.inner.iter().map(PacketFrame::new)
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }
}

pub struct PacketFrame<'a> {
    inner: NetringPacket<'a>,
}

impl<'a> PacketFrame<'a> {
    fn new(inner: NetringPacket<'a>) -> Self {
        Self { inner }
    }

    pub fn data(&self) -> &'a [u8] {
        self.inner.data()
    }

    pub fn socket_timestamp(&self) -> std::time::SystemTime {
        self.inner.timestamp().to_system_time()
    }

    pub fn original_len(&self) -> usize {
        self.inner.original_len()
    }

    pub fn direction(&self) -> PacketDirection {
        self.inner.direction().into()
    }
}
