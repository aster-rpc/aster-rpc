//! A persistent node re-binds the UDP ports it last bound, when they are free.
//!
//! Peers cache a restarted node's direct addresses, and a NAT keeps the
//! mapping for the port it last saw. A node that comes back on the same port
//! keeps both valid: in the 2026-10-15 restart investigation, Docs recovery
//! dropped to 0.3-1.7 s, and a NAT'd node with relays off went from never
//! reachable to reachable in about 32 s. A node that comes back on a fresh
//! port invalidates both.
//!
//! The rule:
//!
//! - Only a persistent node (one with a data directory) records ports, and
//!   only when its bind address leaves the port to the OS (no `bind_addr`, or
//!   one with port 0). An explicit non-zero port is used as given and never
//!   recorded. [`CoreEndpointConfig::reuse_last_port`] turns it off.
//! - After a node starts, the bound IPv4 and IPv6 ports are written to
//!   [`BIND_PORTS_FILE`] in the data directory, atomically.
//! - On the next start each recorded port is probed first. A port that is
//!   taken, or a record that is unreadable, falls back to a fresh port for
//!   that address family, as before. The recorded port never fails a start:
//!   if the bind itself still fails (a race after the probe), the node binds
//!   fresh ports for every family.
//!
//! Iroh's `BindOpts::set_is_required(false)` is not a fallback on its own: a
//! user-defined socket replaces iroh's default socket for its family whether
//! or not it binds, so a busy port would leave that family with no socket.
//! Hence the probe, and the full retry with OS-chosen ports.
//!
//! Two processes on one data directory are not supported. The second never
//! takes the first one's port (the probe fails, so it binds a fresh one), and
//! it records nothing until it holds the stores, which the first one locks.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::CoreEndpointConfig;

/// File in a persistent node's data directory that holds its last bound ports.
pub const BIND_PORTS_FILE: &str = "bind-ports.json";

const SCHEMA: &str = "aster.bind-ports";
const VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Record {
    schema: String,
    version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    v4: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    v6: Option<u16>,
}

/// Ports one bind attempt asks for, per address family. `None` keeps the
/// configured (OS-chosen) port.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PreferredPorts {
    pub(crate) v4: Option<u16>,
    pub(crate) v6: Option<u16>,
}

impl PreferredPorts {
    pub(crate) fn is_empty(&self) -> bool {
        self.v4.is_none() && self.v6.is_none()
    }
}

/// Port reuse for one persistent node start.
#[derive(Debug)]
pub(crate) struct PortReuse {
    file: PathBuf,
    /// The configured bind address (port 0), if any.
    configured: Option<SocketAddr>,
    /// Whether an IP socket of each family is bound at all.
    v4: bool,
    v6: bool,
}

impl PortReuse {
    /// Port reuse for a node with this data directory and endpoint config
    /// (`None` = defaults), or `None` when it does not apply.
    pub(crate) fn for_node(config: Option<&CoreEndpointConfig>, data_dir: &Path) -> Option<Self> {
        let (reuse, bind_addr, clear_ip) = match config {
            Some(c) => (
                c.reuse_last_port,
                c.bind_addr.as_deref(),
                c.clear_ip_transports,
            ),
            None => (true, None, false),
        };
        if !reuse {
            return None;
        }
        let configured = match bind_addr {
            None => None,
            // An unparsable address is left for the endpoint builder to reject.
            Some(addr) => match addr.parse::<SocketAddr>() {
                Ok(addr) if addr.port() == 0 => Some(addr),
                _ => return None,
            },
        };
        // `bind_addr` replaces the default socket of its own family only;
        // `clear_ip_transports` removes the defaults.
        let configured_v4 = configured.is_some_and(|a| a.is_ipv4());
        let configured_v6 = configured.is_some_and(|a| a.is_ipv6());
        let v4 = configured_v4 || !clear_ip;
        let v6 = configured_v6 || !clear_ip;
        if !v4 && !v6 {
            return None;
        }
        Some(Self {
            file: data_dir.join(BIND_PORTS_FILE),
            configured,
            v4,
            v6,
        })
    }

    fn v4_ip(&self) -> Ipv4Addr {
        match self.configured {
            Some(SocketAddr::V4(a)) => *a.ip(),
            _ => Ipv4Addr::UNSPECIFIED,
        }
    }

    fn v6_ip(&self) -> Ipv6Addr {
        match self.configured {
            Some(SocketAddr::V6(a)) => *a.ip(),
            _ => Ipv6Addr::UNSPECIFIED,
        }
    }

    /// The recorded ports that are free to bind right now.
    pub(crate) fn preferred(&self) -> PreferredPorts {
        let Some(record) = self.read() else {
            return PreferredPorts::default();
        };
        let v4 = record
            .v4
            .filter(|_| self.v4)
            .filter(|&port| self.free(SocketAddr::new(self.v4_ip().into(), port)));
        let v6 = record
            .v6
            .filter(|_| self.v6)
            .filter(|&port| self.free(SocketAddr::new(self.v6_ip().into(), port)));
        PreferredPorts { v4, v6 }
    }

    fn read(&self) -> Option<Record> {
        let bytes = match std::fs::read(&self.file) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => {
                warn!(file = %self.file.display(), "ignoring unreadable bind-port record: {e}");
                return None;
            }
        };
        match serde_json::from_slice::<Record>(&bytes) {
            Ok(r) if r.schema == SCHEMA && r.version == VERSION => Some(Record {
                // Port 0 is not a recorded port.
                v4: r.v4.filter(|&p| p != 0),
                v6: r.v6.filter(|&p| p != 0),
                ..r
            }),
            Ok(r) => {
                warn!(
                    file = %self.file.display(),
                    schema = %r.schema,
                    version = r.version,
                    "ignoring bind-port record with an unknown schema or version"
                );
                None
            }
            Err(e) => {
                warn!(file = %self.file.display(), "ignoring corrupt bind-port record: {e}");
                None
            }
        }
    }

    /// Whether `addr` can be bound now. The probe socket is closed before
    /// the endpoint binds.
    fn free(&self, addr: SocketAddr) -> bool {
        match probe(addr) {
            Ok(()) => true,
            Err(e) => {
                info!(%addr, "last bound port is not available, binding a fresh one: {e}");
                false
            }
        }
    }

    /// Record the ports the node is bound to. Failure is logged, never fatal.
    pub(crate) fn record(&self, bound: &[SocketAddr]) {
        let port = |v4: bool| {
            bound
                .iter()
                .find(|a| a.is_ipv4() == v4 && a.port() != 0)
                .map(SocketAddr::port)
        };
        let record = Record {
            schema: SCHEMA.into(),
            version: VERSION,
            v4: port(true).filter(|_| self.v4),
            v6: port(false).filter(|_| self.v6),
        };
        if record.v4.is_none() && record.v6.is_none() {
            return;
        }
        if self.read().as_ref() == Some(&record) {
            return;
        }
        match write_atomic(&self.file, &record) {
            Ok(()) => {
                debug!(file = %self.file.display(), v4 = ?record.v4, v6 = ?record.v6, "recorded bound ports")
            }
            Err(e) => warn!(file = %self.file.display(), "could not record bound ports: {e}"),
        }
    }
}

/// Bind a UDP socket the way the endpoint will (no address reuse; IPv6 only
/// for IPv6), then close it.
fn probe(addr: SocketAddr) -> std::io::Result<()> {
    let socket = socket2::Socket::new(
        socket2::Domain::for_address(addr),
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    )?;
    if addr.is_ipv6() {
        socket.set_only_v6(true)?;
    }
    socket.bind(&addr.into())
}

fn write_atomic(file: &Path, record: &Record) -> std::io::Result<()> {
    let tmp = file.with_extension(format!("json.tmp-{}", std::process::id()));
    let mut bytes = serde_json::to_vec_pretty(record).map_err(std::io::Error::other)?;
    bytes.push(b'\n');
    let result = (|| {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, file)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reuse(dir: &Path, config: &CoreEndpointConfig) -> PortReuse {
        PortReuse::for_node(Some(config), dir).expect("port reuse applies")
    }

    fn tempdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "aster-bind-ports-{}-{}",
            std::process::id(),
            rand_suffix()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn rand_suffix() -> u64 {
        let mut b = [0u8; 8];
        getrandom::getrandom(&mut b).unwrap();
        u64::from_le_bytes(b)
    }

    #[test]
    fn applies_only_to_wildcard_ports() {
        let dir = Path::new("/nonexistent");
        let with = |bind: Option<&str>, clear_ip: bool, reuse: bool| {
            let config = CoreEndpointConfig {
                bind_addr: bind.map(Into::into),
                clear_ip_transports: clear_ip,
                reuse_last_port: reuse,
                ..Default::default()
            };
            PortReuse::for_node(Some(&config), dir).map(|r| (r.v4, r.v6))
        };
        assert_eq!(with(None, false, true), Some((true, true)));
        assert_eq!(with(Some("127.0.0.1:0"), false, true), Some((true, true)));
        assert_eq!(with(Some("127.0.0.1:0"), true, true), Some((true, false)));
        assert_eq!(with(Some("[::1]:0"), true, true), Some((false, true)));
        assert_eq!(with(Some("127.0.0.1:4567"), false, true), None);
        assert_eq!(with(Some("not an address"), false, true), None);
        assert_eq!(with(None, true, true), None);
        assert_eq!(with(None, false, false), None);
        assert!(PortReuse::for_node(None, dir).is_some());
    }

    #[test]
    fn records_and_reads_back_free_ports() {
        let dir = tempdir();
        let config = CoreEndpointConfig {
            bind_addr: Some("127.0.0.1:0".into()),
            clear_ip_transports: true,
            ..Default::default()
        };
        let r = reuse(&dir, &config);
        assert_eq!(r.preferred(), PreferredPorts::default());
        let held = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = held.local_addr().unwrap().port();
        r.record(&[held.local_addr().unwrap()]);
        // Taken: falls back.
        assert_eq!(r.preferred(), PreferredPorts::default());
        drop(held);
        assert_eq!(
            r.preferred(),
            PreferredPorts {
                v4: Some(port),
                v6: None
            }
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_or_old_records_are_ignored() {
        let dir = tempdir();
        let config = CoreEndpointConfig {
            bind_addr: Some("127.0.0.1:0".into()),
            clear_ip_transports: true,
            ..Default::default()
        };
        let r = reuse(&dir, &config);
        let file = dir.join(BIND_PORTS_FILE);
        for body in [
            "not json".to_string(),
            r#"{"schema":"aster.bind-ports","version":0,"v4":40000}"#.to_string(),
            r#"{"schema":"other","version":1,"v4":40000}"#.to_string(),
            r#"{"schema":"aster.bind-ports","version":1,"v4":0}"#.to_string(),
        ] {
            std::fs::write(&file, body).unwrap();
            assert_eq!(r.preferred(), PreferredPorts::default());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
