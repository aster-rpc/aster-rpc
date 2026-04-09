//! Compact Aster ticket format — binary encoding for endpoint addresses + credentials.
//!
//! Wire format (max 256 bytes):
//! ```text
//! [32 bytes endpoint_id]
//! [1 byte header: version(4b) | relay_type(2b) | reserved(2b)]
//! [0/6/18 bytes relay addr+port]
//! [1 byte: direct_count(4b) | reserved(4b)]
//! [ceil(count*2/8) bytes type bitfield — 2 bits per addr: 00=ipv4, 01=ipv6]
//! [addresses: 6 bytes (ipv4+port) or 18 bytes (ipv6+port) each]
//! [optional credential TLV: 1 byte type + 2 bytes length BE + payload]
//! ```
//!
//! String format: `aster1<base58>`

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};

use anyhow::{anyhow, bail, ensure, Result};

/// Maximum total wire size.
const MAX_WIRE_SIZE: usize = 256;
/// Maximum credential payload size.
const MAX_CREDENTIAL_PAYLOAD: usize = 220;
/// Format version.
const FORMAT_VERSION: u8 = 1;
/// String prefix.
const STRING_PREFIX: &str = "aster1";
/// Maximum direct addresses (4 bits).
const MAX_DIRECT_ADDRS: usize = 15;

/// Credential attached to a ticket.
#[derive(Clone, Debug, PartialEq)]
pub enum TicketCredential {
    /// Open access — no credential payload.
    Open,
    /// Consumer RCAN credential (JSON payload).
    ConsumerRcan(Vec<u8>),
    /// Enrollment credential (JSON payload).
    Enrollment(Vec<u8>),
    /// Registry read capability: the namespace public key (32 bytes).
    /// Knowing the namespace ID grants read access to the doc.
    RegistryRead([u8; 32]),
}

impl TicketCredential {
    fn type_byte(&self) -> u8 {
        match self {
            Self::Open => 0x00,
            Self::ConsumerRcan(_) => 0x01,
            Self::Enrollment(_) => 0x02,
            Self::RegistryRead(_) => 0x03,
        }
    }

    fn payload_bytes(&self) -> Vec<u8> {
        match self {
            Self::Open => vec![],
            Self::ConsumerRcan(v) | Self::Enrollment(v) => v.clone(),
            Self::RegistryRead(ns) => ns.to_vec(),
        }
    }

    fn from_tlv(type_byte: u8, payload: &[u8]) -> Result<Self> {
        match type_byte {
            0x00 => Ok(Self::Open),
            0x01 => Ok(Self::ConsumerRcan(payload.to_vec())),
            0x02 => Ok(Self::Enrollment(payload.to_vec())),
            0x03 => {
                ensure!(
                    payload.len() == 32,
                    "registry_read credential must be 32 bytes"
                );
                let mut ns = [0u8; 32];
                ns.copy_from_slice(payload);
                Ok(Self::RegistryRead(ns))
            }
            _ => bail!("unknown credential type 0x{:02x}", type_byte),
        }
    }
}

/// Compact Aster ticket.
#[derive(Clone, Debug, PartialEq)]
pub struct AsterTicket {
    pub endpoint_id: [u8; 32],
    pub relay: Option<SocketAddr>,
    pub direct_addrs: Vec<SocketAddr>,
    pub credential: Option<TicketCredential>,
}

impl AsterTicket {
    /// Serialize to compact binary wire format.
    pub fn encode(&self) -> Result<Vec<u8>> {
        ensure!(
            self.direct_addrs.len() <= MAX_DIRECT_ADDRS,
            "too many direct addresses (max {})",
            MAX_DIRECT_ADDRS
        );

        let mut buf = Vec::with_capacity(128);

        // endpoint_id: 32 bytes
        buf.extend_from_slice(&self.endpoint_id);

        // header byte: version(4b) | relay_type(2b) | reserved(2b)
        let relay_type: u8 = match &self.relay {
            None => 0b00,
            Some(SocketAddr::V4(_)) => 0b01,
            Some(SocketAddr::V6(_)) => 0b10,
        };
        let header = (FORMAT_VERSION << 4) | (relay_type << 2);
        buf.push(header);

        // relay address
        match &self.relay {
            None => {}
            Some(SocketAddr::V4(v4)) => {
                buf.extend_from_slice(&v4.ip().octets());
                buf.extend_from_slice(&v4.port().to_be_bytes());
            }
            Some(SocketAddr::V6(v6)) => {
                buf.extend_from_slice(&v6.ip().octets());
                buf.extend_from_slice(&v6.port().to_be_bytes());
            }
        }

        // direct addresses
        let count = self.direct_addrs.len() as u8;
        // count byte: count(4b) | reserved(4b)
        buf.push(count << 4);

        // type bitfield: 2 bits per address
        if count > 0 {
            let bitfield_bytes = ((count as usize) * 2).div_ceil(8);
            let bitfield_start = buf.len();
            buf.resize(buf.len() + bitfield_bytes, 0);

            for (i, addr) in self.direct_addrs.iter().enumerate() {
                let bits: u8 = match addr {
                    SocketAddr::V4(_) => 0b00,
                    SocketAddr::V6(_) => 0b01,
                };
                let byte_idx = (i * 2) / 8;
                let bit_offset = (i * 2) % 8;
                buf[bitfield_start + byte_idx] |= bits << (6 - bit_offset);
            }

            // address data
            for addr in &self.direct_addrs {
                match addr {
                    SocketAddr::V4(v4) => {
                        buf.extend_from_slice(&v4.ip().octets());
                        buf.extend_from_slice(&v4.port().to_be_bytes());
                    }
                    SocketAddr::V6(v6) => {
                        buf.extend_from_slice(&v6.ip().octets());
                        buf.extend_from_slice(&v6.port().to_be_bytes());
                    }
                }
            }
        }

        // optional credential TLV
        if let Some(cred) = &self.credential {
            let payload = cred.payload_bytes();
            ensure!(
                payload.len() <= MAX_CREDENTIAL_PAYLOAD,
                "credential payload too large ({} > {})",
                payload.len(),
                MAX_CREDENTIAL_PAYLOAD
            );
            buf.push(cred.type_byte());
            buf.extend_from_slice(&(payload.len() as u16).to_be_bytes());
            buf.extend_from_slice(&payload);
        }

        ensure!(
            buf.len() <= MAX_WIRE_SIZE,
            "encoded ticket too large ({} > {} bytes)",
            buf.len(),
            MAX_WIRE_SIZE
        );

        Ok(buf)
    }

    /// Deserialize from compact binary wire format.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        ensure!(
            bytes.len() <= MAX_WIRE_SIZE,
            "ticket too large ({} > {} bytes)",
            bytes.len(),
            MAX_WIRE_SIZE
        );
        ensure!(bytes.len() >= 34, "ticket too short");

        let mut pos = 0;

        // endpoint_id
        let mut endpoint_id = [0u8; 32];
        endpoint_id.copy_from_slice(&bytes[pos..pos + 32]);
        pos += 32;

        // header
        let header = bytes[pos];
        pos += 1;
        let version = header >> 4;
        ensure!(
            version == FORMAT_VERSION,
            "unsupported ticket version {}",
            version
        );
        let relay_type = (header >> 2) & 0b11;

        // relay
        let relay = match relay_type {
            0b00 => None,
            0b01 => {
                ensure!(pos + 6 <= bytes.len(), "truncated IPv4 relay");
                let ip = Ipv4Addr::new(bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]);
                let port = u16::from_be_bytes([bytes[pos + 4], bytes[pos + 5]]);
                pos += 6;
                Some(SocketAddr::V4(SocketAddrV4::new(ip, port)))
            }
            0b10 => {
                ensure!(pos + 18 <= bytes.len(), "truncated IPv6 relay");
                let mut octets = [0u8; 16];
                octets.copy_from_slice(&bytes[pos..pos + 16]);
                let ip = Ipv6Addr::from(octets);
                let port = u16::from_be_bytes([bytes[pos + 16], bytes[pos + 17]]);
                pos += 18;
                Some(SocketAddr::V6(SocketAddrV6::new(ip, port, 0, 0)))
            }
            _ => bail!("reserved relay type {}", relay_type),
        };

        // direct addresses
        ensure!(pos < bytes.len(), "truncated: missing direct addr count");
        let count_byte = bytes[pos];
        pos += 1;
        let count = (count_byte >> 4) as usize;

        let mut direct_addrs = Vec::with_capacity(count);
        if count > 0 {
            let bitfield_bytes = (count * 2).div_ceil(8);
            ensure!(
                pos + bitfield_bytes <= bytes.len(),
                "truncated type bitfield"
            );
            let bitfield = &bytes[pos..pos + bitfield_bytes];
            pos += bitfield_bytes;

            for i in 0..count {
                let byte_idx = (i * 2) / 8;
                let bit_offset = (i * 2) % 8;
                let addr_type = (bitfield[byte_idx] >> (6 - bit_offset)) & 0b11;

                match addr_type {
                    0b00 => {
                        ensure!(pos + 6 <= bytes.len(), "truncated IPv4 direct addr");
                        let ip = Ipv4Addr::new(
                            bytes[pos],
                            bytes[pos + 1],
                            bytes[pos + 2],
                            bytes[pos + 3],
                        );
                        let port = u16::from_be_bytes([bytes[pos + 4], bytes[pos + 5]]);
                        pos += 6;
                        direct_addrs.push(SocketAddr::V4(SocketAddrV4::new(ip, port)));
                    }
                    0b01 => {
                        ensure!(pos + 18 <= bytes.len(), "truncated IPv6 direct addr");
                        let mut octets = [0u8; 16];
                        octets.copy_from_slice(&bytes[pos..pos + 16]);
                        let ip = Ipv6Addr::from(octets);
                        let port = u16::from_be_bytes([bytes[pos + 16], bytes[pos + 17]]);
                        pos += 18;
                        direct_addrs.push(SocketAddr::V6(SocketAddrV6::new(ip, port, 0, 0)));
                    }
                    _ => bail!("reserved address type {}", addr_type),
                }
            }
        }

        // optional credential TLV
        let credential = if pos < bytes.len() {
            ensure!(pos + 3 <= bytes.len(), "truncated credential TLV header");
            let cred_type = bytes[pos];
            pos += 1;
            let payload_len = u16::from_be_bytes([bytes[pos], bytes[pos + 1]]) as usize;
            pos += 2;
            ensure!(
                payload_len <= MAX_CREDENTIAL_PAYLOAD,
                "credential payload too large"
            );
            ensure!(
                pos + payload_len <= bytes.len(),
                "truncated credential payload"
            );
            let payload = &bytes[pos..pos + payload_len];
            pos += payload_len;
            Some(TicketCredential::from_tlv(cred_type, payload)?)
        } else {
            None
        };

        ensure!(
            pos == bytes.len(),
            "trailing bytes after ticket ({} extra)",
            bytes.len() - pos
        );

        Ok(Self {
            endpoint_id,
            relay,
            direct_addrs,
            credential,
        })
    }

    /// Encode to `aster1<base58>` string.
    pub fn to_base58_string(&self) -> Result<String> {
        let wire = self.encode()?;
        let encoded = bs58::encode(&wire).into_string();
        Ok(format!("{}{}", STRING_PREFIX, encoded))
    }

    /// Parse from `aster1<base58>` string.
    pub fn from_base58_str(s: &str) -> Result<Self> {
        let payload = s
            .strip_prefix(STRING_PREFIX)
            .ok_or_else(|| anyhow!("ticket must start with '{}'", STRING_PREFIX))?;
        let wire = bs58::decode(payload)
            .into_vec()
            .map_err(|e| anyhow!("invalid base58: {}", e))?;
        Self::decode(&wire)
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_endpoint_id() -> [u8; 32] {
        let mut id = [0u8; 32];
        for (i, b) in id.iter_mut().enumerate() {
            *b = i as u8;
        }
        id
    }

    #[test]
    fn roundtrip_minimal() {
        let ticket = AsterTicket {
            endpoint_id: sample_endpoint_id(),
            relay: None,
            direct_addrs: vec![],
            credential: None,
        };
        let wire = ticket.encode().unwrap();
        assert_eq!(wire.len(), 34); // 32 + 1 header + 1 count
        let decoded = AsterTicket::decode(&wire).unwrap();
        assert_eq!(ticket, decoded);
    }

    #[test]
    fn roundtrip_ipv4_relay() {
        let ticket = AsterTicket {
            endpoint_id: sample_endpoint_id(),
            relay: Some("87.254.0.136:443".parse().unwrap()),
            direct_addrs: vec![],
            credential: None,
        };
        let wire = ticket.encode().unwrap();
        assert_eq!(wire.len(), 40); // 32 + 1 + 6 + 1
        let decoded = AsterTicket::decode(&wire).unwrap();
        assert_eq!(ticket, decoded);
    }

    #[test]
    fn roundtrip_with_direct_addrs() {
        let ticket = AsterTicket {
            endpoint_id: sample_endpoint_id(),
            relay: Some("87.254.0.136:443".parse().unwrap()),
            direct_addrs: vec![
                "192.168.1.179:53332".parse().unwrap(),
                "10.0.0.1:9000".parse().unwrap(),
            ],
            credential: None,
        };
        let wire = ticket.encode().unwrap();
        // 32 + 1 + 6 + 1 + 1(bitfield) + 12 = 53
        assert_eq!(wire.len(), 53);
        let decoded = AsterTicket::decode(&wire).unwrap();
        assert_eq!(ticket, decoded);
    }

    #[test]
    fn roundtrip_ipv6_direct() {
        let ticket = AsterTicket {
            endpoint_id: sample_endpoint_id(),
            relay: Some("87.254.0.136:443".parse().unwrap()),
            direct_addrs: vec!["[::1]:9000".parse().unwrap()],
            credential: None,
        };
        let wire = ticket.encode().unwrap();
        let decoded = AsterTicket::decode(&wire).unwrap();
        assert_eq!(ticket, decoded);
    }

    #[test]
    fn roundtrip_open_credential() {
        let ticket = AsterTicket {
            endpoint_id: sample_endpoint_id(),
            relay: Some("87.254.0.136:443".parse().unwrap()),
            direct_addrs: vec![],
            credential: Some(TicketCredential::Open),
        };
        let wire = ticket.encode().unwrap();
        let decoded = AsterTicket::decode(&wire).unwrap();
        assert_eq!(ticket, decoded);
    }

    #[test]
    fn roundtrip_consumer_rcan() {
        let payload = b"{\"sig\":\"abc\",\"exp\":12345}".to_vec();
        let ticket = AsterTicket {
            endpoint_id: sample_endpoint_id(),
            relay: Some("87.254.0.136:443".parse().unwrap()),
            direct_addrs: vec!["192.168.1.1:8080".parse().unwrap()],
            credential: Some(TicketCredential::ConsumerRcan(payload)),
        };
        let wire = ticket.encode().unwrap();
        assert!(wire.len() <= MAX_WIRE_SIZE);
        let decoded = AsterTicket::decode(&wire).unwrap();
        assert_eq!(ticket, decoded);
    }

    #[test]
    fn roundtrip_registry_read() {
        let mut ns = [0u8; 32];
        ns[0] = 0xAA;
        let ticket = AsterTicket {
            endpoint_id: sample_endpoint_id(),
            relay: None,
            direct_addrs: vec![],
            credential: Some(TicketCredential::RegistryRead(ns)),
        };
        let wire = ticket.encode().unwrap();
        let decoded = AsterTicket::decode(&wire).unwrap();
        assert_eq!(ticket, decoded);
    }

    #[test]
    fn roundtrip_base58_string() {
        let ticket = AsterTicket {
            endpoint_id: sample_endpoint_id(),
            relay: Some("87.254.0.136:443".parse().unwrap()),
            direct_addrs: vec!["192.168.1.179:53332".parse().unwrap()],
            credential: Some(TicketCredential::Open),
        };
        let s = ticket.to_base58_string().unwrap();
        assert!(s.starts_with("aster1"));
        let decoded = AsterTicket::from_base58_str(&s).unwrap();
        assert_eq!(ticket, decoded);
    }

    #[test]
    fn rejects_too_large() {
        let ticket = AsterTicket {
            endpoint_id: sample_endpoint_id(),
            relay: Some("87.254.0.136:443".parse().unwrap()),
            direct_addrs: vec![],
            credential: Some(TicketCredential::ConsumerRcan(vec![0xAA; 221])),
        };
        assert!(ticket.encode().is_err());
    }

    #[test]
    fn rejects_bad_prefix() {
        assert!(AsterTicket::from_base58_str("foobar123").is_err());
    }

    #[test]
    fn rejects_truncated() {
        let ticket = AsterTicket {
            endpoint_id: sample_endpoint_id(),
            relay: Some("87.254.0.136:443".parse().unwrap()),
            direct_addrs: vec!["192.168.1.1:80".parse().unwrap()],
            credential: None,
        };
        let wire = ticket.encode().unwrap();
        assert!(AsterTicket::decode(&wire[..wire.len() - 1]).is_err());
    }

    #[test]
    fn rejects_trailing_bytes() {
        let ticket = AsterTicket {
            endpoint_id: sample_endpoint_id(),
            relay: None,
            direct_addrs: vec![],
            credential: None,
        };
        let mut wire = ticket.encode().unwrap();
        wire.push(0xFF);
        assert!(AsterTicket::decode(&wire).is_err());
    }

    #[test]
    fn ipv6_relay_roundtrip() {
        let ticket = AsterTicket {
            endpoint_id: sample_endpoint_id(),
            relay: Some("[2001:db8::1]:443".parse().unwrap()),
            direct_addrs: vec![],
            credential: None,
        };
        let wire = ticket.encode().unwrap();
        assert_eq!(wire.len(), 52); // 32 + 1 + 18 + 1
        let decoded = AsterTicket::decode(&wire).unwrap();
        assert_eq!(ticket, decoded);
    }

    #[test]
    fn mixed_addr_types() {
        let ticket = AsterTicket {
            endpoint_id: sample_endpoint_id(),
            relay: Some("10.0.0.1:443".parse().unwrap()),
            direct_addrs: vec![
                "192.168.1.1:8080".parse().unwrap(),
                "[::1]:9000".parse().unwrap(),
                "10.0.0.2:7000".parse().unwrap(),
            ],
            credential: None,
        };
        let wire = ticket.encode().unwrap();
        let decoded = AsterTicket::decode(&wire).unwrap();
        assert_eq!(ticket, decoded);
    }
}
