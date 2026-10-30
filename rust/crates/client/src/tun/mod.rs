//! The virtual network interface.
//!
//! Both platforms expose the same shape: a handle that is `Send + Sync` and
//! whose `read`/`write` take `&self`, so one `Arc<Tun>` can be shared by a
//! reader thread and a writer thread without further locking.
//!
//! Reads block. That is deliberate - the device is driven from dedicated
//! threads rather than the async runtime, because neither platform offers a
//! portable readiness primitive that tokio can poll directly.

use std::net::Ipv4Addr;

use anyhow::Result;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as platform;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as platform;

/// How the interface should be set up.
#[derive(Debug, Clone)]
pub struct TunConfig {
    pub name: String,
    pub address: Ipv4Addr,
    pub prefix_len: u8,
    pub mtu: u32,
}

pub struct Tun {
    inner: platform::Device,
}

impl Tun {
    pub fn open(config: &TunConfig) -> Result<Self> {
        Ok(Self { inner: platform::Device::open(config)? })
    }

    /// Reads one IP packet. Blocks until a packet is available.
    pub fn read(&self, buf: &mut [u8]) -> Result<usize> {
        self.inner.read(buf)
    }

    /// Writes one IP packet into the interface.
    pub fn write(&self, packet: &[u8]) -> Result<()> {
        self.inner.write(packet)
    }

    /// Shuts the device down and unblocks any reader waiting on it.
    pub fn stop(&self) {
        self.inner.stop();
    }

    /// Reassigns the interface address. Needed after reconnecting to the
    /// coordinator, which hands out a fresh address per connection.
    pub fn set_address(&self, address: Ipv4Addr, prefix_len: u8) -> Result<()> {
        self.inner.set_address(address, prefix_len)
    }
}

/// Converts a prefix length into a dotted netmask.
pub fn netmask_from_prefix(prefix_len: u8) -> Ipv4Addr {
    let bits: u32 = match prefix_len {
        0 => 0,
        p if p >= 32 => u32::MAX,
        p => u32::MAX << (32 - p),
    };
    Ipv4Addr::from(bits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_converts_to_netmask() {
        assert_eq!(netmask_from_prefix(16), Ipv4Addr::new(255, 255, 0, 0));
        assert_eq!(netmask_from_prefix(24), Ipv4Addr::new(255, 255, 255, 0));
        assert_eq!(netmask_from_prefix(8), Ipv4Addr::new(255, 0, 0, 0));
        assert_eq!(netmask_from_prefix(32), Ipv4Addr::new(255, 255, 255, 255));
        assert_eq!(netmask_from_prefix(0), Ipv4Addr::new(0, 0, 0, 0));
    }
}
