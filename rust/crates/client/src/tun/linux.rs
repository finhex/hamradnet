//! Linux TUN via `/dev/net/tun`.
//!
//! The interface is created with `IFF_TUN | IFF_NO_PI`: layer 3, and without the
//! 4-byte protocol header, so what we read is exactly an IP packet.
//!
//! Addressing is done by invoking `ip` rather than by speaking netlink. It is a
//! handful of calls made once at startup, and shelling out keeps this file small
//! and easy to verify against what an administrator would type by hand.

use std::process::Command;
use std::sync::atomic::{AtomicI32, Ordering};

use anyhow::{bail, Context, Result};

use super::TunConfig;

const IFF_TUN: libc::c_short = 0x0001;
const IFF_NO_PI: libc::c_short = 0x1000;
const TUNSETIFF: libc::c_ulong = 0x4004_54ca;

#[repr(C)]
struct IfReq {
    name: [libc::c_char; libc::IFNAMSIZ],
    flags: libc::c_short,
    _padding: [u8; 22],
}

pub struct Device {
    /// Set to -1 once closed, so a blocked reader can be released without the
    /// file descriptor being closed twice.
    fd: AtomicI32,
    name: String,
}

impl Device {
    pub fn open(config: &TunConfig) -> Result<Self> {
        if config.name.len() >= libc::IFNAMSIZ {
            bail!("interface name '{}' is too long", config.name);
        }

        let fd = unsafe { libc::open(c"/dev/net/tun".as_ptr(), libc::O_RDWR) };
        if fd < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::PermissionDenied {
                bail!("cannot open /dev/net/tun: {err}. Run the daemon as root.");
            }
            return Err(err).context("opening /dev/net/tun");
        }

        let mut req = IfReq {
            name: [0; libc::IFNAMSIZ],
            flags: IFF_TUN | IFF_NO_PI,
            _padding: [0; 22],
        };
        for (slot, byte) in req.name.iter_mut().zip(config.name.as_bytes()) {
            *slot = *byte as libc::c_char;
        }

        // `as _` because the request argument is c_ulong against glibc but
        // c_int against musl, and this builds for both.
        if unsafe { libc::ioctl(fd, TUNSETIFF as _, &mut req) } < 0 {
            let err = std::io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(err).context("TUNSETIFF failed while creating the interface");
        }

        let device = Device { fd: AtomicI32::new(fd), name: config.name.clone() };
        device.set_address(config.address, config.prefix_len)?;
        run("ip", &["link", "set", "dev", &config.name, "mtu", &config.mtu.to_string()])?;
        run("ip", &["link", "set", "dev", &config.name, "up"])?;
        Ok(device)
    }

    pub fn set_address(&self, address: std::net::Ipv4Addr, prefix_len: u8) -> Result<()> {
        // `replace` rather than `add` so reassignment after a reconnect does not
        // leave the previous address on the interface.
        let cidr = format!("{address}/{prefix_len}");
        run("ip", &["addr", "flush", "dev", &self.name])?;
        run("ip", &["addr", "replace", &cidr, "dev", &self.name])?;
        Ok(())
    }

    pub fn read(&self, buf: &mut [u8]) -> Result<usize> {
        let fd = self.fd.load(Ordering::SeqCst);
        if fd < 0 {
            bail!("the tun device is closed");
        }
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n < 0 {
            return Err(std::io::Error::last_os_error()).context("reading from the tun device");
        }
        Ok(n as usize)
    }

    pub fn write(&self, packet: &[u8]) -> Result<()> {
        let fd = self.fd.load(Ordering::SeqCst);
        if fd < 0 {
            bail!("the tun device is closed");
        }
        let n = unsafe { libc::write(fd, packet.as_ptr() as *const libc::c_void, packet.len()) };
        if n < 0 {
            return Err(std::io::Error::last_os_error()).context("writing to the tun device");
        }
        Ok(())
    }

    /// Closes the device, releasing a reader blocked in `read` with EBADF so its
    /// thread can exit. Safe to call more than once.
    pub fn stop(&self) {
        let fd = self.fd.swap(-1, Ordering::SeqCst);
        if fd >= 0 {
            unsafe { libc::close(fd) };
        }
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        // Closing the fd removes the interface, since it was never persisted.
        self.stop();
    }
}

// The kernel serialises concurrent read/write on a tun fd, so sharing the handle
// across the reader and writer threads needs no extra locking.
unsafe impl Send for Device {}
unsafe impl Sync for Device {}

fn run(program: &str, args: &[&str]) -> Result<()> {
    let output = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("running `{program} {}`", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "`{program} {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}
