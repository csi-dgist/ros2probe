#![no_std]

pub mod ebpf;
pub mod rtps;

pub use ebpf::*;
pub use event::*;
pub use rtps::*;

mod event;

#[cfg(feature = "user")]
mod pod {
    use super::{FlowTuple, IpAddr, Ipv4FragmentKey, TopicGid};

    unsafe impl aya::Pod for TopicGid {}
    unsafe impl aya::Pod for IpAddr {}
    unsafe impl aya::Pod for Ipv4FragmentKey {}
    unsafe impl aya::Pod for FlowTuple {}
}
