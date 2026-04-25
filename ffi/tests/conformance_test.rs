//! Cross-Language Conformance Tests (5b.7)
//!
//! Validates that Rust, Java, and Go FFI bindings all observe the same logical
//! event sequence when running the same scenario.
//!
//! ## Trace Format
//!
//! Each scenario is a JSON file with a sequence of entries:
//! ```json
//! {
//!   "name": "accept_connect_read_close",
//!   "entries": [
//!     { "seq": 0, "relative_time_ms": 0, "action": "SUBMIT", "op_id": 1, "op": "accept" },
//!     { "seq": 1, "relative_time_ms": 50, "action": "COMPLETE", "op_id": 1, "kind": "ACCEPTED" }
//!   ]
//! }
//! ```
//!
//! ## Conformance Invariants
//!
//! - **Exactly-once delivery**: Each operation produces at most one terminal event
//! - **Same logical sequence**: Java and Go must observe the same order of actions
//! - **Handle generation**: Old operations must not see new connection's events
//!
//! ## Test Cases
//!
//! | Scenario | Description |
//! |----------|-------------|
//! | `happy_path.json` | accept → connect → read → close |
//! | `cancel_before_accept.json` | submit accept → cancel → drain |
//! | `close_while_pending.json` | accept → read → close → ERROR |
//!
//! ## Note
//!
//! Full conformance testing requires actually running the scenarios through
//! the FFI, which needs the full async networking stack. These tests validate
//! the trace format and schema, and provide a framework for future conformance
//! testing via the Java and Go harnesses.

use std::fs;
use std::path::Path;

#[cfg(test)]
mod tests {
    use super::*;

    const CONFORMANCE_DIR: &str = "tests/conformance";

    fn load_scenario(name: &str) -> serde_json::Value {
        let path = Path::new(CONFORMANCE_DIR).join(format!("{}.json", name));
        let contents = fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("Failed to read {}: {}", path.display(), e));
        serde_json::from_str(&contents)
            .unwrap_or_else(|e| panic!("Failed to parse {}: {}", path.display(), e))
    }

    fn validate_trace_entry(entry: &serde_json::Value) -> Result<(), String> {
        // Required fields
        let seq = entry.get("seq").ok_or("missing 'seq'")?;
        if !seq.is_u64() {
            return Err("'seq' must be a non-negative integer".into());
        }

        let action = entry.get("action").ok_or("missing 'action'")?;
        let action_str = action.as_str().ok_or("'action' must be a string")?;
        let valid_actions = [
            "SUBMIT", "COMPLETE", "ERROR", "CANCEL", "RELEASE", "CLOSE", "POLL",
        ];
        if !valid_actions.contains(&action_str) {
            return Err(format!("Invalid action: {}", action_str));
        }

        let op_id = entry.get("op_id").ok_or("missing 'op_id'")?;
        if !op_id.is_u64() {
            return Err("'op_id' must be a non-negative integer".into());
        }

        Ok(())
    }

    fn validate_scenario(value: &serde_json::Value) -> Result<(), String> {
        let name = value
            .get("name")
            .and_then(|n| n.as_str())
            .ok_or("missing or invalid 'name'")?;
        println!("Validating scenario: {}", name);

        let entries = value
            .get("entries")
            .and_then(|e| e.as_array())
            .ok_or("missing or invalid 'entries' (expected array)")?;

        if entries.is_empty() {
            return Err("'entries' array is empty".into());
        }

        let mut prev_seq = -1i64;
        for entry in entries {
            validate_trace_entry(entry)?;

            let seq = entry.get("seq").and_then(|s| s.as_i64()).unwrap_or(-1);
            if seq <= prev_seq {
                return Err(format!(
                    "Sequence numbers must increase: {} <= {}",
                    seq, prev_seq
                ));
            }
            prev_seq = seq;
        }

        println!("  {} entries, sequence is valid", entries.len());
        Ok(())
    }

    #[test]
    fn test_schema_file_exists() {
        let schema_path = Path::new(CONFORMANCE_DIR).join("schema.json");
        assert!(
            schema_path.exists(),
            "Schema file should exist at {}",
            schema_path.display()
        );
    }

    #[test]
    fn test_happy_path_trace_valid() {
        let scenario = load_scenario("happy_path");
        validate_scenario(&scenario).expect("happy_path trace should be valid");
    }

    #[test]
    fn test_cancel_trace_valid() {
        let scenario = load_scenario("cancel_before_accept");
        validate_scenario(&scenario).expect("cancel trace should be valid");
    }

    #[test]
    fn test_close_while_pending_trace_valid() {
        let scenario = load_scenario("close_while_pending");
        validate_scenario(&scenario).expect("close_while_pending trace should be valid");
    }

    #[test]
    fn test_all_scenarios_have_entries() {
        let scenarios = ["happy_path", "cancel_before_accept", "close_while_pending"];
        for name in scenarios {
            let scenario = load_scenario(name);
            let entries = scenario
                .get("entries")
                .and_then(|e| e.as_array())
                .unwrap_or_else(|| panic!("{} should have entries array", name));
            assert!(
                !entries.is_empty(),
                "{} should have at least one entry",
                name
            );
        }
    }

    #[test]
    fn test_conformance_invariants_happy_path() {
        // Verify the happy path trace satisfies conformance invariants
        let scenario = load_scenario("happy_path");
        let entries = scenario.get("entries").unwrap().as_array().unwrap();

        let mut op_events: std::collections::HashMap<u64, Vec<&str>> =
            std::collections::HashMap::new();

        for entry in entries {
            let action = entry.get("action").unwrap().as_str().unwrap();
            let op_id = entry.get("op_id").unwrap().as_u64().unwrap();

            if action == "SUBMIT" || action == "COMPLETE" || action == "ERROR" || action == "CANCEL"
            {
                op_events.entry(op_id).or_default().push(action);
            }
        }

        // Each op should have at most one terminal event (COMPLETE, ERROR, or CANCEL)
        for (op_id, events) in &op_events {
            let terminals: Vec<_> = events.iter().filter(|&&e| e != "SUBMIT").collect();
            assert!(
                terminals.len() <= 1,
                "op_id {} has multiple terminal events: {:?}",
                op_id,
                terminals
            );
        }
    }

    #[test]
    fn test_conformance_invariants_cancel() {
        let scenario = load_scenario("cancel_before_accept");
        let entries = scenario.get("entries").unwrap().as_array().unwrap();

        // Find the CANCEL action and verify there's exactly one terminal event for that op
        let cancel_entries: Vec<_> = entries
            .iter()
            .filter(|e| e.get("action").unwrap().as_str() == Some("CANCEL"))
            .collect();

        if !cancel_entries.is_empty() {
            // If we have a CANCEL, we should see exactly one terminal event for that op
            let cancelled_op_id = cancel_entries[0].get("op_id").unwrap().as_u64().unwrap();

            // Terminal events are COMPLETE or ERROR actions (not CANCEL action)
            let terminal_events: Vec<_> = entries
                .iter()
                .filter(|e| {
                    let action = e.get("action").unwrap().as_str().unwrap();
                    let op_id = e.get("op_id").unwrap().as_u64().unwrap();
                    (action == "COMPLETE" || action == "ERROR") && op_id == cancelled_op_id
                })
                .collect();

            assert!(
                terminal_events.len() <= 1,
                "Cancelled op {} has multiple terminal events: {}",
                cancelled_op_id,
                terminal_events.len()
            );
        }
    }

    // ─── Java and Go conformance test placeholders ──────────────────────────
    //
    // These tests document the expected behavior for Java and Go bindings.
    // They are not yet implemented because they require running actual scenarios.
    //
    // To implement:
    // 1. Java: Add a JUnit test that loads the JSON, runs the scenario via FFI,
    //    and compares the observed trace against the golden trace
    // 2. Go: Add a Go test that does the same via cgo
    //
    // The key assertion is that ALL THREE (Rust, Java, Go) observe the same
    // sequence of actions for each operation.

    #[test]
    #[ignore = "requires Java FFI harness to run scenario"]
    fn test_java_conformance_happy_path() {
        // TODO: Load happy_path.json, execute via Java FFI, compare traces
        unimplemented!("Java conformance test requires Java test harness")
    }

    #[test]
    #[ignore = "requires Go FFI harness to run scenario"]
    fn test_go_conformance_happy_path() {
        // TODO: Load happy_path.json, execute via Go FFI, compare traces
        unimplemented!("Go conformance test requires Go test harness")
    }
}
