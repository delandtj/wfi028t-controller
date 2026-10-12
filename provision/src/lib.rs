//! Provisioning pieces of ADR 0004 that need no hardware: the on-flash
//! records for WiFi, the admin password and the device, and the small
//! responders of setup mode (DHCP server, captive DNS, mDNS).
//!
//! Everything here works on byte slices and plain values, so the firmware
//! only has to move bytes between sockets, flash and these functions, and
//! the logic is tested on the host with `cargo test -p provision`.
//!
//! `no_std`, no allocator. `std` is pulled in only for the test build.
#![cfg_attr(not(test), no_std)]
#![deny(unsafe_code)]

pub mod dhcp;
pub mod dns;
pub mod mdns;
