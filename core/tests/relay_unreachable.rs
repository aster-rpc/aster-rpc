//! Start-up is bounded when the home relay is unreachable: it fails with
//! `RelayOnlineTimeout` naming the relay, instead of waiting forever.
use aster_transport_core::test_support::wait_budget;
use aster_transport_core::{CoreEndpointConfig, CoreNetClient, CoreNode, RelayOnlineTimeout};
use std::net::TcpListener;
use std::time::Duration;

/// A node whose only relay is a closed localhost port. The configured bound is
/// short; `wait_budget()` only guards the test against a hang.
fn config() -> (CoreEndpointConfig, String) {
    let port = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let relay = format!("http://127.0.0.1:{port}");
    let config = CoreEndpointConfig {
        relay_mode: Some("custom".into()),
        relay_urls: vec![relay.clone()],
        bind_addr: Some("127.0.0.1:0".into()),
        discovery: Some("none".into()),
        portmapper_config: Some("disabled".into()),
        online_timeout_ms: 500,
        ..Default::default()
    };
    (config, relay)
}

fn assert_names_relay(err: &anyhow::Error, relay: &str) {
    let typed = err
        .downcast_ref::<RelayOnlineTimeout>()
        .unwrap_or_else(|| panic!("not a RelayOnlineTimeout: {err:#}"));
    assert_eq!(typed.timeout, Duration::from_millis(500));
    assert!(
        typed.relay_urls.iter().any(|u| u.starts_with(relay)),
        "relay urls {:?} should include {relay}",
        typed.relay_urls
    );
    assert!(err.to_string().contains(relay), "message: {err}");
}

#[tokio::test]
async fn node_start_fails_within_the_bound_on_an_unreachable_relay() {
    let (config, relay) = config();
    let started = tokio::time::timeout(
        wait_budget(),
        CoreNode::memory_with_alpns_and_gc(Vec::new(), Some(config), None),
    )
    .await
    .expect("start-up must be bounded by online_timeout_ms");
    assert_names_relay(&started.err().expect("start must fail"), &relay);
}

#[tokio::test]
async fn bare_endpoint_fails_within_the_bound_on_an_unreachable_relay() {
    let (config, relay) = config();
    let started = tokio::time::timeout(wait_budget(), CoreNetClient::create_with_config(config))
        .await
        .expect("start-up must be bounded by online_timeout_ms");
    assert_names_relay(&started.err().expect("start must fail"), &relay);
}

#[tokio::test]
async fn zero_timeout_is_rejected() {
    let (mut config, _) = config();
    config.online_timeout_ms = 0;
    let err = CoreNetClient::create_with_config(config)
        .await
        .err()
        .expect("zero must be rejected");
    assert!(err.to_string().contains("online_timeout_ms"), "{err}");
}
