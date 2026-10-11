use aster_transport_salvo::{generate_webtransport_cert, RawWtConfig, RawWtListener};
use std::{sync::Arc, time::Duration};
use wtransport::{ClientConfig, Endpoint, VarInt};

fn config() -> RawWtConfig {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let cert = generate_webtransport_cert("raw-test", None, &["127.0.0.1".into()]).unwrap();
    RawWtConfig::new("127.0.0.1:0", cert.cert_pem, cert.key_pem, "/media")
}
fn client() -> Endpoint<wtransport::endpoint::endpoint_side::Client> {
    Endpoint::client(
        ClientConfig::builder()
            .with_bind_default()
            .with_no_cert_validation()
            .build(),
    )
    .unwrap()
}

#[tokio::test]
async fn raw_bytes_bidirectional_unidirectional_datagrams_priority_ack_and_close() {
    tokio::time::timeout(Duration::from_secs(20), async {
        let mut cfg = config();
        cfg.max_read_bytes = 16 * 1024 * 1024;
        let listener = RawWtListener::start_public(cfg).await.unwrap();
        let endpoint = client();
        let peer = endpoint
            .connect(format!("https://{}/media", listener.quic_addr()))
            .await
            .unwrap();
        let session = listener.accept().await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(20), session.accept_bi())
                .await
                .is_err()
        );
        let (mut ps, mut pr) = peer.open_bi().await.unwrap().await.unwrap();
        ps.write_all(b"proof:unframed\0bytes").await.unwrap();
        let stream = session.accept_bi().await.unwrap();
        assert_eq!(stream.read(128).await.unwrap(), b"proof:unframed\0bytes");
        stream.set_priority(-20).await.unwrap();
        assert_eq!(stream.priority().await.unwrap(), Some(-20));
        stream.write(b"reply").await.unwrap();
        stream.finish().await.unwrap();
        let mut reply = [0; 5];
        pr.read_exact(&mut reply).await.unwrap();
        assert_eq!(&reply, b"reply");
        assert!(stream.transport_acked().await.unwrap());
        let mut uni = peer.open_uni().await.unwrap().await.unwrap();
        uni.write_all(b"incoming uni").await.unwrap();
        uni.finish().await.unwrap();
        let incoming = session.accept_uni().await.unwrap();
        assert!(!incoming.info().has_send);
        assert_eq!(incoming.read(128).await.unwrap(), b"incoming uni");
        assert!(incoming.read(128).await.unwrap().is_empty());
        let outgoing = session.open_uni().await.unwrap();
        outgoing.write(b"outgoing uni").await.unwrap();
        outgoing.finish().await.unwrap();
        let mut ur = peer.accept_uni().await.unwrap();
        let mut bytes = [0; 12];
        ur.read_exact(&mut bytes).await.unwrap();
        assert_eq!(&bytes, b"outgoing uni");
        assert!(outgoing.transport_acked().await.unwrap());
        let outgoing_bi = session.open_bi().await.unwrap();
        outgoing_bi.write(b"server bidi").await.unwrap();
        outgoing_bi.finish().await.unwrap();
        let (mut bs, mut br) = peer.accept_bi().await.unwrap();
        let mut bytes = [0; 11];
        br.read_exact(&mut bytes).await.unwrap();
        assert_eq!(&bytes, b"server bidi");
        bs.write_all(b"response").await.unwrap();
        bs.finish().await.unwrap();
        assert_eq!(outgoing_bi.read(128).await.unwrap(), b"response");
        peer.send_datagram(b"peer datagram").unwrap();
        assert_eq!(session.receive_datagram().await.unwrap(), b"peer datagram");
        session.send_datagram(b"server datagram").unwrap();
        assert_eq!(&*peer.receive_datagram().await.unwrap(), b"server datagram");
        assert!(session.send_datagram(&vec![0; 65536]).is_err());
        assert!(stream.read(0).await.is_err());
        assert!(session.stats().sent_packets > 0);
        assert!(!session.info().closed);
        let blocked = session.accept_uni();
        tokio::pin!(blocked);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut blocked)
                .await
                .is_err()
        );
        session.close(73, b"done").unwrap();
        assert!(blocked.await.is_err());
        assert!(matches!(
            peer.closed().await,
            wtransport::error::ConnectionError::ApplicationClosed(_)
        ));
        listener.stop(Duration::ZERO).await.unwrap();
        let _tcp = std::net::TcpListener::bind(listener.tcp_addr()).unwrap();
        let _udp = std::net::UdpSocket::bind(listener.quic_addr()).unwrap();
    })
    .await
    .unwrap();
}

struct Deny;
#[salvo::async_trait]
impl aster::rpc::Authenticator for Deny {
    async fn authenticate(
        &self,
        ctx: &aster::rpc::AuthContext,
    ) -> Result<aster::rpc::AuthOutcome, aster::rpc::RpcStatus> {
        assert_eq!(ctx.service, "webtransport");
        assert_eq!(ctx.method, "CONNECT");
        assert_eq!(ctx.metadata["aster.webtransport.path"], "/media");
        Err(aster::rpc::RpcStatus {
            code: 16,
            message: "denied".into(),
            detail_keys: vec![],
            detail_values: vec![],
        })
    }
}

struct Allow;
#[salvo::async_trait]
impl aster::rpc::Authenticator for Allow {
    async fn authenticate(
        &self,
        ctx: &aster::rpc::AuthContext,
    ) -> Result<aster::rpc::AuthOutcome, aster::rpc::RpcStatus> {
        assert_eq!(ctx.metadata["aster.webtransport.path"], "/media");
        assert_eq!(ctx.metadata["aster.webtransport.query"], "ticket=reserved");
        Ok(aster::rpc::AuthOutcome {
            principal: Some("verified-principal".into()),
            attributes: std::collections::HashMap::from([("role".into(), "media".into())]),
        })
    }
}
#[tokio::test]
async fn authenticated_identity_is_preserved_and_closed_owners_do_not_exhaust_admission() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut cfg = config();
        cfg.max_sessions = 1;
        let listener = RawWtListener::start_authenticated(cfg, Arc::new(Allow))
            .await
            .unwrap();
        let endpoint = client();
        let options = || {
            wtransport::endpoint::ConnectOptions::builder(format!(
                "https://{}/media?ticket=reserved",
                listener.quic_addr()
            ))
            .add_header("aster.webtransport.path", "spoofed")
            .build()
        };
        let peer = endpoint.connect(options()).await.unwrap();
        let first = listener.accept().await.unwrap();
        assert_eq!(
            first.info().principal.as_deref(),
            Some("verified-principal")
        );
        assert_eq!(first.info().attributes["role"], "media");
        first.close(7, b"closed but handle retained").unwrap();
        peer.closed().await;
        first.closed().await;
        let second_peer = endpoint.connect(options()).await.unwrap();
        let second = listener.accept().await.unwrap();
        assert!(!second.info().closed);
        let clone = second.clone();
        drop(second);
        assert!(!clone.info().closed);
        drop(clone);
        second_peer.closed().await;
        listener.stop(Duration::ZERO).await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn stop_pending_read_and_reset_blocked_write_preserve_application_codes() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut cfg = config();
        cfg.max_read_bytes = 16 * 1024 * 1024;
        let listener = RawWtListener::start_public(cfg).await.unwrap();
        let endpoint = client();
        let peer = endpoint
            .connect(format!("https://{}/media", listener.quic_addr()))
            .await
            .unwrap();
        let session = listener.accept().await.unwrap();
        let (mut ps, mut pr) = peer.open_bi().await.unwrap().await.unwrap();
        ps.write_all(b"x").await.unwrap();
        let stream = session.accept_bi().await.unwrap();
        assert_eq!(stream.read(1).await.unwrap(), b"x");
        let read = stream.read(1);
        tokio::pin!(read);
        assert!(tokio::time::timeout(Duration::from_millis(20), &mut read)
            .await
            .is_err());
        stream.stop(37).await.unwrap();
        assert!(read.await.is_err());
        assert!(format!("{:?}", ps.stopped().await).contains("37"));
        let data = vec![0; 16 * 1024 * 1024];
        let write = stream.write(&data);
        tokio::pin!(write);
        assert!(tokio::time::timeout(Duration::from_millis(20), &mut write)
            .await
            .is_err());
        stream.set_priority(20).await.unwrap();
        assert_eq!(stream.priority().await.unwrap(), Some(20));
        stream.reset(41).await.unwrap();
        assert!(write.await.is_err());
        let mut buf = [0; 65536];
        loop {
            match pr.read(&mut buf).await {
                Ok(Some(_)) => {}
                Err(e) => {
                    assert!(format!("{e:?}").contains("41"));
                    break;
                }
                other => panic!("unexpected read {other:?}"),
            }
        }
        assert!(!stream.transport_acked().await.unwrap());
        listener.stop(Duration::ZERO).await.unwrap();
    })
    .await
    .unwrap();
}
#[tokio::test]
async fn authentication_precedes_upgrade_and_listener_close_cancels_accept() {
    let listener = RawWtListener::start_authenticated(config(), Arc::new(Deny))
        .await
        .unwrap();
    let peer = client();
    assert!(peer
        .connect(format!("https://{}/media", listener.quic_addr()))
        .await
        .is_err());
    assert!(
        tokio::time::timeout(Duration::from_millis(20), listener.accept())
            .await
            .is_err()
    );
    listener.close();
    assert!(listener.accept().await.is_err());
    listener.stop(Duration::ZERO).await.unwrap();
}

#[tokio::test]
async fn listener_close_cancels_reads_and_peer_stop_unblocks_write() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut cfg = config();
        cfg.max_read_bytes = 16 * 1024 * 1024;
        let listener = RawWtListener::start_public(cfg).await.unwrap();
        let endpoint = client();
        let peer = endpoint
            .connect(format!("https://{}/media", listener.quic_addr()))
            .await
            .unwrap();
        let session = listener.accept().await.unwrap();
        let (mut ps, pr) = peer.open_bi().await.unwrap().await.unwrap();
        ps.write_all(b"x").await.unwrap();
        let stream = session.accept_bi().await.unwrap();
        assert_eq!(stream.read(1).await.unwrap(), b"x");
        let read = stream.read(1);
        tokio::pin!(read);
        assert!(tokio::time::timeout(Duration::from_millis(20), &mut read)
            .await
            .is_err());
        let bytes = vec![0; 16 * 1024 * 1024];
        let write = stream.write(&bytes);
        tokio::pin!(write);
        assert!(tokio::time::timeout(Duration::from_millis(20), &mut write)
            .await
            .is_err());
        pr.stop(VarInt::from_u32(99));
        assert!(write.await.is_err());
        listener.stop(Duration::ZERO).await.unwrap();
        assert!(read.await.is_err());
    })
    .await
    .unwrap();
}
