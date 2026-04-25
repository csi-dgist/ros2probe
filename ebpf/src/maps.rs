use aya_ebpf::{
    macros::map,
    maps::{HashMap, LruHashMap},
};
use ros2probe_common::{MAX_FRAGMENT_FLOWS, MAX_TOPIC_GIDS, TopicGid};

#[map]
pub static TOPIC_FILTER_GIDS: HashMap<TopicGid, u8> =
    HashMap::<TopicGid, u8>::with_max_entries(MAX_TOPIC_GIDS, 0);

pub fn gid_state(gid: &TopicGid) -> Option<u8> {
    unsafe { TOPIC_FILTER_GIDS.get(gid) }.copied()
}

/// Per-datagram "approved fragments" table. Keyed by the IP-layer fragment
/// identity, one entry is inserted when the first fragment of an RTPS-bearing
/// datagram has been passed upstream. Subsequent middle/trailing fragments —
/// which have no UDP/RTPS headers and therefore can't be classified on their
/// own — look up this key to decide pass vs drop. LRU eviction handles
/// orphaned entries (first fragment passed but the tail never arrived).
///
/// **Known limitation**: under extreme pressure (more than `MAX_FRAGMENT_FLOWS`
/// concurrently in-flight fragmented datagrams) LRU may evict an approved
/// entry before its last fragment arrives, dropping the tail and stalling
/// userspace reassembly for that datagram. The 4096 default is far above the
/// typical DDS fragmentation load, so this is documented rather than avoided.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FragmentFlowKey {
    pub src_ip: [u8; 16],
    pub dst_ip: [u8; 16],
    pub identification: u32,
    /// Upper-layer protocol (IPv4 `protocol` field / IPv6 fragment extension
    /// `next header`). Kept as `u32` to avoid implicit struct padding, which
    /// eBPF map hashing would otherwise include as uninitialized bytes.
    pub protocol: u32,
}

#[map]
pub static FRAG_FLOW_PASS: LruHashMap<FragmentFlowKey, u8> =
    LruHashMap::<FragmentFlowKey, u8>::with_max_entries(MAX_FRAGMENT_FLOWS, 0);

pub fn frag_flow_mark(key: &FragmentFlowKey) {
    let _ = FRAG_FLOW_PASS.insert(key, &1, 0);
}

pub fn frag_flow_approved(key: &FragmentFlowKey) -> bool {
    unsafe { FRAG_FLOW_PASS.get(key) }.is_some()
}

pub fn frag_flow_forget(key: &FragmentFlowKey) {
    let _ = FRAG_FLOW_PASS.remove(key);
}
