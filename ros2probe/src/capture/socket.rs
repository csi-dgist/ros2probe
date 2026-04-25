use std::time::Duration;

use anyhow::Context;
use netring::{
    AfPacketRx, AfPacketRxBuilder, Packet as NetringPacket, PacketDirection as NetringPacketDirection,
    PacketSource, TimestampSource,
};

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
        let inner = AfPacketRxBuilder::default()
            .interface(interface)
            .promiscuous(true)
            .block_size(256 * 1024)
            .block_count(8)
            .timestamp_source(TimestampSource::Software)
            .build()
            .with_context(|| format!("create AF_PACKET socket for interface {interface}"))?;
        Ok(Self { inner })
    }

    pub fn as_mut_inner(&mut self) -> &mut AfPacketRx {
        &mut self.inner
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
