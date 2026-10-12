//! Opt-in reciprocal enrollment on a restricted ALPN.
//!
//! Install before dialing, route only this ALPN to `accept`, and keep the
//! returned service alive. Ordinary protocols stay gated until verified
//! admission. Configured replica addresses are discovery hints, never trust.

use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, ensure, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, Semaphore, SemaphorePermit};

use super::{
    admit, AdmissionContext, AdmissionDecision, AdmissionEvidenceSource, AdmissionPolicy,
    EnrollmentPolicyStore, JwtEnrollmentVerifier, ObservedSourceAddress, PolicySynchronization,
    SourceAddressProvenance, SynchronizationSource, VerifiedAdmissionEvidence, MAX_JWT_BYTES,
};
use crate::{
    CoreAfterHandshakeDecision, CoreConnection, CoreDoc, CoreNode, CoreNodeAddr, CoreRecvStream,
    CoreSendStream,
};

pub const ENROLLMENT_ALPN: &[u8] = b"aster.enrollment/1";
const MAX_ENVELOPE: usize = MAX_JWT_BYTES + 8192;
const REJECT: u64 = 403;
const MAX_RATE_KEYS: usize = 8192;

#[derive(Clone, Debug)]
pub struct EnrollmentOptions {
    /// Bounds opening the first stream and receiving the credential-free probe.
    pub first_message_timeout: Duration,
    pub handshake_timeout: Duration,
    pub policy_timeout: Duration,
    pub reconciliation_interval: Duration,
    pub max_concurrent_handshakes: usize,
    pub max_concurrent_handshakes_per_peer: usize,
    pub max_concurrent_handshakes_per_source: usize,
    /// Separate capacity reserved for already-admitted ordinary connections.
    pub max_concurrent_gate_checks: usize,
    pub max_concurrent_reconciliations: usize,
    /// Per authenticated peer identity, rather than one globally shared budget.
    pub max_handshakes_per_second: u32,
    pub max_handshakes_per_second_per_source: u32,
    pub max_admitted_peers: usize,
}

impl Default for EnrollmentOptions {
    fn default() -> Self {
        Self {
            first_message_timeout: Duration::from_secs(2),
            handshake_timeout: Duration::from_secs(10),
            policy_timeout: Duration::from_secs(2),
            reconciliation_interval: Duration::from_secs(1),
            max_concurrent_handshakes: 16,
            max_concurrent_handshakes_per_peer: 1,
            max_concurrent_handshakes_per_source: 4,
            max_concurrent_gate_checks: 64,
            max_concurrent_reconciliations: 16,
            max_handshakes_per_second: 32,
            max_handshakes_per_second_per_source: 128,
            max_admitted_peers: 1024,
        }
    }
}

/// Readiness describes membership only, never operation/Exec permission.
#[derive(Clone, Debug)]
pub struct EnrollmentReadiness {
    pub peer: [u8; 32],
    pub source: AdmissionEvidenceSource,
    pub policy_sync: Option<PolicySynchronization>,
    /// Direct selected-path observation at admission/check time; relay paths
    /// provide no originating-peer address.
    pub observed_source: Option<ObservedSourceAddress>,
}

/// Consumers receive this after local reconciliation denies a formerly
/// admitted peer, and should cancel their own jobs/output access as required.
#[derive(Clone, Debug)]
pub struct EnrollmentRevoked {
    pub peer: [u8; 32],
}

#[derive(Clone)]
struct Session {
    generation: u64,
    evidence: VerifiedAdmissionEvidence,
    connections: Vec<CoreConnection>,
    observed_source: Option<ObservedSourceAddress>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Hello {
    version: u8,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplicaProof {
    version: u8,
    credential: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Presentation {
    credential: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Welcome {
    version: u8,
    policy_ticket: Option<String>,
}

struct RateState {
    started: Instant,
    peers: BTreeMap<[u8; 32], u32>,
    sources: BTreeMap<IpAddr, u32>,
}

#[derive(Default)]
struct HandshakeCounts {
    peers: BTreeMap<[u8; 32], usize>,
    sources: BTreeMap<IpAddr, usize>,
}

struct HandshakeSlot<'a> {
    counts: &'a Mutex<HandshakeCounts>,
    peer: [u8; 32],
    source: Option<IpAddr>,
    _permit: SemaphorePermit<'a>,
}

impl Drop for HandshakeSlot<'_> {
    fn drop(&mut self) {
        let mut counts = self
            .counts
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        decrement_count(&mut counts.peers, &self.peer);
        if let Some(source) = self.source {
            decrement_count(&mut counts.sources, &source);
        }
    }
}

fn decrement_count<K: Ord>(counts: &mut BTreeMap<K, usize>, key: &K) {
    if let Some(count) = counts.get_mut(key) {
        *count -= 1;
        if *count == 0 {
            counts.remove(key);
        }
    }
}

/// Shared core implementation; the facade delegates to this type.
pub struct EnrollmentService {
    node: CoreNode,
    store: Arc<EnrollmentPolicyStore>,
    verifier: JwtEnrollmentVerifier,
    credential: Option<String>,
    policy: Arc<dyn AdmissionPolicy>,
    options: EnrollmentOptions,
    document: RwLock<Option<CoreDoc>>,
    context: RwLock<AdmissionContext>,
    sessions: Mutex<BTreeMap<[u8; 32], Session>>,
    inbound_permits: Semaphore,
    outbound_permits: Semaphore,
    gate_permits: Arc<Semaphore>,
    handshake_counts: Mutex<HandshakeCounts>,
    rate: Mutex<RateState>,
    next_generation: AtomicU64,
    revoked: broadcast::Sender<EnrollmentRevoked>,
}

impl EnrollmentService {
    /// Requires a node started with hooks and `ENROLLMENT_ALPN` registered.
    /// Consumes its hook receiver and promptly services both hook directions.
    /// The consumer still owns the node's custom-ALPN accept loop.
    pub fn install(
        node: CoreNode,
        store: Arc<EnrollmentPolicyStore>,
        verifier: JwtEnrollmentVerifier,
        credential: Option<String>,
        policy: Arc<dyn AdmissionPolicy>,
        options: EnrollmentOptions,
    ) -> Result<Arc<Self>> {
        ensure!(
            options.max_concurrent_handshakes > 0
                && options.max_concurrent_handshakes_per_peer > 0
                && options.max_concurrent_handshakes_per_source > 0
                && options.max_concurrent_gate_checks > 0
                && options.max_concurrent_reconciliations > 0
                && options.max_admitted_peers > 0
                && options.max_handshakes_per_second > 0
                && options.max_handshakes_per_second_per_source > 0
                && !options.first_message_timeout.is_zero()
                && !options.handshake_timeout.is_zero()
                && !options.policy_timeout.is_zero()
                && !options.reconciliation_interval.is_zero(),
            "invalid enrollment bounds"
        );
        ensure!(
            node.inner.enrollment_hook_failure_mode == crate::trust::HookFailureMode::FailClosed,
            "enrollment requires fail-closed handshake hooks"
        );
        ensure!(
            verifier.trusted_root() == store.root()
                && verifier.intended_audience() == store.audience(),
            "enrollment verifier and policy store trust contexts differ"
        );
        if let Some(token) = &credential {
            verifier.verify(token, &identity(&node.node_id())?, unix_now())?;
        }
        let mut hooks = node
            .take_hook_receiver()
            .ok_or_else(|| anyhow!("enrollment requires unused node hooks"))?;
        let (revoked, _) = broadcast::channel(128);
        let service = Arc::new(Self {
            node,
            store,
            verifier,
            credential,
            policy,
            inbound_permits: Semaphore::new(options.max_concurrent_handshakes),
            outbound_permits: Semaphore::new(options.max_concurrent_handshakes),
            gate_permits: Arc::new(Semaphore::new(options.max_concurrent_gate_checks)),
            handshake_counts: Mutex::new(HandshakeCounts::default()),
            options,
            document: RwLock::new(None),
            context: RwLock::new(AdmissionContext::default()),
            sessions: Mutex::new(BTreeMap::new()),
            rate: Mutex::new(RateState {
                started: Instant::now(),
                peers: BTreeMap::new(),
                sources: BTreeMap::new(),
            }),
            next_generation: AtomicU64::new(1),
            revoked,
        });
        let weak = Arc::downgrade(&service);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    Some((_, reply)) = hooks.before_connect_rx.recv() => {
                        let _ = reply.send(true);
                    }
                    Some((info, reply)) = hooks.after_handshake_rx.recv() => {
                        let Some(service) = weak.upgrade() else { break; };
                        if info.alpn == ENROLLMENT_ALPN {
                            let allowed = identity(&info.remote_endpoint_id).is_ok_and(|peer| service.take_rate_slot(peer, info.observed_peer_addr.map(|address| address.ip())));
                            let _ = reply.send(hook_decision(allowed));
                        } else if let Ok(peer) = identity(&info.remote_endpoint_id) {
                            let permit = if service.has_session(&peer) { service.gate_permits.clone().try_acquire_owned().ok() } else { None };
                            if let Some(permit) = permit {
                                tokio::spawn(async move {
                                    let _permit = permit;
                                    let source = info.observed_peer_addr.map(|address| ObservedSourceAddress {
                                        address: address.ip(), provenance: SourceAddressProvenance::DirectPeer,
                                    });
                                    let _ = reply.send(hook_decision(service.check_peer_at_source(&peer, source).await.is_ok()));
                                });
                            } else { let _ = reply.send(hook_decision(false)); }
                        } else { let _ = reply.send(hook_decision(false)); }
                    }
                    else => break,
                }
            }
        });
        let weak = Arc::downgrade(&service);
        let interval = service.options.reconciliation_interval;
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            loop {
                ticker.tick().await;
                let Some(service) = weak.upgrade() else {
                    break;
                };
                service.reconcile().await;
            }
        });
        Ok(service)
    }

    pub fn revocations(&self) -> broadcast::Receiver<EnrollmentRevoked> {
        self.revoked.subscribe()
    }

    /// Attach the pinned policy namespace. It may be a read-only replica.
    pub fn set_policy_document(&self, doc: CoreDoc) -> Result<()> {
        ensure!(
            namespace(&doc.doc_id())? == *self.store.namespace(),
            "wrong policy namespace"
        );
        *self.document.write().unwrap_or_else(|e| e.into_inner()) = Some(doc);
        Ok(())
    }

    fn take_rate_slot(&self, peer: [u8; 32], source: Option<IpAddr>) -> bool {
        let mut rate = self.rate.lock().unwrap_or_else(|e| e.into_inner());
        if rate.started.elapsed() >= Duration::from_secs(1) {
            rate.started = Instant::now();
            rate.peers.clear();
            rate.sources.clear();
        }
        if rate.peers.get(&peer).copied().unwrap_or_default()
            >= self.options.max_handshakes_per_second
            || (!rate.peers.contains_key(&peer) && rate.peers.len() >= MAX_RATE_KEYS)
            || source.is_some_and(|source| {
                rate.sources.get(&source).copied().unwrap_or_default()
                    >= self.options.max_handshakes_per_second_per_source
                    || (!rate.sources.contains_key(&source) && rate.sources.len() >= MAX_RATE_KEYS)
            })
        {
            return false;
        }
        *rate.peers.entry(peer).or_default() += 1;
        if let Some(source) = source {
            *rate.sources.entry(source).or_default() += 1;
        }
        true
    }

    fn inbound_slot(&self, peer: [u8; 32], source: Option<IpAddr>) -> Result<HandshakeSlot<'_>> {
        let permit = self
            .inbound_permits
            .try_acquire()
            .map_err(|_| anyhow!("enrollment busy"))?;
        let mut counts = self
            .handshake_counts
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        ensure!(
            counts.peers.get(&peer).copied().unwrap_or_default()
                < self.options.max_concurrent_handshakes_per_peer,
            "peer enrollment concurrency limit"
        );
        ensure!(
            source.is_none_or(
                |source| counts.sources.get(&source).copied().unwrap_or_default()
                    < self.options.max_concurrent_handshakes_per_source
            ),
            "source enrollment concurrency limit"
        );
        *counts.peers.entry(peer).or_default() += 1;
        if let Some(source) = source {
            *counts.sources.entry(source).or_default() += 1;
        }
        Ok(HandshakeSlot {
            counts: &self.handshake_counts,
            peer,
            source,
            _permit: permit,
        })
    }

    async fn evidence(
        &self,
        peer: &[u8; 32],
        token: Option<&str>,
        observed_source: Option<ObservedSourceAddress>,
    ) -> Result<VerifiedAdmissionEvidence> {
        let doc = self
            .document
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(doc) = &doc {
            let pending = match self.store.refresh_node(doc, peer).await {
                Ok(availability) => availability == super::RecordAvailability::Pending,
                Err(super::RecordError::StaleRevision) => true,
                Err(error) => return Err(error.into()),
            };
            if pending {
                self.context
                    .write()
                    .unwrap_or_else(|e| e.into_inner())
                    .pending_policy_content = true;
            }
        }
        // Independent root registration replaces presentation evidence. An
        // obsolete/expired JWT must not block a newer valid registration.
        if let Ok(registration) = self.store.select_evidence(peer, None, unix_now()) {
            self.evaluate(&registration, observed_source).await?;
            return Ok(registration);
        }
        let jwt = token
            .map(|token| self.verifier.verify(token, peer, unix_now()))
            .transpose()?;
        if let Some(doc) = &doc {
            if let Some(id) = jwt.as_ref().and_then(|jwt| jwt.credential_id()) {
                let pending = match self.store.refresh_credential(doc, id).await {
                    Ok(availability) => availability == super::RecordAvailability::Pending,
                    Err(super::RecordError::StaleRevision) => true,
                    Err(error) => return Err(error.into()),
                };
                if pending {
                    self.context
                        .write()
                        .unwrap_or_else(|e| e.into_inner())
                        .pending_policy_content = true;
                }
            }
        }
        let evidence = self.store.select_evidence(peer, jwt.as_ref(), unix_now())?;
        self.evaluate(&evidence, observed_source).await?;
        Ok(evidence)
    }

    async fn evaluate(
        &self,
        evidence: &VerifiedAdmissionEvidence,
        observed_source: Option<ObservedSourceAddress>,
    ) -> Result<()> {
        self.evaluate_with_policy(
            evidence,
            observed_source,
            self.policy.as_ref(),
            self.options.policy_timeout,
        )
        .await
    }

    async fn evaluate_with_policy(
        &self,
        evidence: &VerifiedAdmissionEvidence,
        observed_source: Option<ObservedSourceAddress>,
        policy: &dyn AdmissionPolicy,
        timeout: Duration,
    ) -> Result<()> {
        let mut context = self
            .context
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        context.observed_source = observed_source;
        ensure!(
            admit(
                evidence,
                self.store.as_ref(),
                &context,
                policy,
                timeout,
                unix_now
            )
            .await
                == AdmissionDecision::Allow,
            "enrollment denied"
        );
        Ok(())
    }

    fn remember(
        &self,
        evidence: VerifiedAdmissionEvidence,
        observed_source: Option<ObservedSourceAddress>,
    ) -> Result<EnrollmentReadiness> {
        self.replace_session(evidence, observed_source, None)
    }

    /// Reconciliation may replace exactly the generation it evaluated, never
    /// resurrect a removed peer or overwrite a concurrent fresh handshake.
    fn replace_session(
        &self,
        evidence: VerifiedAdmissionEvidence,
        observed_source: Option<ObservedSourceAddress>,
        expected_generation: Option<u64>,
    ) -> Result<EnrollmentReadiness> {
        let peer = *evidence.subject();
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(expected) = expected_generation {
            ensure!(
                sessions
                    .get(&peer)
                    .is_some_and(|session| session.generation == expected),
                "admission changed during reconciliation"
            );
        }
        ensure!(
            sessions.contains_key(&peer) || sessions.len() < self.options.max_admitted_peers,
            "admitted peer limit"
        );
        if sessions.get(&peer).is_some_and(|session| {
            session.evidence.source() == evidence.source()
                && session.evidence.original_jwt() == evidence.original_jwt()
                && session.observed_source == observed_source
        }) {
            return Ok(self.readiness(peer, evidence.source(), observed_source));
        }
        let source = evidence.source();
        let generation = self
            .next_generation
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .map_err(|_| anyhow!("admission generation exhausted"))?;
        let connections = sessions
            .remove(&peer)
            .map(|session| session.connections)
            .unwrap_or_default();
        sessions.insert(
            peer,
            Session {
                generation,
                evidence,
                connections,
                observed_source,
            },
        );
        Ok(self.readiness(peer, source, observed_source))
    }

    fn readiness(
        &self,
        peer: [u8; 32],
        source: AdmissionEvidenceSource,
        observed_source: Option<ObservedSourceAddress>,
    ) -> EnrollmentReadiness {
        EnrollmentReadiness {
            peer,
            source,
            policy_sync: self
                .context
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .policy_sync
                .clone(),
            observed_source,
        }
    }

    fn session_snapshot(&self, peer: &[u8; 32]) -> Result<Session> {
        self.sessions
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(peer)
            .cloned()
            .ok_or_else(|| anyhow!("peer not admitted"))
    }

    fn has_session(&self, peer: &[u8; 32]) -> bool {
        self.sessions
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .contains_key(peer)
    }

    fn ensure_generation(&self, peer: &[u8; 32], generation: u64) -> Result<()> {
        ensure!(
            self.sessions
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .get(peer)
                .is_some_and(|session| session.generation == generation),
            "admission changed during evaluation"
        );
        Ok(())
    }

    /// Handle one connection selected by the caller's sole ALPN accept loop.
    /// Failure closes it. At most one bounded handshake is consumed.
    pub async fn accept(&self, connection: &CoreConnection) -> Result<EnrollmentReadiness> {
        let result = tokio::time::timeout(self.options.handshake_timeout, async {
            ensure!(
                connection.connection_info().alpn == ENROLLMENT_ALPN,
                "wrong enrollment ALPN"
            );
            let peer = identity(&connection.remote_id())?;
            let source = observed_source(connection);
            let _slot = self.inbound_slot(peer, source.map(|source| source.address))?;
            let (send, recv, hello) =
                tokio::time::timeout(self.options.first_message_timeout, async {
                    let (send, recv) = connection.accept_bi().await?;
                    let hello: Hello = receive(&recv).await?;
                    Ok::<_, anyhow::Error>((send, recv, hello))
                })
                .await
                .map_err(|_| anyhow!("enrollment first message timeout"))??;
            ensure!(hello.version == 1, "unsupported enrollment version");
            // Joining credentials are withheld until the client has verified
            // this responder. The responder's own identity proof is explicitly
            // public to bounded unauthenticated probes; no policy ticket yet.
            transmit(
                &send,
                &ReplicaProof {
                    version: 1,
                    credential: self.credential.clone(),
                },
            )
            .await?;
            let presentation: Presentation = receive(&recv).await?;
            let evidence = self
                .evidence(&peer, presentation.credential.as_deref(), source)
                .await?;
            let doc = self
                .document
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let ticket = match doc {
                Some(doc) => Some(doc.share_with_addr("read".into()).await?),
                None => None,
            };
            transmit(
                &send,
                &Welcome {
                    version: 1,
                    policy_ticket: ticket,
                },
            )
            .await?;
            // Explicit reciprocal confirmation: a peer that rejects our identity
            // must not leave a one-way admission behind.
            let confirmed: bool = receive(&recv).await?;
            ensure!(confirmed, "reciprocal enrollment rejected");
            self.evaluate(&evidence, source).await?;
            transmit(&send, &true).await?;
            let received: bool = receive(&recv).await?;
            ensure!(received, "reciprocal confirmation missing");
            self.evaluate(&evidence, source).await?;
            self.remember(evidence, source)
        })
        .await
        .map_err(|_| anyhow!("enrollment handshake timeout"))
        .and_then(|x| x);
        let (code, reason) = if result.is_ok() {
            (0, b"enrollment complete".to_vec())
        } else {
            (REJECT, b"enrollment denied".to_vec())
        };
        let _ = connection.close(code, reason);
        result
    }

    /// Try configured reachable replicas in order; addresses carry no authority.
    pub async fn connect_replicas(
        &self,
        replicas: Vec<CoreNodeAddr>,
    ) -> Result<(EnrollmentReadiness, Option<String>)> {
        for replica in replicas {
            if let Ok(value) = self.connect(replica).await {
                return Ok(value);
            }
        }
        bail!("no configured replica completed reciprocal enrollment")
    }

    pub async fn connect(
        &self,
        replica: CoreNodeAddr,
    ) -> Result<(EnrollmentReadiness, Option<String>)> {
        tokio::time::timeout(self.options.handshake_timeout, async {
            let _permit = self
                .outbound_permits
                .try_acquire()
                .map_err(|_| anyhow!("enrollment busy"))?;
            let connection = self
                .node
                .net_client()
                .connect_node_addr(replica, ENROLLMENT_ALPN.to_vec())
                .await?;
            let _close_on_completion_or_cancel = CloseEnrollmentConnection(connection.clone());
            let result = async {
                let (send, recv) = connection.open_bi().await?;
                transmit(&send, &Hello { version: 1 }).await?;
                let proof: ReplicaProof =
                    tokio::time::timeout(self.options.first_message_timeout, receive(&recv))
                        .await
                        .map_err(|_| anyhow!("replica identity proof timeout"))??;
                ensure!(proof.version == 1, "unsupported enrollment version");
                let peer = identity(&connection.remote_id())?;
                let source = observed_source(&connection);
                let evidence = self
                    .evidence(&peer, proof.credential.as_deref(), source)
                    .await?;
                transmit(
                    &send,
                    &Presentation {
                        credential: self.credential.clone(),
                    },
                )
                .await?;
                let welcome: Welcome = receive(&recv).await?;
                ensure!(welcome.version == 1, "unsupported enrollment version");
                transmit(&send, &true).await?;
                let confirmed: bool = receive(&recv).await?;
                ensure!(confirmed, "reciprocal enrollment rejected");
                self.evaluate(&evidence, source).await?;
                transmit(&send, &true).await?;
                let closed = connection.closed().await;
                ensure!(
                    closed.code == Some(0),
                    "replica did not complete enrollment"
                );
                self.evaluate(&evidence, source).await?;
                Ok((self.remember(evidence, source)?, welcome.policy_ticket))
            }
            .await;
            if result.is_err() {
                let _ = connection.close(REJECT, b"enrollment denied".to_vec());
            }
            result
        })
        .await
        .map_err(|_| anyhow!("enrollment handshake timeout"))?
    }

    /// Import only a read capability for the pinned namespace, sync from the
    /// authenticated replica, and wait for successful reconciliation. A replica
    /// timestamp never proves current root policy. Missing content stays unknown.
    pub async fn synchronize(
        &self,
        peer: [u8; 32],
        ticket: String,
        timeout: Duration,
    ) -> Result<CoreDoc> {
        use iroh_docs::{Capability, DocTicket};
        use iroh_tickets::Ticket;
        let mut decoded = DocTicket::decode_string(&ticket)?;
        ensure!(
            matches!(decoded.capability, Capability::Read(_)),
            "bootstrap requires read-only policy capability"
        );
        ensure!(
            namespace(&decoded.capability.id().to_string())? == *self.store.namespace(),
            "wrong policy namespace"
        );
        decoded.nodes.retain(|node| node.id.as_bytes() == &peer);
        ensure!(
            !decoded.nodes.is_empty(),
            "policy ticket has no authenticated replica address"
        );
        self.check_peer(&peer).await?;
        let doc = self
            .node
            .docs_client()
            .join(decoded.encode_string())
            .await?;
        let events = doc.doc.subscribe().await?;
        futures_lite::pin!(events);
        self.set_policy_document(doc.clone())?;
        self.context
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .pending_policy_content = true;
        doc.start_sync(vec![hex::encode(peer)]).await?;
        let result = tokio::time::timeout(timeout, async {
            use futures_lite::StreamExt;
            let mut synchronized = false;
            loop {
                match events.next().await {
                    Some(Ok(iroh_docs::engine::LiveEvent::SyncFinished(event)))
                        if identity(&event.peer.to_string())? == peer =>
                    {
                        event
                            .result
                            .map_err(|error| anyhow!("policy sync failed: {error}"))?;
                        synchronized = true;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(error)) => return Err(error),
                    None => bail!("policy sync ended"),
                }
                if synchronized {
                    let report = self.store.refresh(&doc).await?;
                    self.context
                        .write()
                        .unwrap_or_else(|e| e.into_inner())
                        .pending_policy_content = report.has_uncertainty();
                    if report.pending == 0 {
                        break;
                    }
                }
            }
            Ok::<(), anyhow::Error>(())
        })
        .await;
        result.map_err(|_| anyhow!("policy synchronization timeout"))??;
        self.reconcile().await;
        // This observes successful transport reconciliation only. Individual
        // policy reads still report pending content and must not be fabricated.
        self.context
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .policy_sync = Some(PolicySynchronization {
            completed_at: unix_now(),
            source: if peer == *self.store.root() {
                SynchronizationSource::Root
            } else {
                SynchronizationSource::Replica(peer)
            },
            observed_revision: None,
        });
        self.check_peer(&peer).await?;
        Ok(doc)
    }

    /// Recheck before each consumer operation. Gate-0 also calls this before
    /// accepting each new ordinary connection. This does not authorize the
    /// operation itself.
    pub async fn check_peer(&self, peer: &[u8; 32]) -> Result<EnrollmentReadiness> {
        let session = self.session_snapshot(peer)?;
        if self
            .evaluate(&session.evidence, session.observed_source)
            .await
            .is_err()
        {
            self.revoke_generation(peer, session.generation);
            bail!("peer no longer admitted");
        }
        self.ensure_generation(peer, session.generation)?;
        Ok(self.readiness(*peer, session.evidence.source(), session.observed_source))
    }

    /// Recheck a live connection's current selected path at operation time.
    /// Consumers call this after path changes or before sensitive operations.
    pub async fn check_connection(
        &self,
        connection: &CoreConnection,
    ) -> Result<EnrollmentReadiness> {
        let peer = identity(&connection.remote_id())?;
        self.check_peer_at_source(&peer, observed_source(connection))
            .await
    }

    /// Evaluate a stronger consumer operation policy against the same opaque
    /// evidence, real pending/freshness state, and current connection path.
    /// Mandatory validity and known revocations still surround that handler.
    pub async fn check_connection_with_policy(
        &self,
        connection: &CoreConnection,
        policy: &dyn AdmissionPolicy,
        timeout: Duration,
    ) -> Result<EnrollmentReadiness> {
        let peer = identity(&connection.remote_id())?;
        self.check_connection(connection).await?;
        let session = self.session_snapshot(&peer)?;
        let source = observed_source(connection);
        self.evaluate_with_policy(&session.evidence, source, policy, timeout)
            .await?;
        self.ensure_generation(&peer, session.generation)?;
        Ok(self.readiness(peer, session.evidence.source(), source))
    }

    async fn check_peer_at_source(
        &self,
        peer: &[u8; 32],
        source: Option<ObservedSourceAddress>,
    ) -> Result<EnrollmentReadiness> {
        let session = self.session_snapshot(peer)?;
        self.evaluate(&session.evidence, source).await?;
        self.ensure_generation(peer, session.generation)?;
        Ok(self.readiness(*peer, session.evidence.source(), source))
    }

    /// Opt into closing a custom connection when this service learns denial.
    /// Built-in Docs/blobs/gossip connections are not exposed by this API;
    /// consumers must use revocation notifications for their own teardown.
    pub async fn track_connection(&self, connection: CoreConnection) -> Result<()> {
        let peer = identity(&connection.remote_id())?;
        self.check_connection(&connection).await?;
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let session = sessions
            .get_mut(&peer)
            .ok_or_else(|| anyhow!("peer revoked"))?;
        session
            .connections
            .retain(|connection| connection.connection_info().is_connected);
        ensure!(session.connections.len() < 64, "tracked connection limit");
        session.connections.push(connection);
        Ok(())
    }

    fn revoke_generation(&self, peer: &[u8; 32], expected_generation: u64) {
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if sessions
            .get(peer)
            .is_none_or(|session| session.generation != expected_generation)
        {
            return;
        }
        if let Some(session) = sessions.remove(peer) {
            drop(sessions);
            for connection in session.connections {
                let _ = connection.close(REJECT, b"enrollment revoked".to_vec());
            }
            let _ = self.revoked.send(EnrollmentRevoked { peer: *peer });
        }
    }

    /// Refresh currently admitted identities from the local Docs replica.
    /// Errors/pending content never invent a revocation or new authority.
    pub async fn reconcile(&self) {
        let doc = self
            .document
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(doc) = doc {
            let pending = match self.store.refresh(&doc).await {
                Ok(report) => report.has_uncertainty(),
                Err(_) => true,
            };
            self.context
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .pending_policy_content = pending;
        }
        let peers: Vec<_> = self
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(peer, session)| {
                (
                    *peer,
                    session.evidence.clone(),
                    session.observed_source,
                    session.generation,
                )
            })
            .collect();
        // Enforce all already-observed mandatory denials before awaiting any
        // consumer handler. A slow unrelated policy cannot defer revocations.
        let mut pending = VecDeque::new();
        for (peer, evidence, source, generation) in peers {
            if self
                .store
                .select_evidence(&peer, Some(&evidence), unix_now())
                .is_err()
            {
                self.revoke_generation(&peer, generation);
            } else {
                pending.push_back((peer, evidence, source, generation));
            }
        }
        let count = pending
            .len()
            .min(self.options.max_concurrent_reconciliations);
        let pending = Arc::new(Mutex::new(pending));
        let mut workers: Pin<Box<dyn Future<Output = ()> + Send + '_>> = Box::pin(async {});
        for _ in 0..count {
            let pending = pending.clone();
            let worker = async move {
                loop {
                    let next = pending
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .pop_front();
                    let Some((peer, old, source, generation)) = next else {
                        break;
                    };
                    match self.evidence(&peer, old.original_jwt(), source).await {
                        Ok(evidence) => {
                            let _ = self.replace_session(evidence, source, Some(generation));
                        }
                        Err(_) => self.revoke_generation(&peer, generation),
                    }
                }
            };
            workers = Box::pin(async move {
                futures_lite::future::zip(workers, worker).await;
            });
        }
        workers.await;
    }
}

fn identity(hex: &str) -> Result<[u8; 32]> {
    Ok(super::canonical_identity(hex)?)
}
fn namespace(hex: &str) -> Result<[u8; 32]> {
    hex::decode(hex)?
        .try_into()
        .map_err(|_| anyhow!("invalid node identity"))
}
fn observed_source(connection: &CoreConnection) -> Option<ObservedSourceAddress> {
    connection
        .transport_snapshot()
        .peer_addr
        .map(|address| ObservedSourceAddress {
            address: address.ip(),
            provenance: SourceAddressProvenance::DirectPeer,
        })
}
fn hook_decision(allow: bool) -> CoreAfterHandshakeDecision {
    if allow {
        CoreAfterHandshakeDecision::Accept
    } else {
        CoreAfterHandshakeDecision::Reject {
            error_code: REJECT as u32,
            reason: b"enrollment required".to_vec(),
        }
    }
}
struct CloseEnrollmentConnection(CoreConnection);
impl Drop for CloseEnrollmentConnection {
    fn drop(&mut self) {
        let _ = self.0.close(0, b"enrollment complete".to_vec());
    }
}
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

async fn transmit(send: &CoreSendStream, value: &impl Serialize) -> Result<()> {
    let payload = serde_json::to_vec(value)?;
    ensure!(
        payload.len() <= MAX_ENVELOPE,
        "enrollment envelope too large"
    );
    send.write_all((payload.len() as u32).to_be_bytes().to_vec())
        .await?;
    send.write_all(payload).await?;
    Ok(())
}
async fn receive<T: for<'de> Deserialize<'de>>(recv: &CoreRecvStream) -> Result<T> {
    let len = u32::from_be_bytes(recv.read_exact(4).await?.try_into().unwrap()) as usize;
    ensure!(len <= MAX_ENVELOPE, "enrollment envelope too large");
    Ok(serde_json::from_slice(&recv.read_exact(len).await?)?)
}

#[cfg(test)]
mod tests {
    use super::super::{DenialReason, EnrollmentIssueRequest, JwtEnrollmentIssuer, PolicyError};
    use super::*;
    use std::sync::atomic::AtomicBool;
    use tokio::sync::Notify;

    const AUDIENCE: &str = "bootstrap-races";

    fn public(seed: u8) -> [u8; 32] {
        JwtEnrollmentIssuer::from_seed(&[seed; 32]).public_key()
    }

    fn evidence(seed: u8, credential: &str) -> VerifiedAdmissionEvidence {
        let mut request = EnrollmentIssueRequest::new(public(seed), AUDIENCE);
        request.credential_id = Some(credential.into());
        let issued = JwtEnrollmentIssuer::from_seed(&[1; 32])
            .issue(&request, unix_now())
            .unwrap();
        JwtEnrollmentVerifier::new(public(1), AUDIENCE)
            .unwrap()
            .verify(issued.jwt(), &public(seed), unix_now())
            .unwrap()
    }

    struct PausedPolicy {
        pause_next: AtomicBool,
        evaluated: AtomicU64,
        entered: Notify,
        release: Notify,
        mixed: bool,
    }

    impl PausedPolicy {
        fn new(mixed: bool) -> Arc<Self> {
            Arc::new(Self {
                pause_next: AtomicBool::new(true),
                evaluated: AtomicU64::new(0),
                entered: Notify::new(),
                release: Notify::new(),
                mixed,
            })
        }
    }

    #[async_trait::async_trait]
    impl AdmissionPolicy for PausedPolicy {
        async fn evaluate(
            &self,
            evidence: &VerifiedAdmissionEvidence,
            _: &AdmissionContext,
            _: u64,
        ) -> std::result::Result<AdmissionDecision, PolicyError> {
            if evidence.subject() == &public(3) && self.pause_next.swap(false, Ordering::SeqCst) {
                self.entered.notify_one();
                self.release.notified().await;
            }
            self.evaluated.fetch_add(1, Ordering::SeqCst);
            if self.mixed && evidence.subject() == &public(4) {
                Ok(AdmissionDecision::Deny(DenialReason::Consumer(
                    "test denial".into(),
                )))
            } else if self.mixed && evidence.subject() == &public(5) {
                Err(PolicyError::Handler("test error".into()))
            } else {
                Ok(AdmissionDecision::Allow)
            }
        }
    }

    // Bypass install's periodic task so each interleaving is explicitly driven.
    async fn service(
        policy: Arc<dyn AdmissionPolicy>,
        options: EnrollmentOptions,
    ) -> (Arc<EnrollmentService>, std::path::PathBuf) {
        let node = CoreNode::memory_with_alpns(
            Vec::new(),
            Some(crate::CoreEndpointConfig {
                relay_mode: Some("disabled".into()),
                bind_addr: Some("127.0.0.1:0".into()),
                secret_key: Some(vec![2; 32]),
                ..Default::default()
            }),
        )
        .await
        .unwrap();
        let mut nonce = [0; 8];
        getrandom::getrandom(&mut nonce).unwrap();
        let path = std::env::temp_dir().join(format!(
            "aster-bootstrap-{}-{}.json",
            std::process::id(),
            hex::encode(nonce)
        ));
        let store =
            Arc::new(EnrollmentPolicyStore::open(&path, public(1), [42; 32], AUDIENCE).unwrap());
        let (revoked, _) = broadcast::channel(128);
        (
            Arc::new(EnrollmentService {
                node,
                store,
                verifier: JwtEnrollmentVerifier::new(public(1), AUDIENCE).unwrap(),
                credential: None,
                policy,
                inbound_permits: Semaphore::new(options.max_concurrent_handshakes),
                outbound_permits: Semaphore::new(options.max_concurrent_handshakes),
                gate_permits: Arc::new(Semaphore::new(options.max_concurrent_gate_checks)),
                options,
                document: RwLock::new(None),
                context: RwLock::new(AdmissionContext::default()),
                sessions: Mutex::new(BTreeMap::new()),
                handshake_counts: Mutex::new(HandshakeCounts::default()),
                rate: Mutex::new(RateState {
                    started: Instant::now(),
                    peers: BTreeMap::new(),
                    sources: BTreeMap::new(),
                }),
                next_generation: AtomicU64::new(1),
                revoked,
            }),
            path,
        )
    }

    async fn cleanup(service: Arc<EnrollmentService>, path: std::path::PathBuf) {
        service.node.close().await;
        drop(service);
        let mut lock_path = path.as_os_str().to_os_string();
        lock_path.push(".lock");
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(lock_path);
    }

    #[tokio::test]
    async fn reconciliation_cannot_resurrect_or_overwrite_reenrollment() {
        for reenroll in [false, true] {
            let policy = PausedPolicy::new(false);
            let (service, path) = service(policy.clone(), EnrollmentOptions::default()).await;
            service.remember(evidence(3, "old"), None).unwrap();
            let old_generation = service.session_snapshot(&public(3)).unwrap().generation;
            let task = tokio::spawn({
                let service = service.clone();
                async move { service.reconcile().await }
            });
            policy.entered.notified().await;
            service.revoke_generation(&public(3), old_generation);
            if reenroll {
                service.remember(evidence(3, "new"), None).unwrap();
            }
            policy.release.notify_one();
            task.await.unwrap();
            if reenroll {
                assert_eq!(
                    service
                        .session_snapshot(&public(3))
                        .unwrap()
                        .evidence
                        .credential_id(),
                    Some("new")
                );
            } else {
                assert!(service.session_snapshot(&public(3)).is_err());
            }
            cleanup(service, path).await;
        }
    }

    #[tokio::test]
    async fn unchanged_reconciliation_preserves_slow_check_generation() {
        let policy = PausedPolicy::new(false);
        let (service, path) = service(policy.clone(), EnrollmentOptions::default()).await;
        service.remember(evidence(3, "same"), None).unwrap();
        let generation = service.session_snapshot(&public(3)).unwrap().generation;
        let task = tokio::spawn({
            let service = service.clone();
            async move { service.check_peer(&public(3)).await }
        });
        policy.entered.notified().await;
        service.reconcile().await;
        assert_eq!(
            service.session_snapshot(&public(3)).unwrap().generation,
            generation
        );
        policy.release.notify_one();
        assert!(task.await.unwrap().is_ok());
        cleanup(service, path).await;
    }

    #[tokio::test]
    async fn reconciliation_isolates_slow_denied_and_failed_policies() {
        let policy = PausedPolicy::new(true);
        let (service, path) = service(
            policy.clone(),
            EnrollmentOptions {
                max_concurrent_reconciliations: 4,
                ..Default::default()
            },
        )
        .await;
        for seed in [3, 4, 5, 7] {
            service
                .remember(evidence(seed, &format!("peer-{seed}")), None)
                .unwrap();
        }
        for seed in 20..52 {
            service
                .remember(evidence(seed, &format!("queued-{seed}")), None)
                .unwrap();
        }
        let task = tokio::spawn({
            let service = service.clone();
            async move { service.reconcile().await }
        });
        policy.entered.notified().await;
        tokio::time::timeout(Duration::from_secs(1), async {
            while service.has_session(&public(4))
                || service.has_session(&public(5))
                || policy.evaluated.load(Ordering::SeqCst) < 35
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(!task.is_finished());
        assert!(service.has_session(&public(3)) && service.has_session(&public(7)));
        policy.release.notify_one();
        task.await.unwrap();
        assert!(service.check_peer(&public(7)).await.is_ok());
        cleanup(service, path).await;
    }

    #[tokio::test]
    async fn handshake_limits_are_independent_and_release_bounded_state() {
        let (service, path) = service(
            PausedPolicy::new(false),
            EnrollmentOptions {
                max_concurrent_handshakes: 2,
                max_concurrent_handshakes_per_source: 1,
                max_handshakes_per_second: 2,
                max_handshakes_per_second_per_source: 3,
                ..Default::default()
            },
        )
        .await;
        let first: IpAddr = "127.0.0.1".parse().unwrap();
        let second: IpAddr = "127.0.0.2".parse().unwrap();
        let one = service.inbound_slot(public(3), Some(first)).unwrap();
        assert!(service.inbound_slot(public(3), Some(second)).is_err());
        assert!(service.inbound_slot(public(4), Some(first)).is_err());
        let two = service.inbound_slot(public(4), Some(second)).unwrap();
        assert!(service.inbound_slot(public(5), None).is_err());
        assert!(service.outbound_permits.try_acquire().is_ok());
        assert!(service.gate_permits.try_acquire().is_ok());
        drop((one, two));
        assert!(service.handshake_counts.lock().unwrap().peers.is_empty());
        assert!(service.handshake_counts.lock().unwrap().sources.is_empty());
        assert!(service.take_rate_slot(public(3), Some(first)));
        assert!(service.take_rate_slot(public(3), Some(first)));
        assert!(!service.take_rate_slot(public(3), Some(first)));
        assert!(service.take_rate_slot(public(4), Some(first)));
        assert!(!service.take_rate_slot(public(5), Some(first)));
        assert!(service.take_rate_slot(public(5), Some(second)));
        assert!(service.take_rate_slot(public(6), None));
        assert!(service.take_rate_slot(public(7), None));
        service.rate.lock().unwrap().started = Instant::now() - Duration::from_secs(2);
        assert!(service.take_rate_slot(public(3), Some(first)));
        assert_eq!(service.rate.lock().unwrap().peers.len(), 1);
        assert_eq!(service.rate.lock().unwrap().sources.len(), 1);
        {
            let mut rate = service.rate.lock().unwrap();
            rate.started = Instant::now() + Duration::from_secs(60);
            rate.peers = (0..MAX_RATE_KEYS)
                .map(|i| {
                    let mut key = [0; 32];
                    key[..8].copy_from_slice(&(i as u64).to_be_bytes());
                    (key, 1)
                })
                .collect();
        }
        assert!(!service.take_rate_slot(public(8), None));
        assert_eq!(service.rate.lock().unwrap().peers.len(), MAX_RATE_KEYS);
        cleanup(service, path).await;
    }
}
