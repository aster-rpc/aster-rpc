use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use bytes::Bytes;
use futures_lite::StreamExt;
use iroh::address_lookup::memory::MemoryLookup;
use iroh::endpoint::{presets, Connection, ConnectionError, Endpoint, RelayMode, VarInt};
use iroh::protocol::Router;
use iroh::{EndpointAddr, EndpointId, RelayUrl, SecretKey, TransportAddr};
use iroh_blobs::api::downloader::Downloader;
use iroh_blobs::api::Store as BlobStore;
use iroh_blobs::store::fs::FsStore;
use iroh_blobs::store::mem::MemStore;
use iroh_blobs::ticket::BlobTicket;
use iroh_blobs::{BlobFormat, BlobsProtocol, Hash, ALPN as BLOBS_ALPN};
use iroh_docs::api::protocol::{AddrInfoOptions, ShareMode};
use iroh_docs::api::Doc;
use iroh_docs::protocol::Docs;
use iroh_docs::{AuthorId, DocTicket, ALPN as DOCS_ALPN};
use iroh_gossip::api::{Event, GossipReceiver, GossipSender};
use iroh_gossip::net::Gossip;
use iroh_gossip::proto::TopicId;
use iroh_gossip::ALPN as GOSSIP_ALPN;
use iroh_tickets::Ticket;
use tokio::sync::Mutex;

#[derive(Clone, Debug)]
pub struct CoreNodeAddr {
    pub endpoint_id: String,
    pub relay_url: Option<String>,
    pub direct_addresses: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct CoreEndpointConfig {
    pub relay_mode: Option<String>,
    pub alpns: Vec<Vec<u8>>,
    pub secret_key: Option<Vec<u8>>,
}

#[derive(Clone, Debug)]
pub struct CoreClosedInfo {
    pub kind: String,
    pub code: Option<u64>,
    pub reason: Option<Vec<u8>>,
}

#[derive(Clone, Debug)]
pub struct CoreGossipEvent {
    pub event_type: String,
    pub data: Option<Vec<u8>>,
}

fn u64_to_varint(value: u64) -> Result<VarInt> {
    VarInt::try_from(value).map_err(Into::into)
}

pub fn endpoint_addr_to_core(addr: EndpointAddr) -> CoreNodeAddr {
    CoreNodeAddr {
        endpoint_id: addr.id.to_string(),
        relay_url: addr.relay_urls().next().map(|url| url.to_string()),
        direct_addresses: addr.ip_addrs().map(|addr| addr.to_string()).collect(),
    }
}

pub fn core_to_endpoint_addr(addr: &CoreNodeAddr) -> Result<EndpointAddr> {
    let id: EndpointId = addr.endpoint_id.parse()?;
    let mut addrs: Vec<TransportAddr> = addr
        .direct_addresses
        .iter()
        .map(|addr| addr.parse::<SocketAddr>().map(TransportAddr::Ip))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if let Some(relay_url) = &addr.relay_url {
        addrs.push(TransportAddr::Relay(relay_url.parse::<RelayUrl>()?));
    }
    Ok(EndpointAddr::from_parts(id, addrs))
}

fn relay_mode_from_config(config: &CoreEndpointConfig) -> Result<RelayMode> {
    match config.relay_mode.as_deref() {
        None | Some("default") => Ok(RelayMode::Default),
        Some("disabled") => Ok(RelayMode::Disabled),
        Some("staging") => Ok(RelayMode::Staging),
        Some(other) => Err(anyhow!("unsupported relay_mode: {other}")),
    }
}

fn build_endpoint_config(config: CoreEndpointConfig) -> Result<iroh::endpoint::Builder> {
    let relay_mode = relay_mode_from_config(&config)?;
    let mut builder = Endpoint::builder(presets::N0)
        .alpns(config.alpns)
        .relay_mode(relay_mode);
    if let Some(secret_key) = config.secret_key {
        let bytes: [u8; 32] = secret_key
            .try_into()
            .map_err(|_| anyhow!("secret_key must be exactly 32 bytes"))?;
        builder = builder.secret_key(SecretKey::from_bytes(&bytes));
    }
    Ok(builder)
}

pub struct CoreNode {
    pub endpoint: Endpoint,
    #[allow(dead_code)]
    pub router: Router,
    pub blobs: BlobsProtocol,
    pub docs: Docs,
    pub gossip: Gossip,
    pub store: BlobStore,
}

impl CoreNode {
    pub async fn memory() -> Result<Self> {
        let endpoint = Endpoint::bind(presets::N0).await?;
        endpoint.online().await;
        let mem_store = MemStore::new();
        let store: BlobStore = (*mem_store).clone();
        let blobs = BlobsProtocol::new(&store, None);
        let gossip = Gossip::builder().spawn(endpoint.clone());
        let docs = Docs::memory()
            .spawn(endpoint.clone(), store.clone(), gossip.clone())
            .await?;
        let router = Router::builder(endpoint.clone())
            .accept(BLOBS_ALPN, blobs.clone())
            .accept(GOSSIP_ALPN, gossip.clone())
            .accept(DOCS_ALPN, docs.clone())
            .spawn();
        Ok(Self { endpoint, router, blobs, docs, gossip, store })
    }

    pub async fn persistent(path: String) -> Result<Self> {
        let endpoint = Endpoint::bind(presets::N0).await?;
        endpoint.online().await;
        let fs_store = FsStore::load(path).await?;
        let store: BlobStore = fs_store.into();
        let blobs = BlobsProtocol::new(&store, None);
        let gossip = Gossip::builder().spawn(endpoint.clone());
        let docs = Docs::memory()
            .spawn(endpoint.clone(), store.clone(), gossip.clone())
            .await?;
        let router = Router::builder(endpoint.clone())
            .accept(BLOBS_ALPN, blobs.clone())
            .accept(GOSSIP_ALPN, gossip.clone())
            .accept(DOCS_ALPN, docs.clone())
            .spawn();
        Ok(Self { endpoint, router, blobs, docs, gossip, store })
    }

    pub fn node_id(&self) -> String { self.endpoint.id().to_string() }
    pub fn node_addr_info(&self) -> CoreNodeAddr { endpoint_addr_to_core(self.endpoint.addr()) }
    pub fn node_addr_debug(&self) -> String { format!("{:?}", self.endpoint.addr()) }
    pub async fn close(&self) { self.endpoint.close().await; }

    pub fn add_node_addr(&self, other: &CoreNode) -> Result<()> {
        let addr = other.endpoint.addr();
        let memory_lookup = MemoryLookup::new();
        memory_lookup.add_endpoint_info(addr);
        self.endpoint.address_lookup()?.add(memory_lookup);
        Ok(())
    }

    pub fn blobs_client(&self) -> CoreBlobsClient {
        CoreBlobsClient { store: self.store.clone(), endpoint: self.endpoint.clone() }
    }
    pub fn docs_client(&self) -> CoreDocsClient {
        CoreDocsClient { inner: self.docs.clone(), store: self.store.clone(), endpoint: self.endpoint.clone() }
    }
    pub fn gossip_client(&self) -> CoreGossipClient { CoreGossipClient { inner: self.gossip.clone() } }
    pub fn net_client(&self) -> CoreNetClient { CoreNetClient { endpoint: self.endpoint.clone() } }
}

pub struct CoreNetClient { pub endpoint: Endpoint }

impl CoreNetClient {
    pub async fn create(alpn: Vec<u8>) -> Result<Self> {
        let endpoint = Endpoint::builder(presets::N0).alpns(vec![alpn]).bind().await?;
        endpoint.online().await;
        Ok(Self { endpoint })
    }
    pub async fn create_with_config(config: CoreEndpointConfig) -> Result<Self> {
        let relay_mode = relay_mode_from_config(&config)?;
        let endpoint = build_endpoint_config(config)?.bind().await?;
        if !matches!(relay_mode, RelayMode::Disabled) { endpoint.online().await; }
        Ok(Self { endpoint })
    }
    pub async fn connect(&self, node_id: String, alpn: Vec<u8>) -> Result<CoreConnection> {
        let id: EndpointId = node_id.parse()?;
        Ok(CoreConnection { inner: self.endpoint.connect(id, &alpn).await? })
    }
    pub async fn connect_node_addr(&self, addr: CoreNodeAddr, alpn: Vec<u8>) -> Result<CoreConnection> {
        Ok(CoreConnection { inner: self.endpoint.connect(core_to_endpoint_addr(&addr)?, &alpn).await? })
    }
    pub async fn accept(&self) -> Result<CoreConnection> {
        let incoming = self.endpoint.accept().await.ok_or_else(|| anyhow!("endpoint closed, no incoming connection"))?;
        Ok(CoreConnection { inner: incoming.accept()?.await? })
    }
    pub fn endpoint_id(&self) -> String { self.endpoint.id().to_string() }
    pub fn endpoint_addr_debug(&self) -> String { format!("{:?}", self.endpoint.addr()) }
    pub fn endpoint_addr_info(&self) -> CoreNodeAddr { endpoint_addr_to_core(self.endpoint.addr()) }
    pub async fn close(&self) { self.endpoint.close().await; }
    pub async fn closed(&self) { self.endpoint.closed().await; }
}

pub struct CoreConnection { pub inner: Connection }

impl CoreConnection {
    pub async fn open_bi(&self) -> Result<(CoreSendStream, CoreRecvStream)> {
        let (send, recv) = self.inner.open_bi().await?;
        Ok((CoreSendStream { inner: Arc::new(Mutex::new(send)) }, CoreRecvStream { inner: Arc::new(Mutex::new(recv)) }))
    }
    pub async fn accept_bi(&self) -> Result<(CoreSendStream, CoreRecvStream)> {
        let (send, recv) = self.inner.accept_bi().await?;
        Ok((CoreSendStream { inner: Arc::new(Mutex::new(send)) }, CoreRecvStream { inner: Arc::new(Mutex::new(recv)) }))
    }
    pub async fn open_uni(&self) -> Result<CoreSendStream> {
        Ok(CoreSendStream { inner: Arc::new(Mutex::new(self.inner.open_uni().await?)) })
    }
    pub async fn accept_uni(&self) -> Result<CoreRecvStream> {
        Ok(CoreRecvStream { inner: Arc::new(Mutex::new(self.inner.accept_uni().await?)) })
    }
    pub fn send_datagram(&self, data: Vec<u8>) -> Result<()> { self.inner.send_datagram(Bytes::from(data))?; Ok(()) }
    pub async fn read_datagram(&self) -> Result<Vec<u8>> { Ok(self.inner.read_datagram().await?.to_vec()) }
    pub fn remote_id(&self) -> String { self.inner.remote_id().to_string() }
    pub fn close(&self, code: u64, reason: Vec<u8>) -> Result<()> { self.inner.close(u64_to_varint(code)?, &reason); Ok(()) }
    pub async fn closed(&self) -> CoreClosedInfo {
        let closed = self.inner.closed().await;
        let (code, reason) = match &closed {
            ConnectionError::ApplicationClosed(app) => (Some(u64::from(app.error_code.into_inner())), Some(app.reason.to_vec())),
            _ => (None, Some(closed.to_string().into_bytes())),
        };
        CoreClosedInfo { kind: format!("{closed:?}"), code, reason }
    }
}

pub struct CoreSendStream { pub inner: Arc<Mutex<iroh::endpoint::SendStream>> }
impl CoreSendStream {
    pub async fn write_all(&self, data: Vec<u8>) -> Result<()> { let mut s = self.inner.lock().await; s.write_all(&data).await?; Ok(()) }
    pub async fn finish(&self) -> Result<()> { let mut s = self.inner.lock().await; s.finish()?; Ok(()) }
    pub async fn stopped(&self) -> Result<Option<u64>> { let s = &mut *self.inner.lock().await; Ok(s.stopped().await?.map(|v| u64::from(v.into_inner()))) }
}

pub struct CoreRecvStream { pub inner: Arc<Mutex<iroh::endpoint::RecvStream>> }
impl CoreRecvStream {
    pub async fn read(&self, max_len: usize) -> Result<Option<Vec<u8>>> { let mut s = self.inner.lock().await; Ok(s.read_chunk(max_len).await?.map(|c| c.bytes.to_vec())) }
    pub async fn read_exact(&self, n: usize) -> Result<Vec<u8>> { let mut s = self.inner.lock().await; let mut buf = vec![0u8; n]; s.read_exact(&mut buf).await?; Ok(buf) }
    pub async fn read_to_end(&self, max_size: usize) -> Result<Vec<u8>> { let mut s = self.inner.lock().await; Ok(s.read_to_end(max_size).await?.to_vec()) }
    pub fn stop(&self, code: u64) -> Result<()> { let mut s = self.inner.try_lock().map_err(|_| anyhow!("recv stream is busy"))?; s.stop(u64_to_varint(code)?)?; Ok(()) }
}

pub struct CoreBlobsClient { pub store: BlobStore, pub endpoint: Endpoint }
impl CoreBlobsClient {
    pub async fn add_bytes(&self, data: Vec<u8>) -> Result<String> { Ok(self.store.add_slice(&data).await?.hash.to_string()) }
    pub async fn read_to_bytes(&self, hash_hex: String) -> Result<Vec<u8>> { Ok(self.store.get_bytes(hash_hex.parse::<Hash>()?).await?.to_vec()) }
    pub fn create_ticket(&self, hash_hex: String) -> Result<String> { Ok(BlobTicket::new(self.endpoint.addr(), hash_hex.parse::<Hash>()?, BlobFormat::Raw).serialize()) }
    pub async fn download_blob(&self, ticket_str: String) -> Result<Vec<u8>> {
        let ticket = BlobTicket::deserialize(&ticket_str)?;
        let hash = ticket.hash();
        let (addr, _, _) = ticket.into_parts();
        if let Ok(lookup) = self.endpoint.address_lookup() {
            let mem = MemoryLookup::new();
            mem.add_endpoint_info(addr.clone());
            lookup.add(mem);
        }
        Downloader::new(&self.store, &self.endpoint).download(hash, vec![addr.id]).await?;
        Ok(self.store.get_bytes(hash).await?.to_vec())
    }
}

pub struct CoreDocsClient { pub inner: Docs, pub store: BlobStore, pub endpoint: Endpoint }
impl CoreDocsClient {
    pub async fn create(&self) -> Result<CoreDoc> { Ok(CoreDoc { doc: self.inner.api().create().await?, store: self.store.clone() }) }
    pub async fn create_author(&self) -> Result<String> { Ok(self.inner.api().author_create().await?.to_string()) }
    pub async fn join(&self, ticket_str: String) -> Result<CoreDoc> {
        let ticket = DocTicket::deserialize(&ticket_str)?;
        if let Ok(lookup) = self.endpoint.address_lookup() {
            for node_addr in &ticket.nodes {
                let mem = MemoryLookup::new();
                mem.add_endpoint_info(node_addr.clone());
                lookup.add(mem);
            }
        }
        Ok(CoreDoc { doc: self.inner.api().import_namespace(ticket.capability).await?, store: self.store.clone() })
    }
}

pub struct CoreDoc { pub doc: Doc, pub store: BlobStore }
impl CoreDoc {
    pub fn doc_id(&self) -> String { self.doc.id().to_string() }
    pub async fn set_bytes(&self, author_hex: String, key: Vec<u8>, value: Vec<u8>) -> Result<String> {
        let author_id: AuthorId = author_hex.parse()?;
        Ok(self.doc.set_bytes(author_id, Bytes::from(key), Bytes::from(value)).await?.to_hex().to_string())
    }
    pub async fn get_exact(&self, author_hex: String, key: Vec<u8>) -> Result<Option<Vec<u8>>> {
        let author_id: AuthorId = author_hex.parse()?;
        match self.doc.get_exact(author_id, key, false).await? {
            Some(entry) => Ok(Some(self.store.get_bytes(entry.content_hash()).await?.to_vec())),
            None => Ok(None),
        }
    }
    pub async fn share(&self, mode: String) -> Result<String> {
        let share_mode = match mode.as_str() {
            "read" | "Read" => ShareMode::Read,
            "write" | "Write" => ShareMode::Write,
            _ => return Err(anyhow!("mode must be 'read' or 'write'")),
        };
        Ok(self.doc.share(share_mode, AddrInfoOptions::Id).await?.serialize())
    }
}

pub struct CoreGossipClient { pub inner: Gossip }
impl CoreGossipClient {
    pub async fn subscribe(&self, topic_bytes: Vec<u8>, bootstrap_peers: Vec<String>) -> Result<CoreGossipTopic> {
        let topic_arr: [u8; 32] = topic_bytes.try_into().map_err(|_| anyhow!("topic_bytes must be exactly 32 bytes"))?;
        let peers: Vec<EndpointId> = bootstrap_peers.iter().map(|s| s.parse::<EndpointId>()).collect::<std::result::Result<Vec<_>, _>>()?;
        let topic = self.inner.subscribe_and_join(TopicId::from_bytes(topic_arr), peers).await?;
        let (sender, receiver) = topic.split();
        Ok(CoreGossipTopic { sender, receiver: Arc::new(Mutex::new(receiver)) })
    }
}

pub struct CoreGossipTopic { pub sender: GossipSender, pub receiver: Arc<Mutex<GossipReceiver>> }
impl CoreGossipTopic {
    pub async fn broadcast(&self, data: Vec<u8>) -> Result<()> { self.sender.broadcast(Bytes::from(data)).await?; Ok(()) }
    pub async fn recv(&self) -> Result<CoreGossipEvent> {
        let mut rx = self.receiver.lock().await;
        use std::pin::Pin;
        let event = Pin::new(&mut *rx).next().await.ok_or_else(|| anyhow!("gossip topic closed"))??;
        Ok(match event {
            Event::Received(msg) => CoreGossipEvent { event_type: "received".into(), data: Some(msg.content.to_vec()) },
            Event::NeighborUp(id) => CoreGossipEvent { event_type: "neighbor_up".into(), data: Some(id.to_string().into_bytes()) },
            Event::NeighborDown(id) => CoreGossipEvent { event_type: "neighbor_down".into(), data: Some(id.to_string().into_bytes()) },
            Event::Lagged => CoreGossipEvent { event_type: "lagged".into(), data: None },
        })
    }
}