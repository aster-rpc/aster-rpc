//! Direct tests of the **production** `Dispatcher::frame_limit_resolver`.
//!
//! The core-side tests use a hand-written closure, so they prove the reactor
//! honours a resolver -- not that the resolver Aster actually installs returns
//! the right thing. In particular, session-scoped services register a factory
//! rather than an instance, so a resolver consulting only the shared registry
//! silently leaves them on the 16 MiB default.
#![cfg(feature = "rpc")]

use aster::rpc::StreamHeader;
use aster::rpc::{Server, ServiceDispatch};
use aster::{AsterConfig, Node, RelayMode};
use aster_transport_core::reactor::FrameLimit;

fn header(service: &str, version: i32, method: &str) -> Vec<u8> {
    aster::rpc::encode_stream_header(&StreamHeader {
        service: service.to_string(),
        version,
        method: method.to_string(),
        ..Default::default()
    })
    .expect("encode header")
}

struct Shared;
#[async_trait::async_trait]
impl ServiceDispatch for Shared {
    fn name(&self) -> &str {
        "test.Shared"
    }
    fn version(&self) -> i32 {
        1
    }
    fn max_frame_size(&self, method: &str) -> Option<u32> {
        match method {
            "small" => Some(4 * 1024),
            "big" => Some(8 * 1024 * 1024),
            _ => None,
        }
    }
    async fn dispatch(&self, _m: &str, _c: aster::rpc::Call) {}
}

#[derive(Default)]
struct Scoped;
#[async_trait::async_trait]
impl ServiceDispatch for Scoped {
    fn name(&self) -> &str {
        "test.Scoped"
    }
    fn version(&self) -> i32 {
        1
    }
    fn max_frame_size(&self, method: &str) -> Option<u32> {
        (method == "capped").then_some(2 * 1024)
    }
    async fn dispatch(&self, _m: &str, _c: aster::rpc::Call) {}
}

fn cfg() -> AsterConfig {
    AsterConfig::builder()
        .relay(RelayMode::Disabled)
        .bind_addr("127.0.0.1:0")
        .build()
}

async fn resolver_for_both() -> (Node, aster_transport_core::reactor::FrameLimitResolver) {
    let node = Node::start(cfg()).await.unwrap();
    let resolver = Server::new(&node)
        .register(Shared)
        .register_session(Scoped::default)
        .dispatcher()
        .frame_limit_resolver();
    (node, resolver)
}

#[tokio::test]
async fn shared_service_methods_resolve_to_their_declared_caps() {
    let (_n, r) = resolver_for_both().await;
    assert!(matches!(
        r(&header("test.Shared", 1, "small")),
        FrameLimit::Limit(4096)
    ));
    assert!(matches!(
        r(&header("test.Shared", 1, "big")),
        FrameLimit::Limit(8_388_608)
    ));
    // A method with no declared cap keeps the global default.
    assert!(matches!(
        r(&header("test.Shared", 1, "other")),
        FrameLimit::Default
    ));
}

/// The finding: session-scoped services are registered as factories, so a
/// resolver reading only the shared registry never sees them.
#[tokio::test]
async fn session_scoped_service_methods_also_resolve() {
    let (_n, r) = resolver_for_both().await;
    assert!(
        matches!(
            r(&header("test.Scoped", 1, "capped")),
            FrameLimit::Limit(2048)
        ),
        "a session-scoped service must not silently keep the 16 MiB default"
    );
    assert!(matches!(
        r(&header("test.Scoped", 1, "uncapped")),
        FrameLimit::Default
    ));
}

#[tokio::test]
async fn unknown_service_and_wrong_version_fall_back_to_default() {
    let (_n, r) = resolver_for_both().await;
    assert!(matches!(
        r(&header("nope.Thing", 1, "m")),
        FrameLimit::Default
    ));
    // Same name, different version is a different service.
    assert!(matches!(
        r(&header("test.Shared", 2, "small")),
        FrameLimit::Default
    ));
}

#[tokio::test]
async fn undecodable_header_is_rejected_not_defaulted() {
    let (_n, r) = resolver_for_both().await;
    assert!(
        matches!(r(b"\xff\xff not a header"), FrameLimit::Reject),
        "a header that does not decode must reject, not fall back to a \
         generous default with a body behind it"
    );
}
