#![no_std]
#![no_main]

mod filter;
mod maps;
mod rtps;

use aya_ebpf::{macros::socket_filter, programs::SkBuffContext};

#[socket_filter]
pub fn ros2probe(ctx: SkBuffContext) -> i64 {
    filter::run(&ctx).unwrap_or(0)
}

#[cfg(not(test))]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}

// SPDX-License-Identifier: GPL-2.0 OR Apache-2.0
//
// The Linux BPF verifier only checks for the substring "GPL" in this section
// to gate access to GPL-only kernel helpers. Our source license is dual
// `GPL-2.0 OR Apache-2.0` (declared in this crate's Cargo.toml); when the
// program is loaded into the kernel, the GPL alternative applies and helpers
// are available.
#[unsafe(link_section = "license")]
#[unsafe(no_mangle)]
static LICENSE: [u8; 4] = *b"GPL\0";
