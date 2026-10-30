//! On-disk configuration: identity key, server address, and the networks to
//! rejoin automatically at startup.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use hamradnet_proto::DEFAULT_CONTROL_PORT;
use serde::{Deserialize, Serialize};

use crate::session::NOISE_PARAMS;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedNetwork {
    pub network_id: String,
    /// Kept so the daemon can rejoin unattended after a restart. The config file
    /// is owner-only for this reason.
    pub password: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Coordinator address, `host:port`.
    pub server: String,
    /// Name other members see in their peer list.
    pub name: String,
    #[serde(with = "b64")]
    pub private_key: [u8; 32],
    #[serde(default, deserialize_with = "null_as_empty")]
    pub networks: Vec<SavedNetwork>,
}

/// The Go client shares this file and once wrote an empty list as `null`.
/// `default` alone does not cover that: it only applies when the field is
/// missing, and a Go-written config would then stop this client from starting.
fn null_as_empty<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<SavedNetwork>, D::Error> {
    Ok(Option::<Vec<SavedNetwork>>::deserialize(d)?.unwrap_or_default())
}

impl Config {
    pub fn generate(server: String, name: String) -> Result<Self> {
        let keypair = snow::Builder::new(NOISE_PARAMS.parse()?)
            .generate_keypair()
            .context("generating an identity key")?;
        let private_key = keypair
            .private
            .try_into()
            .map_err(|_| anyhow::anyhow!("unexpected private key length"))?;
        Ok(Self { server, name, private_key, networks: Vec::new() })
    }

    pub fn public_key(&self) -> Result<[u8; 32]> {
        crate::session::public_from_private(&self.private_key)
    }

    pub fn load(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path)
            .with_context(|| format!("reading config at {}", path.display()))?;
        serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing config at {}", path.display()))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let bytes = serde_json::to_vec_pretty(self)?;
        std::fs::write(path, bytes)
            .with_context(|| format!("writing config to {}", path.display()))?;
        restrict_permissions(path)?;
        Ok(())
    }

    pub fn remember_network(&mut self, network_id: &str, password: &str) {
        if let Some(existing) =
            self.networks.iter_mut().find(|n| n.network_id == network_id)
        {
            existing.password = password.to_string();
        } else {
            self.networks.push(SavedNetwork {
                network_id: network_id.to_string(),
                password: password.to_string(),
            });
        }
    }

    pub fn forget_network(&mut self, network_id: &str) {
        self.networks.retain(|n| n.network_id != network_id);
    }
}

/// The config holds a private key and network passwords, so keep it owner-only.
#[cfg(unix)]
fn restrict_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("restricting permissions on {}", path.display()))
}

/// On Windows the file lives under ProgramData, whose default ACL already keeps
/// non-administrators out of anything we create there.
#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

/// Tidies a server address as typed, adding the default port if there is none,
/// since a bare IP is what people have to hand. Matches the Go client's
/// `NormalizeServer`.
pub fn normalize_server(server: &str) -> String {
    let server = server.trim();
    if server.is_empty() || server.parse::<std::net::SocketAddr>().is_ok() {
        return server.to_string();
    }
    // A name with a port, such as vpn.example.com:7777.
    if let Some((host, port)) = server.rsplit_once(':') {
        if !host.contains(':') && port.parse::<u16>().is_ok() {
            return server.to_string();
        }
    }
    let host = server.trim_start_matches('[').trim_end_matches(']');
    if host.contains(':') {
        format!("[{host}]:{DEFAULT_CONTROL_PORT}")
    } else {
        format!("{host}:{DEFAULT_CONTROL_PORT}")
    }
}

/// `$HAMRADNET_CONFIG` wins; otherwise a system-wide location, since the daemon
/// needs elevated privileges to create the tunnel interface anyway.
pub fn default_config_path() -> PathBuf {
    if let Ok(custom) = std::env::var("HAMRADNET_CONFIG") {
        return PathBuf::from(custom);
    }
    config_dir().join("config.json")
}

/// Where the daemon advertises its local control port for the CLI.
pub fn ipc_path() -> PathBuf {
    if let Ok(custom) = std::env::var("HAMRADNET_IPC") {
        return PathBuf::from(custom);
    }
    config_dir().join("ipc.json")
}

#[cfg(unix)]
fn config_dir() -> PathBuf {
    PathBuf::from("/etc/hamradnet")
}

#[cfg(windows)]
fn config_dir() -> PathBuf {
    let base = std::env::var("ProgramData")
        .unwrap_or_else(|_| r"C:\ProgramData".to_string());
    PathBuf::from(base).join("HamRadNet")
}

mod b64 {
    use base64::{engine::general_purpose::STANDARD, Engine};
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(key: &[u8; 32], s: S) -> Result<S::Ok, S::Error> {
        STANDARD.encode(key).serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 32], D::Error> {
        let text = String::deserialize(d)?;
        STANDARD
            .decode(text.as_bytes())
            .map_err(serde::de::Error::custom)?
            .try_into()
            .map_err(|_| serde::de::Error::custom("expected a 32-byte key"))
    }
}

/// Renders a public key the way it is shown to users.
pub fn encode_key(key: &[u8; 32]) -> String {
    STANDARD.encode(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_round_trips_through_disk() {
        let dir = std::env::temp_dir().join(format!("hamradnet-cfg-{}", std::process::id()));
        let path = dir.join("config.json");
        let mut config = Config::generate("vpn.example.com:7777".into(), "konako".into()).unwrap();
        config.remember_network("HAM-1111-2222", "pw");
        config.save(&path).unwrap();

        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded.private_key, config.private_key);
        assert_eq!(loaded.server, "vpn.example.com:7777");
        assert_eq!(loaded.networks.len(), 1);
        assert_eq!(loaded.networks[0].network_id, "HAM-1111-2222");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn remembering_the_same_network_twice_updates_rather_than_duplicates() {
        let mut config = Config::generate("s:1".into(), "n".into()).unwrap();
        config.remember_network("HAM-1", "old");
        config.remember_network("HAM-1", "new");
        assert_eq!(config.networks.len(), 1);
        assert_eq!(config.networks[0].password, "new");
        config.forget_network("HAM-1");
        assert!(config.networks.is_empty());
    }

    #[test]
    fn a_config_written_by_the_go_client_loads() {
        let key = encode_key(&[7u8; 32]);
        for networks in [r#","networks":null"#, r#","networks":[]"#, ""] {
            let text = format!(r#"{{"server":"s:1","name":"n","private_key":"{key}"{networks}}}"#);
            let config: Config = serde_json::from_str(&text)
                .unwrap_or_else(|err| panic!("{text} did not load: {err}"));
            assert!(config.networks.is_empty());
        }
    }

    #[test]
    fn server_addresses_get_the_default_port() {
        for (typed, want) in [
            ("203.0.113.45", "203.0.113.45:7777"),
            (" 203.0.113.45 ", "203.0.113.45:7777"),
            ("203.0.113.45:9000", "203.0.113.45:9000"),
            ("vpn.example.com", "vpn.example.com:7777"),
            ("vpn.example.com:7777", "vpn.example.com:7777"),
            ("2001:db8::1", "[2001:db8::1]:7777"),
            ("[2001:db8::1]", "[2001:db8::1]:7777"),
            ("[2001:db8::1]:7777", "[2001:db8::1]:7777"),
            ("", ""),
        ] {
            assert_eq!(normalize_server(typed), want, "for {typed:?}");
        }
    }

    #[test]
    fn generated_keys_are_distinct() {
        let a = Config::generate("s:1".into(), "n".into()).unwrap();
        let b = Config::generate("s:1".into(), "n".into()).unwrap();
        assert_ne!(a.private_key, b.private_key);
    }
}
