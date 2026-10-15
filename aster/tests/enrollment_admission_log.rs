#![cfg(all(feature = "enrollment", feature = "docs"))]
//! Every Gate-0 decision Enrollment makes is one structured event under the
//! `aster::admission` target: allows at debug, denials at info, with the
//! peer, ALPN, decision, reason and (for an allow) the evidence source.
//!
//! Its own test binary: it installs a process-wide capturing subscriber.

use std::fmt;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aster::enrollment::*;
use aster::{AsterConfig, Node, RelayMode, SecretKey};

const FLEET: &str = "admission-log-test";
const APP_ALPN: &[u8] = b"admission-log/1";

/// One captured `aster::admission` event: its level and `name=value;` fields.
#[derive(Clone, Debug)]
struct Line {
    level: tracing::Level,
    fields: String,
}

#[derive(Clone)]
struct Capture(Arc<Mutex<Vec<Line>>>);

impl tracing::Subscriber for Capture {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        metadata.target() == "aster::admission"
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        struct Fields(String);
        impl tracing::field::Visit for Fields {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn fmt::Debug) {
                use std::fmt::Write;
                write!(self.0, "{}={value:?};", field.name()).unwrap();
            }
            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                use std::fmt::Write;
                write!(self.0, "{}={value};", field.name()).unwrap();
            }
        }
        let mut fields = Fields(String::new());
        event.record(&mut fields);
        self.0.lock().unwrap().push(Line {
            level: *event.metadata().level(),
            fields: fields.0,
        });
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

fn captured() -> &'static Arc<Mutex<Vec<Line>>> {
    static LINES: OnceLock<Arc<Mutex<Vec<Line>>>> = OnceLock::new();
    LINES.get_or_init(|| {
        let lines = Arc::new(Mutex::new(Vec::new()));
        tracing::subscriber::set_global_default(Capture(lines.clone())).unwrap();
        lines
    })
}

async fn wait_for_line(matches: impl Fn(&Line) -> bool) -> Line {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(line) = captured().lock().unwrap().iter().find(|l| matches(l)) {
                return line.clone();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the admission event was logged")
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}
fn key(seed: u8) -> SecretKey {
    SecretKey::from_bytes([seed; 32])
}
fn public(seed: u8) -> [u8; 32] {
    JwtEnrollmentIssuer::from_seed(&[seed; 32]).public_key()
}
fn credential(seed: u8) -> String {
    let request = EnrollmentIssueRequest::new(public(seed), FLEET, now() + 3600);
    issue_credential(&key(1), &request, now())
        .unwrap()
        .into_chain()
}

async fn node(seed: u8, hooks: bool) -> Node {
    let mut config = AsterConfig::builder()
        .relay(RelayMode::Disabled)
        .bind_addr("127.0.0.1:0")
        .secret_key(key(seed));
    if hooks {
        config = config
            .hooks(true)
            .hook_failure_mode(aster::HookFailureMode::FailClosed);
    }
    Node::start_with_alpns(
        config.build(),
        vec![ENROLLMENT_ALPN.to_vec(), APP_ALPN.to_vec()],
    )
    .await
    .unwrap()
}

fn enrollment(node: &Node, path: &std::path::Path, seed: u8) -> Enrollment {
    Enrollment::install(
        node,
        Arc::new(EnrollmentPolicyStore::open(path, public(1), [9; 32], FLEET).unwrap()),
        JwtEnrollmentVerifier::new(public(1), FLEET).unwrap(),
        Some(credential(seed)),
        Arc::new(PolicyExpressionV1::Predicate(
            PolicyPredicate::TolerateStalePolicy,
        )),
        EnrollmentOptions::default(),
    )
    .unwrap()
}

fn serve(node: Node, enrollment: Enrollment) {
    tokio::spawn(async move {
        while let Ok((alpn, connection)) = node.accept().await {
            if alpn == ENROLLMENT_ALPN {
                let enrollment = enrollment.clone();
                tokio::spawn(async move {
                    let _ = enrollment.accept(&connection).await;
                    connection.closed().await;
                });
            }
        }
    });
}

#[tokio::test]
async fn every_gate_zero_decision_is_logged_under_aster_admission() {
    captured();
    tokio::time::timeout(Duration::from_secs(60), async {
        let files = tempfile::tempdir().unwrap();
        let gate = node(2, true).await;
        let gate_enroll = enrollment(&gate, &files.path().join("gate.json"), 2);
        serve(gate.clone(), gate_enroll.clone());
        let gate_hex = hex::encode(public(2));

        // A stranger that never enrolled dials an application ALPN: denied
        // at info, naming the peer, the ALPN and why.
        let stranger = node(7, false).await;
        let stranger_hex = hex::encode(public(7));
        if let Ok(connection) = stranger.connect_addr(gate.addr(), APP_ALPN).await {
            connection.closed().await;
        }
        let denied = wait_for_line(|line| {
            line.fields.contains(&format!("peer={stranger_hex};"))
                && line.fields.contains("decision=deny;")
        })
        .await;
        assert_eq!(denied.level, tracing::Level::INFO, "{denied:?}");
        for field in [
            "alpn=admission-log/1;",
            "reason=no session;",
            "suppressed=0;",
        ] {
            assert!(denied.fields.contains(field), "{field} in {denied:?}");
        }

        // An enrolled peer: its enrollment connection and then the
        // application connection are each allowed at debug, the latter with
        // the evidence it rests on.
        let member = node(3, true).await;
        let member_enroll = enrollment(&member, &files.path().join("member.json"), 3);
        serve(member.clone(), member_enroll.clone());
        member_enroll.connect(gate.addr()).await.unwrap();
        let member_hex = hex::encode(public(3));
        let handshake = wait_for_line(|line| {
            line.fields.contains(&format!("peer={member_hex};"))
                && line.fields.contains("decision=allow;")
                && line.fields.contains("reason=enrollment handshake;")
        })
        .await;
        assert_eq!(handshake.level, tracing::Level::DEBUG);
        let connection = member.connect_addr(gate.addr(), APP_ALPN).await.unwrap();
        let admitted = wait_for_line(|line| {
            line.fields.contains(&format!("peer={member_hex};"))
                && line.fields.contains("alpn=admission-log/1;")
                && line.fields.contains("decision=allow;")
        })
        .await;
        assert_eq!(admitted.level, tracing::Level::DEBUG);
        assert!(
            admitted.fields.contains("source=credential;"),
            "{admitted:?}"
        );
        // The member's own Gate 0 logged its outbound side too.
        wait_for_line(|line| {
            line.fields.contains(&format!("peer={gate_hex};"))
                && line.fields.contains("alpn=admission-log/1;")
                && line.fields.contains("decision=allow;")
        })
        .await;
        drop(connection);

        // Repeated denials of one peer are demoted to debug and counted, so a
        // dialing loop cannot flood the info log.
        for _ in 0..3 {
            if let Ok(connection) = stranger.connect_addr(gate.addr(), APP_ALPN).await {
                connection.closed().await;
            }
        }
        let lines = captured().lock().unwrap().clone();
        let stranger_denials: Vec<_> = lines
            .iter()
            .filter(|line| {
                line.fields.contains(&format!("peer={stranger_hex};"))
                    && line.fields.contains("decision=deny;")
            })
            .collect();
        assert!(stranger_denials.len() >= 4, "{stranger_denials:?}");
        assert_eq!(
            stranger_denials
                .iter()
                .filter(|line| line.level == tracing::Level::INFO)
                .count(),
            1,
            "{stranger_denials:?}"
        );

        for node in [stranger, member, gate] {
            node.shutdown().await;
        }
    })
    .await
    .unwrap();
}
