//! Windows TUN via Wintun.
//!
//! Wintun is WireGuard's layer 3 adapter. It ships as a plain DLL that is loaded
//! at runtime - there is no driver package to install and no unsigned-driver
//! prompt, which is the main reason this design chose layer 3 over a TAP
//! adapter. Put `wintun.dll` next to `hamradnet.exe`.

use std::sync::Arc;

use anyhow::{bail, Context, Result};

use super::{netmask_from_prefix, TunConfig};

/// Windows sends a 255.255.255.255 broadcast out of the interface with the
/// lowest metric only, so making that the tunnel lets games that announce
/// themselves that way reach the other players. It is the fix Hamachi and
/// Radmin VPN users are told to apply by hand. Everything else works without
/// it, so a failure is only reported.
fn prefer_for_broadcasts(name: &str) {
    use std::os::windows::process::CommandExt;
    // Keeps a console window from flashing up behind the app.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let result = std::process::Command::new("netsh")
        .args(["interface", "ipv4", "set", "interface", &format!("interface={name}"), "metric=1"])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    match result {
        Ok(output) if output.status.success() => {}
        Ok(output) => tracing::warn!(
            output = %String::from_utf8_lossy(&output.stdout).trim(),
            "could not make the tunnel the preferred interface for LAN broadcasts"
        ),
        Err(err) => tracing::warn!(
            %err,
            "could not make the tunnel the preferred interface for LAN broadcasts"
        ),
    }
}

pub struct Device {
    session: Arc<wintun::Session>,
    // The adapter must outlive the session; dropping it tears down the interface.
    _adapter: Arc<wintun::Adapter>,
}

impl Device {
    pub fn open(config: &TunConfig) -> Result<Self> {
        let wintun = load_wintun()?;

        // Reuse an adapter left behind by an unclean shutdown rather than
        // failing or accumulating duplicates.
        let adapter = match wintun::Adapter::open(&wintun, &config.name) {
            Ok(existing) => existing,
            Err(_) => wintun::Adapter::create(&wintun, &config.name, "HamRadNet", None)
                .map_err(|err| anyhow::anyhow!("creating the Wintun adapter: {err}"))
                .context("is hamradnet running as Administrator?")?,
        };

        adapter
            .set_mtu(config.mtu as usize)
            .map_err(|err| anyhow::anyhow!("setting the adapter MTU: {err}"))?;

        let session = adapter
            .start_session(wintun::MAX_RING_CAPACITY)
            .map_err(|err| anyhow::anyhow!("starting the Wintun session: {err}"))?;

        let device = Self { session: Arc::new(session), _adapter: adapter };
        device.set_address(config.address, config.prefix_len)?;
        prefer_for_broadcasts(&config.name);
        Ok(device)
    }

    /// Releases a reader blocked in `receive_blocking` so its thread can exit.
    pub fn stop(&self) {
        let _ = self.session.shutdown();
    }

    pub fn set_address(&self, address: std::net::Ipv4Addr, prefix_len: u8) -> Result<()> {
        self._adapter
            .set_address(address)
            .map_err(|err| anyhow::anyhow!("setting the adapter address: {err}"))?;
        self._adapter
            .set_netmask(netmask_from_prefix(prefix_len))
            .map_err(|err| anyhow::anyhow!("setting the adapter netmask: {err}"))?;
        Ok(())
    }

    pub fn read(&self, buf: &mut [u8]) -> Result<usize> {
        let packet = self
            .session
            .receive_blocking()
            .map_err(|err| anyhow::anyhow!("reading from the adapter: {err}"))?;
        let bytes = packet.bytes();
        if bytes.len() > buf.len() {
            // Larger than our MTU: the only safe thing is to drop it.
            bail!("received a {}-byte packet, larger than the buffer", bytes.len());
        }
        buf[..bytes.len()].copy_from_slice(bytes);
        Ok(bytes.len())
    }

    pub fn write(&self, data: &[u8]) -> Result<()> {
        let len: u16 = data
            .len()
            .try_into()
            .map_err(|_| anyhow::anyhow!("packet too large for the adapter"))?;
        let mut packet = self
            .session
            .allocate_send_packet(len)
            .map_err(|err| anyhow::anyhow!("allocating a send packet: {err}"))?;
        packet.bytes_mut().copy_from_slice(data);
        self.session.send_packet(packet);
        Ok(())
    }
}

/// Looks for `wintun.dll` beside the executable first, so a self-contained
/// folder works, then falls back to the normal DLL search path.
fn load_wintun() -> Result<wintun::Wintun> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join("wintun.dll");
            if candidate.exists() {
                if let Ok(loaded) = unsafe { wintun::load_from_path(&candidate) } {
                    return Ok(loaded);
                }
            }
        }
    }
    unsafe { wintun::load() }.map_err(|err| {
        anyhow::anyhow!(
            "could not load wintun.dll ({err}). Place wintun.dll next to hamradnet.exe."
        )
    })
}
