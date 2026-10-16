//! The .NET `Native_Addressed_Ticket_And_Explicit_Peers_Transfer_Updates`
//! sequence, under load: join (which imports *and starts a sync*, so a
//! connect is in flight), leave at once, start a sync with an explicit peer,
//! wait for the value, write again, wait, leave, and start a sync with no
//! peers. Several pairs run it at once, for several rounds.
//!
//! It once failed in the full local gate with `aster_doc_start_sync: channel
//! closed`, the error every docs call returns after the iroh-docs live actor
//! has stopped; the only runtime stop is a sync or download task that
//! panicked. A panic hook records every panic with its backtrace, and the test
//! fails if any task panicked, even one the actor survived.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use aster_transport_core::{CoreDoc, CoreEndpointConfig, CoreNode};

const PAIRS: usize = 6;
const ROUNDS: usize = 8;

async fn node() -> CoreNode {
    CoreNode::memory_with_alpns_and_gc(
        Vec::new(),
        Some(CoreEndpointConfig {
            relay_mode: Some("disabled".into()),
            ..Default::default()
        }),
        Some(Duration::from_secs(3600)),
    )
    .await
    .unwrap()
}

async fn wait_value(doc: &CoreDoc, author: &str, key: &[u8], want: &[u8]) {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            // An entry can arrive before its content; reading it then fails.
            let got = doc.get_exact(author.to_string(), key.to_vec()).await;
            if matches!(got, Ok(Some(ref value)) if value.as_slice() == want) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("value {want:?} never arrived"));
}

async fn pair(round: usize, provider: &CoreNode, consumer: &CoreNode) {
    let docs = provider.docs_client();
    let source = docs.create().await.unwrap();
    let author = docs.create_author().await.unwrap();
    let key = b"manifest/root".to_vec();
    source
        .set_bytes(author.clone(), key.clone(), b"first".to_vec())
        .await
        .unwrap();
    let ticket = source.share_with_addr("write".into()).await.unwrap();
    let joined = consumer.docs_client().join(ticket).await.unwrap();
    joined.leave().await.unwrap();
    consumer.add_node_addr(provider).unwrap();
    joined
        .start_sync(vec![provider.node_id()])
        .await
        .unwrap_or_else(|e| panic!("round {round}: start_sync after leave: {e:#}"));
    wait_value(&joined, &author, &key, b"first").await;
    source
        .set_bytes(author.clone(), key.clone(), b"second".to_vec())
        .await
        .unwrap();
    wait_value(&joined, &author, &key, b"second").await;
    joined.leave().await.unwrap();
    joined
        .start_sync(Vec::new())
        .await
        .unwrap_or_else(|e| panic!("round {round}: start_sync with no peers: {e:#}"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn join_leave_start_sync_under_load_never_panics_a_docs_task() {
    let panics: Arc<Mutex<Vec<String>>> = Arc::default();
    let previous = std::panic::take_hook();
    {
        let panics = panics.clone();
        std::panic::set_hook(Box::new(move |info| {
            let trace = std::backtrace::Backtrace::force_capture();
            panics
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(format!("{info}\n{trace}"));
        }));
    }

    let mut nodes = Vec::new();
    for _ in 0..PAIRS {
        nodes.push((node().await, node().await));
    }
    let nodes = Arc::new(nodes);
    let mut tasks = Vec::new();
    for i in 0..PAIRS {
        let nodes = nodes.clone();
        tasks.push(tokio::spawn(async move {
            let (provider, consumer) = &nodes[i];
            for round in 0..ROUNDS {
                pair(round, provider, consumer).await;
            }
        }));
    }
    let mut failures = Vec::new();
    for task in tasks {
        if let Err(e) = task.await {
            failures.push(e.to_string());
        }
    }
    for (a, b) in nodes.iter() {
        a.close().await;
        b.close().await;
    }
    std::panic::set_hook(previous);

    let panics = panics.lock().unwrap_or_else(|e| e.into_inner());
    assert!(
        panics.is_empty() && failures.is_empty(),
        "{} panic(s), {} failed pair(s): {failures:?}\n{}",
        panics.len(),
        failures.len(),
        panics.join("\n----\n")
    );
}
