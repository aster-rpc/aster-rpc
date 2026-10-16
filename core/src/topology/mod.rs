//! Topology v1: local per-peer proximity view.
//!
//! Derives a locality-ladder level, path quality (smoothed RTT, jitter, loss,
//! throughput) and a confidence score for every peer the node holds (or
//! recently held) live connections to. Fed by [`crate::CoreMonitor`]'s remote
//! map via a periodic sampler; no probe traffic, no shared state — this is
//! the v1 "read what iroh already knows" phase of
//! `docs/_internal/aster-network-topology.md`.
//!
//! v2 (shared doc + clusters) lives in the submodules: [`records`] (wire
//! records + key layout), [`cluster`] (pure derivation), [`shared`] (the
//! swarm engine).

pub mod cluster;
pub mod locality;
pub mod records;
pub mod shared;

use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant, SystemTime};

use iroh::TransportAddr;

/// How often the sampler folds connection stats into per-peer state.
pub(crate) const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
/// A ladder-level change must be observed this many consecutive samples
/// before it takes effect (first assignment is immediate).
const LEVEL_HOLD_SAMPLES: u32 = 30;
/// Same-region RTT band (design-doc defaults): enter below 12 ms, leave
/// only above 18 ms.
const REGION_RTT_ENTER_US: f64 = 12_000.0;
const REGION_RTT_EXIT_US: f64 = 18_000.0;
/// Same-LAN RTT ceiling for a private path: enter below 500 µs, leave only
/// above 1 ms. A private address alone does not mean a LAN: in a cloud VPC
/// every peer, in every availability zone, is reached privately, and
/// same-zone (~0.1–0.5 ms) and cross-zone (~0.5–2 ms) round trips overlap
/// only at the edges. A slow private path (Wi-Fi, a routed VPC) falls to the
/// RTT bands instead.
const LAN_RTT_ENTER_US: f64 = 500.0;
const LAN_RTT_EXIT_US: f64 = 1_000.0;
/// EWMA weight for RTT / jitter / throughput smoothing.
const EWMA_ALPHA: f64 = 0.3;
/// Samples needed for confidence to saturate at its 0.99 cap.
const CONFIDENCE_FULL_SAMPLES: f64 = 30.0;
/// Confidence halves for every this-much staleness past one interval.
const CONFIDENCE_HALF_LIFE: Duration = Duration::from_secs(60);
/// Minimum elapsed time between two samples for a throughput window to be
/// computed — protects the rate estimate from delayed/bunched ticks.
const MIN_RATE_WINDOW: Duration = Duration::from_millis(100);

/// Locality ladder per the topology design doc. Lower = closer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum CoreLadderLevel {
    /// L0 — loopback / same host.
    SameHost = 0,
    /// L1 — a verified private path (the live QUIC connection on a private
    /// address *is* the verified private dial) whose RTT is under the LAN
    /// ceiling. Reachability on a private address alone is not enough.
    SameLan = 1,
    /// L2 — same public egress IP, no private path.
    SameSite = 2,
    /// L3 — RTT under the region threshold.
    SameRegion = 3,
    /// L4 — everything else.
    Far = 4,
}

impl CoreLadderLevel {
    pub fn as_i32(self) -> i32 {
        self as i32
    }

    /// Snake-case name, the `aster.locality.level` metric label value.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SameHost => "same_host",
            Self::SameLan => "same_lan",
            Self::SameSite => "same_site",
            Self::SameRegion => "same_region",
            Self::Far => "far",
        }
    }
}

/// What a connection to a peer carries, by its ALPN. iroh opens one
/// connection per ALPN, so each protocol's QUIC bytes are its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CoreProtocolFamily {
    /// Plain blobs transfers, including docs entry content.
    Blobs,
    /// zstd blobs transfers.
    BlobsZstd,
    /// The docs sync protocol (entries, not their content).
    Docs,
    Gossip,
    /// Aster RPC (`aster/1`), every service on one connection.
    Rpc,
    /// Anything else: admission, enrollment, custom ALPNs.
    Other,
}

impl CoreProtocolFamily {
    pub const ALL: [Self; 6] = [
        Self::Blobs,
        Self::BlobsZstd,
        Self::Docs,
        Self::Gossip,
        Self::Rpc,
        Self::Other,
    ];

    /// Kebab-case name, the `protocol` metric label value.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Blobs => "blobs",
            Self::BlobsZstd => "blobs-zstd",
            Self::Docs => "docs",
            Self::Gossip => "gossip",
            Self::Rpc => "rpc",
            Self::Other => "other",
        }
    }
}

/// QUIC-level bytes on a peer's connections: UDP datagram payload, so QUIC
/// headers, ACKs and retransmits included. Reads somewhat above the
/// application payload.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CoreQuicBytes {
    pub quic_bytes_sent: u64,
    pub quic_bytes_received: u64,
}

impl CoreQuicBytes {
    fn add(&mut self, sent: u64, received: u64) {
        self.quic_bytes_sent += sent;
        self.quic_bytes_received += received;
    }
}

/// A peer's QUIC bytes by protocol, cumulative since this node started and
/// across reconnects. Monotonic: a closed connection's final counts move into
/// the total as it leaves the live set.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CoreQuicTraffic {
    pub blobs: CoreQuicBytes,
    pub blobs_zstd: CoreQuicBytes,
    pub docs: CoreQuicBytes,
    pub gossip: CoreQuicBytes,
    pub rpc: CoreQuicBytes,
    pub other: CoreQuicBytes,
}

impl CoreQuicTraffic {
    /// The bytes of one protocol.
    pub fn get(&self, family: CoreProtocolFamily) -> CoreQuicBytes {
        match family {
            CoreProtocolFamily::Blobs => self.blobs,
            CoreProtocolFamily::BlobsZstd => self.blobs_zstd,
            CoreProtocolFamily::Docs => self.docs,
            CoreProtocolFamily::Gossip => self.gossip,
            CoreProtocolFamily::Rpc => self.rpc,
            CoreProtocolFamily::Other => self.other,
        }
    }

    /// Every protocol with its bytes, in [`CoreProtocolFamily::ALL`] order.
    pub fn iter(&self) -> impl Iterator<Item = (CoreProtocolFamily, CoreQuicBytes)> + '_ {
        CoreProtocolFamily::ALL
            .into_iter()
            .map(|f| (f, self.get(f)))
    }

    /// The sum over every protocol.
    pub fn total(&self) -> CoreQuicBytes {
        let mut sum = CoreQuicBytes::default();
        for (_, b) in self.iter() {
            sum.add(b.quic_bytes_sent, b.quic_bytes_received);
        }
        sum
    }

    pub(crate) fn add(&mut self, family: CoreProtocolFamily, sent: u64, received: u64) {
        let slot = match family {
            CoreProtocolFamily::Blobs => &mut self.blobs,
            CoreProtocolFamily::BlobsZstd => &mut self.blobs_zstd,
            CoreProtocolFamily::Docs => &mut self.docs,
            CoreProtocolFamily::Gossip => &mut self.gossip,
            CoreProtocolFamily::Rpc => &mut self.rpc,
            CoreProtocolFamily::Other => &mut self.other,
        };
        slot.add(sent, received);
    }

    fn add_all(&mut self, other: &Self) {
        for (family, b) in other.iter() {
            self.add(family, b.quic_bytes_sent, b.quic_bytes_received);
        }
    }

    /// Per protocol, `self` minus `base` (never below zero).
    fn since(&self, base: &Self) -> Self {
        let mut out = Self::default();
        for (family, b) in self.iter() {
            let from = base.get(family);
            out.add(
                family,
                b.quic_bytes_sent.saturating_sub(from.quic_bytes_sent),
                b.quic_bytes_received
                    .saturating_sub(from.quic_bytes_received),
            );
        }
        out
    }

    /// Per protocol, the larger of `self` and `other`.
    fn max(&self, other: &Self) -> Self {
        let mut out = Self::default();
        for (family, b) in self.iter() {
            let o = other.get(family);
            out.add(
                family,
                b.quic_bytes_sent.max(o.quic_bytes_sent),
                b.quic_bytes_received.max(o.quic_bytes_received),
            );
        }
        out
    }
}

/// One peer in a [`LevelTrafficLedger`]: the level its bytes are counted
/// under, its traffic when it entered that level, and the latest traffic read.
#[derive(Debug)]
struct LedgerPeer {
    level: CoreLadderLevel,
    base: CoreQuicTraffic,
    last: CoreQuicTraffic,
}

/// QUIC bytes grouped by locality level, each level's total only ever growing.
///
/// A peer's bytes count under the level it had when they were read. When a
/// peer changes level, or leaves the views, the bytes it accumulated under its
/// old level are banked there, and only its later bytes count under the new
/// one. Summing current peer views by level instead would move a peer's whole
/// history between levels, so the old level's total would drop.
#[derive(Debug, Default)]
pub(crate) struct LevelTrafficLedger {
    banked: std::collections::BTreeMap<CoreLadderLevel, CoreQuicTraffic>,
    peers: std::collections::HashMap<String, LedgerPeer>,
}

impl LevelTrafficLedger {
    /// Fold the current views in and return every level's totals.
    pub(crate) fn observe(
        &mut self,
        views: &[CorePeerView],
    ) -> std::collections::BTreeMap<CoreLadderLevel, CoreQuicTraffic> {
        let mut seen = std::collections::HashSet::with_capacity(views.len());
        for view in views {
            seen.insert(view.node_id.as_str());
            match self.peers.get_mut(&view.node_id) {
                None => {
                    self.peers.insert(
                        view.node_id.clone(),
                        LedgerPeer {
                            level: view.level,
                            base: CoreQuicTraffic::default(),
                            last: view.quic_traffic,
                        },
                    );
                }
                Some(peer) => {
                    if peer.level != view.level {
                        self.banked
                            .entry(peer.level)
                            .or_default()
                            .add_all(&peer.last.since(&peer.base));
                        peer.level = view.level;
                        peer.base = peer.last;
                    }
                    peer.last = peer.last.max(&view.quic_traffic);
                }
            }
        }
        let banked = &mut self.banked;
        self.peers.retain(|id, peer| {
            let keep = seen.contains(id.as_str());
            if !keep {
                banked
                    .entry(peer.level)
                    .or_default()
                    .add_all(&peer.last.since(&peer.base));
            }
            keep
        });
        let mut out = self.banked.clone();
        for peer in self.peers.values() {
            out.entry(peer.level)
                .or_default()
                .add_all(&peer.last.since(&peer.base));
        }
        out
    }
}

/// Classification of a direct path's remote IP.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AddrClass {
    Loopback,
    Private,
    Public,
}

/// RFC1918 / ULA / link-local / loopback classification. v4-mapped v6
/// addresses classify as their embedded v4 address.
pub(crate) fn classify_ip(ip: IpAddr) -> AddrClass {
    match ip {
        IpAddr::V4(v4) => {
            if v4.is_loopback() {
                AddrClass::Loopback
            } else if v4.is_private() || v4.is_link_local() {
                AddrClass::Private
            } else {
                AddrClass::Public
            }
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return classify_ip(IpAddr::V4(v4));
            }
            if v6.is_loopback() {
                AddrClass::Loopback
            } else if (v6.segments()[0] & 0xfe00) == 0xfc00 // ULA fc00::/7
                || (v6.segments()[0] & 0xffc0) == 0xfe80
            // link-local fe80::/10
            {
                AddrClass::Private
            } else {
                AddrClass::Public
            }
        }
    }
}

/// What the sampler saw on one tick for one peer — counters summed across
/// every live connection to that peer, path shape from the best selected
/// path (direct preferred, then lowest measured RTT).
#[derive(Debug)]
pub(crate) struct PeerSample {
    /// Fingerprint of the live connection set the counters were summed
    /// over; counter baselines reset when it changes (a connection opened
    /// or closed makes the sums non-comparable).
    pub conn_key: u64,
    /// Remote of the representative selected path.
    pub remote: TransportAddr,
    /// Quinn-smoothed RTT of the representative path. Zero = unmeasured.
    pub rtt: Duration,
    /// Monotonic counters summed across the live connection set.
    pub lost_packets: u64,
    pub congestion_events: u64,
    pub cwnd: u64,
    pub tx_datagrams: u64,
    pub tx_bytes: u64,
    pub rx_bytes: u64,
    /// Local public IPs at sample time (for L2 same-egress detection).
    pub local_public_ips: Vec<IpAddr>,
}

/// Counter snapshot the next sample's deltas are computed against.
#[derive(Debug)]
struct CounterBase {
    conn_key: u64,
    tx_datagrams: u64,
    lost_packets: u64,
    congestion_events: u64,
    total_bytes: u64,
    at: Instant,
}

/// Derived per-peer state. Lives inside the monitor's remote map entry.
#[derive(Debug, Default)]
pub(crate) struct PeerTopoState {
    rtt_us_smooth: f64,
    jitter_us: f64,
    last_rtt_us: Option<f64>,
    loss_ppm: f64,
    throughput_bps: f64,
    cwnd_ceiling_bps: u64,
    congestion_events: u64,
    direct: bool,
    relay_url: Option<String>,
    remote_addr: Option<SocketAddr>,
    level: Option<(CoreLadderLevel, String)>,
    pending_level: Option<(CoreLadderLevel, u32)>,
    samples: u64,
    last_sample: Option<SystemTime>,
    counter_base: Option<CounterBase>,
    /// Publisher-side cluster-edge hysteresis (topology v2): set when
    /// smoothed RTT first drops below the enter threshold, cleared only
    /// above the exit threshold.
    cluster_held_since: Option<SystemTime>,
}

impl PeerTopoState {
    pub(crate) fn apply_sample(&mut self, s: PeerSample) {
        self.samples += 1;
        self.last_sample = Some(SystemTime::now());

        // Path shape.
        match &s.remote {
            TransportAddr::Ip(addr) => {
                self.direct = true;
                self.relay_url = None;
                self.remote_addr = Some(*addr);
            }
            TransportAddr::Relay(url) => {
                self.direct = false;
                self.relay_url = Some(url.to_string());
                self.remote_addr = None;
            }
            _ => {
                self.direct = false;
                self.relay_url = None;
                self.remote_addr = None;
            }
        }

        // RTT + jitter EWMAs (skip unmeasured zero readings).
        if !s.rtt.is_zero() {
            let rtt_us = s.rtt.as_micros() as f64;
            if self.rtt_us_smooth == 0.0 {
                self.rtt_us_smooth = rtt_us;
            } else {
                self.rtt_us_smooth += EWMA_ALPHA * (rtt_us - self.rtt_us_smooth);
            }
            if let Some(prev) = self.last_rtt_us {
                let delta = (rtt_us - prev).abs();
                self.jitter_us += EWMA_ALPHA * (delta - self.jitter_us);
            }
            self.last_rtt_us = Some(rtt_us);

            if s.cwnd > 0 {
                let rtt_secs = rtt_us / 1_000_000.0;
                self.cwnd_ceiling_bps = ((s.cwnd as f64 * 8.0) / rtt_secs) as u64;
            }

            // v2 publisher-owned hold band: start holding only below the
            // enter threshold, stop only above the exit threshold.
            if self.rtt_us_smooth < REGION_RTT_ENTER_US {
                self.cluster_held_since.get_or_insert_with(SystemTime::now);
            } else if self.rtt_us_smooth > REGION_RTT_EXIT_US {
                self.cluster_held_since = None;
            }
        }

        // Windowed counter deltas. Baselines reset when the live connection
        // set changes (the summed counters are only monotonic within one set).
        let now = Instant::now();
        let total_bytes = s.tx_bytes + s.rx_bytes;
        if let Some(base) = &self.counter_base {
            if base.conn_key == s.conn_key {
                let dgrams = s.tx_datagrams.saturating_sub(base.tx_datagrams);
                let lost = s.lost_packets.saturating_sub(base.lost_packets);
                if dgrams > 0 {
                    let window_ppm = (lost as f64 / dgrams as f64) * 1_000_000.0;
                    self.loss_ppm += EWMA_ALPHA * (window_ppm - self.loss_ppm);
                }
                self.congestion_events +=
                    s.congestion_events.saturating_sub(base.congestion_events);

                // Rate over the *actual* elapsed window — a delayed or
                // skipped tick must not overstate throughput.
                let elapsed = now.duration_since(base.at);
                if elapsed >= MIN_RATE_WINDOW {
                    let bytes = total_bytes.saturating_sub(base.total_bytes);
                    let window_bps = (bytes as f64 * 8.0) / elapsed.as_secs_f64();
                    self.throughput_bps += EWMA_ALPHA * (window_bps - self.throughput_bps);
                }
            }
        }
        self.counter_base = Some(CounterBase {
            conn_key: s.conn_key,
            tx_datagrams: s.tx_datagrams,
            lost_packets: s.lost_packets,
            congestion_events: s.congestion_events,
            total_bytes,
            at: now,
        });

        // Ladder level with hold-down. `derive_level` returns `None` when
        // there is no signal at all yet (public/relay path, RTT unmeasured):
        // committing Far in that state would cost a full hold-down period
        // once the first real RTT arrives.
        let prev = self.level.as_ref().map(|(l, _)| *l);
        if let Some((level, reason)) =
            derive_level(&s.remote, &s.local_public_ips, self.rtt_us_smooth, prev)
        {
            self.offer_level(level, reason);
        }
    }

    /// Hold-down: first assignment commits immediately; any change must be
    /// observed [`LEVEL_HOLD_SAMPLES`] consecutive samples first.
    fn offer_level(&mut self, new: CoreLadderLevel, reason: String) {
        match &self.level {
            None => {
                self.level = Some((new, reason));
                self.pending_level = None;
            }
            Some((cur, _)) if *cur == new => {
                self.level = Some((new, reason));
                self.pending_level = None;
            }
            Some(_) => match &mut self.pending_level {
                Some((pending, count)) if *pending == new => {
                    *count += 1;
                    if *count >= LEVEL_HOLD_SAMPLES {
                        self.level = Some((new, reason));
                        self.pending_level = None;
                    }
                }
                _ => self.pending_level = Some((new, 1)),
            },
        }
    }

    pub(crate) fn to_view(&self, node_id: &str, is_connected: bool) -> Option<CorePeerView> {
        let (level, level_reason) = self.level.clone()?;
        let last_sample = self.last_sample?;
        let staleness = SystemTime::now()
            .duration_since(last_sample)
            .unwrap_or_default();

        let mut confidence = (self.samples as f64 / CONFIDENCE_FULL_SAMPLES).min(0.99);
        if staleness > SAMPLE_INTERVAL {
            let half_lives = staleness.as_secs_f64() / CONFIDENCE_HALF_LIFE.as_secs_f64();
            confidence *= 0.5_f64.powf(half_lives);
        }

        Some(CorePeerView {
            node_id: node_id.to_string(),
            level,
            level_reason,
            rtt_us: (self.rtt_us_smooth > 0.0).then_some(self.rtt_us_smooth as u64),
            jitter_us: (self.samples > 1).then_some(self.jitter_us as u64),
            loss_ppm: self.loss_ppm as u32,
            throughput_bps: self.throughput_bps as u64,
            cwnd_ceiling_bps: self.cwnd_ceiling_bps,
            direct: self.direct,
            relay_url: self.relay_url.clone(),
            remote_addr: self.remote_addr,
            congestion_events: self.congestion_events,
            samples: self.samples,
            confidence_ppm: (confidence * 1_000_000.0) as u32,
            last_measured_unix_ms: unix_ms(last_sample),
            cluster_held_since_unix_ms: self.cluster_held_since.map(unix_ms).unwrap_or(0),
            is_connected,
            level_source: locality::CoreLevelSource::Measured,
            declared_region: None,
            declared_zone: None,
            quic_traffic: CoreQuicTraffic::default(),
            blob_transfer: crate::CorePeerBlobStats::default(),
        })
    }
}

fn unix_ms(t: SystemTime) -> u64 {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

/// Pure ladder derivation: highest level the current signals justify.
/// `prev` feeds the L1 and L3 enter/exit hysteresis bands. Returns `None`
/// when no signal exists yet (no RTT measured on a non-loopback path): Far
/// is a conclusion drawn from a measured RTT, not from the absence of one,
/// and a private path is not a LAN until its RTT says so. Committing a level
/// early would cost a full hold-down to correct.
fn derive_level(
    remote: &TransportAddr,
    local_public_ips: &[IpAddr],
    rtt_us_smooth: f64,
    prev: Option<CoreLadderLevel>,
) -> Option<(CoreLadderLevel, String)> {
    if let TransportAddr::Ip(addr) = remote {
        match classify_ip(addr.ip()) {
            AddrClass::Loopback => {
                return Some((CoreLadderLevel::SameHost, "loopback path".to_string()));
            }
            AddrClass::Private => {
                if rtt_us_smooth <= 0.0 {
                    return None;
                }
                let ceiling = if prev == Some(CoreLadderLevel::SameLan) {
                    LAN_RTT_EXIT_US
                } else {
                    LAN_RTT_ENTER_US
                };
                if rtt_us_smooth < ceiling {
                    return Some((
                        CoreLadderLevel::SameLan,
                        "private path, rtt under LAN ceiling".to_string(),
                    ));
                }
            }
            AddrClass::Public => {
                if local_public_ips.contains(&addr.ip()) {
                    return Some((CoreLadderLevel::SameSite, "same public egress".to_string()));
                }
            }
        }
    }

    if rtt_us_smooth > 0.0 {
        let threshold = if prev == Some(CoreLadderLevel::SameRegion) {
            REGION_RTT_EXIT_US
        } else {
            REGION_RTT_ENTER_US
        };
        if rtt_us_smooth < threshold {
            return Some((
                CoreLadderLevel::SameRegion,
                "rtt under region threshold".to_string(),
            ));
        }
        return Some((
            CoreLadderLevel::Far,
            "rtt above region threshold".to_string(),
        ));
    }

    None
}

/// One peer's topology snapshot — the v1 `PeerView` of the design doc.
#[derive(Clone, Debug)]
pub struct CorePeerView {
    pub node_id: String,
    pub level: CoreLadderLevel,
    /// Signal that justified the level (e.g. "private path, rtt under LAN ceiling").
    pub level_reason: String,
    /// Smoothed RTT, microseconds. `None` until first measured.
    pub rtt_us: Option<u64>,
    /// EWMA of |ΔRTT|, microseconds. `None` until two RTT samples exist.
    pub jitter_us: Option<u64>,
    /// Smoothed loss rate, parts per million of sent datagrams.
    pub loss_ppm: u32,
    /// Passively observed goodput (both directions), bits/sec, smoothed.
    pub throughput_bps: u64,
    /// cwnd/RTT ceiling estimate, bits/sec.
    pub cwnd_ceiling_bps: u64,
    /// Selected path is direct (vs relayed).
    pub direct: bool,
    pub relay_url: Option<String>,
    /// Selected path's remote socket address when direct. In-process only —
    /// the `aster.net.Topology` RPC surface deliberately omits it.
    pub remote_addr: Option<SocketAddr>,
    /// Total congestion events observed on sampled paths.
    pub congestion_events: u64,
    pub samples: u64,
    /// Confidence 0..0.99 as parts per million; grows with samples, decays
    /// with staleness.
    pub confidence_ppm: u32,
    pub last_measured_unix_ms: u64,
    /// Since when smoothed RTT has continuously held under the cluster
    /// enter threshold (v2 publisher hysteresis); 0 = not currently held.
    pub cluster_held_since_unix_ms: u64,
    pub is_connected: bool,
    /// Where `level` came from: this node's measurement, or both ends'
    /// verified declarations bounding it (see [`locality::bound_level`]).
    pub level_source: locality::CoreLevelSource,
    /// The peer's verified declared region and zone, when it has them.
    pub declared_region: Option<String>,
    pub declared_zone: Option<String>,
    /// QUIC bytes exchanged with the peer by protocol, cumulative since this
    /// node started and across reconnects. QUIC-level, not payload: see
    /// [`CoreQuicBytes`].
    pub quic_traffic: CoreQuicTraffic,
    /// Exact blob payload (and zstd wire) bytes exchanged with the peer,
    /// cumulative like `quic_traffic`. Zero from a bare monitor without a
    /// node's blob store.
    pub blob_transfer: crate::CorePeerBlobStats,
}

impl CorePeerView {
    /// Fold both ends' declarations into this view's measured level.
    pub fn with_declarations(
        mut self,
        mine: Option<&locality::CoreDeclaredLocality>,
        theirs: Option<&locality::CoreDeclaredLocality>,
    ) -> Self {
        if let Some((level, reason, source)) =
            locality::bound_level(Some((self.level, self.level_reason.clone())), mine, theirs)
        {
            self.level = level;
            self.level_reason = reason;
            self.level_source = source;
        }
        self.declared_region = theirs.map(|t| t.region.clone());
        self.declared_zone = theirs.and_then(|t| t.zone.clone());
        self
    }
}

/// A level for any node id, measured or not: see `CoreNode::topology_locate`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoreLevelEstimate {
    pub level: CoreLadderLevel,
    pub level_reason: String,
    pub level_source: locality::CoreLevelSource,
    pub declared_region: Option<String>,
    pub declared_zone: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn classify_ip_table() {
        assert_eq!(classify_ip(ip("127.0.0.1")), AddrClass::Loopback);
        assert_eq!(classify_ip(ip("::1")), AddrClass::Loopback);
        assert_eq!(classify_ip(ip("10.0.1.5")), AddrClass::Private);
        assert_eq!(classify_ip(ip("172.16.0.1")), AddrClass::Private);
        assert_eq!(classify_ip(ip("192.168.1.1")), AddrClass::Private);
        assert_eq!(classify_ip(ip("169.254.1.1")), AddrClass::Private);
        assert_eq!(classify_ip(ip("fd00::1")), AddrClass::Private); // ULA
        assert_eq!(classify_ip(ip("fe80::1")), AddrClass::Private); // link-local
        assert_eq!(classify_ip(ip("8.8.8.8")), AddrClass::Public);
        assert_eq!(classify_ip(ip("2001:4860:4860::8888")), AddrClass::Public);
        assert_eq!(classify_ip(ip("::ffff:192.168.0.1")), AddrClass::Private); // v4-mapped
        assert_eq!(classify_ip(ip("::ffff:8.8.8.8")), AddrClass::Public);
    }

    fn sample(remote: TransportAddr, rtt_ms: u64, conn_key: u64) -> PeerSample {
        sample_us(remote, rtt_ms * 1_000, conn_key)
    }

    fn sample_us(remote: TransportAddr, rtt_us: u64, conn_key: u64) -> PeerSample {
        PeerSample {
            conn_key,
            remote,
            rtt: Duration::from_micros(rtt_us),
            lost_packets: 0,
            congestion_events: 0,
            cwnd: 64 * 1024,
            tx_datagrams: 100,
            tx_bytes: 10_000,
            rx_bytes: 10_000,
            local_public_ips: vec![],
        }
    }

    fn direct(addr: &str) -> TransportAddr {
        TransportAddr::Ip(addr.parse().unwrap())
    }

    /// A view of `id` at `level` that has received `blobs` and sent `rpc`.
    fn traffic_view(id: &str, level: CoreLadderLevel, blobs: u64, rpc: u64) -> CorePeerView {
        let mut quic_traffic = CoreQuicTraffic::default();
        quic_traffic.add(CoreProtocolFamily::Blobs, 0, blobs);
        quic_traffic.add(CoreProtocolFamily::Rpc, rpc, 0);
        CorePeerView {
            node_id: id.to_string(),
            level,
            level_reason: String::new(),
            rtt_us: None,
            jitter_us: None,
            loss_ppm: 0,
            throughput_bps: 0,
            cwnd_ceiling_bps: 0,
            direct: true,
            relay_url: None,
            remote_addr: None,
            congestion_events: 0,
            samples: 1,
            confidence_ppm: 0,
            last_measured_unix_ms: 0,
            cluster_held_since_unix_ms: 0,
            is_connected: true,
            level_source: locality::CoreLevelSource::Measured,
            declared_region: None,
            declared_zone: None,
            quic_traffic,
            blob_transfer: crate::CorePeerBlobStats::default(),
        }
    }

    /// Every (level, protocol, direction) series of one ledger read.
    fn series(
        totals: &std::collections::BTreeMap<CoreLadderLevel, CoreQuicTraffic>,
    ) -> std::collections::BTreeMap<(CoreLadderLevel, &'static str, bool), u64> {
        let mut out = std::collections::BTreeMap::new();
        for (level, traffic) in totals {
            for (family, b) in traffic.iter() {
                out.insert((*level, family.as_str(), true), b.quic_bytes_sent);
                out.insert((*level, family.as_str(), false), b.quic_bytes_received);
            }
        }
        out
    }

    /// TM-3: `aster.peer.bytes` grouped by level is an observable counter, so
    /// no level series may drop. A peer moving level (and back), and a peer
    /// leaving the views, must leave its bytes where they were counted.
    #[test]
    fn level_traffic_never_decreases_when_a_peer_changes_level() {
        use CoreLadderLevel::{Far, SameLan, SameSite};
        let mut ledger = LevelTrafficLedger::default();
        let steps: Vec<Vec<CorePeerView>> = vec![
            vec![
                traffic_view("p", SameLan, 1_000, 10),
                traffic_view("q", SameLan, 500, 0),
            ],
            vec![
                traffic_view("p", SameLan, 4_000, 20),
                traffic_view("q", SameLan, 600, 0),
            ],
            // p re-estimated: its 4 000 bytes so far stay under same_lan.
            vec![
                traffic_view("p", SameSite, 4_100, 25),
                traffic_view("q", SameLan, 700, 0),
            ],
            vec![
                traffic_view("p", Far, 4_200, 25),
                traffic_view("q", SameLan, 700, 0),
            ],
            // Back to same_lan, then q leaves the views (expired).
            vec![traffic_view("p", SameLan, 4_500, 30)],
            vec![traffic_view("p", SameLan, 5_000, 30)],
        ];
        let mut prev = std::collections::BTreeMap::new();
        for (i, views) in steps.iter().enumerate() {
            let now = series(&ledger.observe(views));
            for (key, before) in &prev {
                let after = now.get(key).copied().unwrap_or(0);
                assert!(
                    after >= *before,
                    "step {i}: series {key:?} fell {before} -> {after}"
                );
            }
            prev = now;
        }
        // Nothing lost or double counted: the levels sum to the peers' totals.
        let total: u64 = prev
            .iter()
            .filter(|((_, family, sent), _)| *family == "blobs" && !sent)
            .map(|(_, n)| n)
            .sum();
        assert_eq!(total, 5_000 + 700);
        assert_eq!(prev[&(SameLan, "blobs", false)], 4_000 + 700 + 800);
        assert_eq!(prev[&(SameSite, "blobs", false)], 100);
        assert_eq!(prev[&(Far, "blobs", false)], 100);
    }

    #[test]
    fn first_level_commits_immediately() {
        let mut st = PeerTopoState::default();
        st.apply_sample(sample(direct("127.0.0.1:4433"), 1, 1));
        let view = st.to_view("peer", true).unwrap();
        assert_eq!(view.level, CoreLadderLevel::SameHost);
        assert_eq!(view.level_reason, "loopback path");
        assert!(view.rtt_us.unwrap() >= 900);
    }

    #[test]
    fn level_change_needs_hold_down() {
        let mut st = PeerTopoState::default();
        st.apply_sample(sample_us(direct("192.168.1.9:4433"), 200, 1));
        assert_eq!(
            st.to_view("p", true).unwrap().level,
            CoreLadderLevel::SameLan
        );

        // Path migrates to a far public address: level must hold until the
        // change persists LEVEL_HOLD_SAMPLES ticks.
        for _ in 0..(LEVEL_HOLD_SAMPLES - 1) {
            st.apply_sample(sample(direct("8.8.8.8:4433"), 200, 1));
            assert_eq!(
                st.to_view("p", true).unwrap().level,
                CoreLadderLevel::SameLan
            );
        }
        st.apply_sample(sample(direct("8.8.8.8:4433"), 200, 1));
        assert_eq!(st.to_view("p", true).unwrap().level, CoreLadderLevel::Far);
    }

    #[test]
    fn region_hysteresis_band() {
        // Committed L3 at 10ms; RTT drifting to 15ms (inside 12–18 band)
        // must keep proposing L3, not flap toward Far.
        let mut st = PeerTopoState::default();
        st.apply_sample(sample(direct("8.8.8.8:4433"), 10, 1));
        assert_eq!(
            st.to_view("p", true).unwrap().level,
            CoreLadderLevel::SameRegion
        );
        for _ in 0..LEVEL_HOLD_SAMPLES + 5 {
            st.apply_sample(sample(direct("8.8.8.8:4433"), 15, 1));
        }
        assert_eq!(
            st.to_view("p", true).unwrap().level,
            CoreLadderLevel::SameRegion
        );
    }

    #[test]
    fn same_egress_is_l2() {
        let mut st = PeerTopoState::default();
        let mut s = sample(direct("203.0.113.7:4433"), 5, 1);
        s.local_public_ips = vec![ip("203.0.113.7")];
        st.apply_sample(s);
        let view = st.to_view("p", true).unwrap();
        assert_eq!(view.level, CoreLadderLevel::SameSite);
    }

    #[test]
    fn confidence_grows_with_samples() {
        let mut st = PeerTopoState::default();
        st.apply_sample(sample(direct("8.8.8.8:4433"), 5, 1));
        let low = st.to_view("p", true).unwrap().confidence_ppm;
        for _ in 0..40 {
            st.apply_sample(sample(direct("8.8.8.8:4433"), 5, 1));
        }
        let high = st.to_view("p", true).unwrap().confidence_ppm;
        assert!(low < 100_000, "single sample should be low-confidence");
        assert_eq!(high, 990_000, "confidence caps at 0.99");
    }

    #[test]
    fn counter_baseline_resets_on_connection_set_change() {
        let mut st = PeerTopoState::default();
        let mut s1 = sample(direct("8.8.8.8:4433"), 5, 1);
        s1.tx_datagrams = 1000;
        s1.lost_packets = 0;
        st.apply_sample(s1);
        // Connection set changed (new conn_key): absolute sums are lower
        // than before — without a baseline reset this would underflow or
        // spike the loss estimate.
        let mut s2 = sample(direct("8.8.8.8:4433"), 5, 2);
        s2.tx_datagrams = 10;
        s2.lost_packets = 5;
        st.apply_sample(s2);
        assert_eq!(st.to_view("p", true).unwrap().loss_ppm, 0);
        // Third sample over the same set computes a real window.
        let mut s3 = sample(direct("8.8.8.8:4433"), 5, 2);
        s3.tx_datagrams = 110; // +100
        s3.lost_packets = 15; // +10 → 10% window loss
        st.apply_sample(s3);
        let loss = st.to_view("p", true).unwrap().loss_ppm;
        assert!(loss > 0, "loss should register after a same-set window");
    }

    #[test]
    fn cluster_hold_band_sets_below_enter_and_clears_above_exit() {
        let mut st = PeerTopoState::default();
        // 5ms < enter (12ms): hold starts.
        st.apply_sample(sample(direct("8.8.8.8:4433"), 5, 1));
        let held = st.to_view("p", true).unwrap().cluster_held_since_unix_ms;
        assert!(held > 0);

        // Drift into the 12–18ms band: hold must persist (and keep its
        // original start stamp).
        for _ in 0..20 {
            st.apply_sample(sample(direct("8.8.8.8:4433"), 15, 1));
        }
        assert_eq!(
            st.to_view("p", true).unwrap().cluster_held_since_unix_ms,
            held,
            "hold persists through the band without re-stamping"
        );

        // Rise above exit (18ms): hold clears.
        for _ in 0..30 {
            st.apply_sample(sample(direct("8.8.8.8:4433"), 60, 1));
        }
        assert_eq!(st.to_view("p", true).unwrap().cluster_held_since_unix_ms, 0);
    }

    #[test]
    fn no_level_committed_before_first_rtt_on_public_path() {
        let mut st = PeerTopoState::default();
        // Public path, RTT not yet measured: no signal, no level — a Far
        // commit here would cost a full hold-down once RTT arrives.
        st.apply_sample(sample(direct("8.8.8.8:4433"), 0, 1));
        assert!(st.to_view("p", true).is_none());
        // First measured RTT commits immediately (level was still unset).
        st.apply_sample(sample(direct("8.8.8.8:4433"), 10, 1));
        assert_eq!(
            st.to_view("p", true).unwrap().level,
            CoreLadderLevel::SameRegion
        );
    }

    #[test]
    fn private_path_without_rtt_gives_no_level() {
        // A bare private path is not a LAN: committing SameLan here would
        // take a full hold-down to correct once a cross-zone RTT arrives.
        let mut st = PeerTopoState::default();
        st.apply_sample(sample(direct("10.0.1.5:4433"), 0, 1));
        assert!(st.to_view("p", true).is_none());
    }

    #[test]
    fn private_path_under_lan_ceiling_is_same_lan() {
        let mut st = PeerTopoState::default();
        st.apply_sample(sample_us(direct("10.0.1.5:4433"), 150, 1));
        let view = st.to_view("p", true).unwrap();
        assert_eq!(view.level, CoreLadderLevel::SameLan);
        assert_eq!(view.level_reason, "private path, rtt under LAN ceiling");
    }

    #[test]
    fn cross_zone_private_path_is_not_same_lan() {
        // A VPC peer in another availability zone: private address, 1.5 ms.
        let mut st = PeerTopoState::default();
        st.apply_sample(sample_us(direct("10.1.7.20:4433"), 1_500, 1));
        let view = st.to_view("p", true).unwrap();
        assert_eq!(view.level, CoreLadderLevel::SameRegion);
        assert_eq!(view.level_reason, "rtt under region threshold");
    }

    #[test]
    fn derive_level_private_path_table() {
        let private = direct("10.0.1.5:4433");
        let lan = |rtt_us: f64, prev| derive_level(&private, &[], rtt_us, prev).map(|(l, _)| l);
        assert_eq!(lan(0.0, None), None);
        assert_eq!(lan(499.0, None), Some(CoreLadderLevel::SameLan));
        assert_eq!(lan(700.0, None), Some(CoreLadderLevel::SameRegion));
        // Hysteresis: a committed LAN peer keeps L1 up to the exit ceiling.
        assert_eq!(
            lan(700.0, Some(CoreLadderLevel::SameLan)),
            Some(CoreLadderLevel::SameLan)
        );
        assert_eq!(
            lan(1_200.0, Some(CoreLadderLevel::SameLan)),
            Some(CoreLadderLevel::SameRegion)
        );
        assert_eq!(lan(40_000.0, None), Some(CoreLadderLevel::Far));
    }
}
