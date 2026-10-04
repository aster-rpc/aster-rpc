//! Source-level guards over the **whole** coordination subsystem.
//!
//! ## Why these are not per-module
//!
//! `storage::durability_guard` proved the shape: a rule about where a
//! construct may appear cannot be checked at runtime, and a guard that scans a
//! hand-picked file list is exempt from its own rule the moment someone adds a
//! file. Slice 3's first attempt at these guards scanned four files each; the
//! Raft service handler — the one place a second `install_full_snapshot` call
//! could plausibly appear — was in none of them.
//!
//! So this module owns one inventory of **every** `.rs` file under
//! `src/coordination/`, checked against the filesystem, and every cross-cutting
//! guard reads it. A new source that nobody adds here fails
//! [`every_coordination_source_is_inventoried`] rather than quietly escaping.
//!
//! ## Production is not "before the last test module"
//!
//! `production_end` cuts a file at its last `#[cfg(test)] mod`, which is right
//! for what is *scanned* and wrong as a definition of what exists: production
//! code appended **after** that module compiles, ships, and sits outside every
//! guard. `storage` guards that boundary for its own tree; this guards it for
//! the rest of the subsystem.

/// Every `.rs` file beneath `src/coordination/`, with its text.
///
/// Hand-written, and cross-checked against the filesystem by
/// [`every_coordination_source_is_inventoried`]. The check is what makes the
/// list trustworthy — the list on its own is a promise.
#[cfg(test)]
pub(crate) const SOURCES: &[(&str, &str)] = &[
    ("admission.rs", include_str!("admission.rs")),
    ("boot.rs", include_str!("boot.rs")),
    ("client/driver.rs", include_str!("client/driver.rs")),
    ("client/engine.rs", include_str!("client/engine.rs")),
    ("client/error.rs", include_str!("client/error.rs")),
    ("client/inbound.rs", include_str!("client/inbound.rs")),
    ("client/journal.rs", include_str!("client/journal.rs")),
    ("client/lease.rs", include_str!("client/lease.rs")),
    ("client/mod.rs", include_str!("client/mod.rs")),
    ("client/manager.rs", include_str!("client/manager.rs")),
    ("client/placement.rs", include_str!("client/placement.rs")),
    ("contract.rs", include_str!("contract.rs")),
    ("convert.rs", include_str!("convert.rs")),
    ("guards.rs", include_str!("guards.rs")),
    ("hash.rs", include_str!("hash.rs")),
    ("host.rs", include_str!("host.rs")),
    ("ids.rs", include_str!("ids.rs")),
    ("mod.rs", include_str!("mod.rs")),
    ("raft.rs", include_str!("raft.rs")),
    ("reason.rs", include_str!("reason.rs")),
    ("rowstream.rs", include_str!("rowstream.rs")),
    ("service.rs", include_str!("service.rs")),
    ("start.rs", include_str!("start.rs")),
    // Inventoried unconditionally, though it compiles only under
    // `coordination-testing`. The inventory is about the *filesystem*: a file
    // exempt from the guards while the feature is off is a file that could grow
    // a second `install_full_snapshot` call site unobserved, and be turned on
    // by a later profile.
    ("testing.rs", include_str!("testing.rs")),
    ("typeconfig.rs", include_str!("typeconfig.rs")),
    ("model/adapter.rs", include_str!("model/adapter.rs")),
    ("model/admin.rs", include_str!("model/admin.rs")),
    ("model/apply.rs", include_str!("model/apply.rs")),
    ("model/authority.rs", include_str!("model/authority.rs")),
    ("model/boundary.rs", include_str!("model/boundary.rs")),
    ("model/classify.rs", include_str!("model/classify.rs")),
    ("model/expire.rs", include_str!("model/expire.rs")),
    ("model/grant.rs", include_str!("model/grant.rs")),
    ("model/history.rs", include_str!("model/history.rs")),
    ("model/limits.rs", include_str!("model/limits.rs")),
    ("model/membership.rs", include_str!("model/membership.rs")),
    ("model/mod.rs", include_str!("model/mod.rs")),
    ("model/project.rs", include_str!("model/project.rs")),
    ("model/resource.rs", include_str!("model/resource.rs")),
    ("model/retire.rs", include_str!("model/retire.rs")),
    ("model/shape.rs", include_str!("model/shape.rs")),
    ("model/state.rs", include_str!("model/state.rs")),
    ("model/tables.rs", include_str!("model/tables.rs")),
    ("model/wire.rs", include_str!("model/wire.rs")),
    ("network/admit.rs", include_str!("network/admit.rs")),
    ("network/chaos.rs", include_str!("network/chaos.rs")),
    ("network/client.rs", include_str!("network/client.rs")),
    ("network/errors.rs", include_str!("network/errors.rs")),
    ("network/handoff.rs", include_str!("network/handoff.rs")),
    ("network/mod.rs", include_str!("network/mod.rs")),
    ("network/service.rs", include_str!("network/service.rs")),
    ("runtime/config.rs", include_str!("runtime/config.rs")),
    ("runtime/expiry.rs", include_str!("runtime/expiry.rs")),
    ("runtime/lifecycle.rs", include_str!("runtime/lifecycle.rs")),
    (
        "runtime/membership.rs",
        include_str!("runtime/membership.rs"),
    ),
    ("runtime/mod.rs", include_str!("runtime/mod.rs")),
    ("runtime/owner.rs", include_str!("runtime/owner.rs")),
    ("runtime/pressure.rs", include_str!("runtime/pressure.rs")),
    ("runtime/readiness.rs", include_str!("runtime/readiness.rs")),
    ("storage/actor.rs", include_str!("storage/actor.rs")),
    ("storage/codec.rs", include_str!("storage/codec.rs")),
    ("storage/bench.rs", include_str!("storage/bench.rs")),
    ("storage/crash.rs", include_str!("storage/crash.rs")),
    ("storage/error.rs", include_str!("storage/error.rs")),
    ("storage/fixture.rs", include_str!("storage/fixture.rs")),
    ("storage/identity.rs", include_str!("storage/identity.rs")),
    ("storage/import.rs", include_str!("storage/import.rs")),
    ("storage/log.rs", include_str!("storage/log.rs")),
    ("storage/machine.rs", include_str!("storage/machine.rs")),
    ("storage/mod.rs", include_str!("storage/mod.rs")),
    ("storage/reads.rs", include_str!("storage/reads.rs")),
    ("storage/rows.rs", include_str!("storage/rows.rs")),
    ("storage/scratch.rs", include_str!("storage/scratch.rs")),
    ("storage/schema.rs", include_str!("storage/schema.rs")),
    ("storage/serve.rs", include_str!("storage/serve.rs")),
    ("storage/snapshot.rs", include_str!("storage/snapshot.rs")),
    ("store/host.rs", include_str!("store/host.rs")),
    ("store/methods.rs", include_str!("store/methods.rs")),
    ("store/mod.rs", include_str!("store/mod.rs")),
    ("store/wire.rs", include_str!("store/wire.rs")),
];

/// Where a file's production code ends: at its **last** `#[cfg(test)]` module.
///
/// Not the first: a file may carry a test-only seam long before its test
/// module, and cutting there narrows every guard below it to a fraction of the
/// file. Not the *name* `tests` either, since these guard modules are not
/// called that.
#[cfg(test)]
pub(crate) fn production_end(source: &str) -> usize {
    source
        .rfind("\n#[cfg(test)]\nmod ")
        .map(|at| at + 1)
        .unwrap_or(source.len())
}

/// Production source lines, with 1-based line numbers, comments stripped.
///
/// Comments are stripped because prose that *names* a forbidden construct in
/// order to explain why it is forbidden is not a use of it — and this
/// subsystem's comments name them constantly.
#[cfg(test)]
pub(crate) fn production_code(source: &str) -> impl Iterator<Item = (usize, &str)> {
    source[..production_end(source)]
        .lines()
        .enumerate()
        .map(|(n, line)| (n + 1, line.split("//").next().unwrap_or("")))
}

/// Whether a line **builds** a value of type `name`.
///
/// `Foo {` appears in four unrelated shapes — `struct Foo {`, `impl Foo {`,
/// `fn f() -> &Foo {`, and `let Foo { .. } = x` — and the first version of
/// these guards counted all of them, reporting three construction sites for a
/// type built once. Only a struct literal obtains a value; a declaration, an
/// impl block, a return type and a destructuring pattern do not.
#[cfg(test)]
pub(crate) fn builds(line: &str, name: &str) -> bool {
    let trimmed = line.trim_start();
    let declaration = trimmed.starts_with("impl")
        || trimmed.contains("struct ")
        || trimmed.contains("fn ")
        || trimmed.starts_with("let ");
    !declaration && line.contains(&format!("{name} {{"))
}

/// Whether a line **destructures** a value of type `name`.
///
/// The counterpart to [`builds`], and counted separately so each guard states
/// "built once, consumed once" rather than "mentioned twice" — which two
/// constructions and no consumer would also satisfy.
#[cfg(test)]
pub(crate) fn destructures(line: &str, name: &str) -> bool {
    line.trim_start().starts_with("let ") && line.contains(&format!("{name} {{"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// The inventory is the directory.
    ///
    /// Compared against the **filesystem** rather than against `mod`
    /// declarations parsed out of the source, for the reason
    /// `storage::durability_guard` records at length: four versions of that
    /// parser were four different wrong answers, and the set of files in a
    /// directory is not a question that needs a Rust lexer.
    #[test]
    fn every_coordination_source_is_inventoried() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/coordination");
        let mut found = Vec::new();
        walk(&dir, "", &mut found);

        let on_disk: BTreeSet<String> = found.into_iter().collect();
        let inventoried: BTreeSet<String> = SOURCES
            .iter()
            .map(|(name, _)| (*name).to_string())
            .collect();

        assert_eq!(
            on_disk,
            inventoried,
            "every .rs file under {} must be in SOURCES, and nothing else; a source \
             missing here is exempt from every guard in this module",
            dir.display()
        );
    }

    /// Every inventoried source is LF-terminated text.
    ///
    /// The guards in this module and in `storage` read their sources through
    /// `include_str!` and cut each file at a literal `"\n#[cfg(test)]\n"`.
    /// A checkout that converts to CRLF hides that marker, every test module
    /// is scanned as production, and twenty-eight guards fail at once with
    /// messages about duplicate constructors — which is what the first native
    /// Windows run produced under Git for Windows' default
    /// `core.autocrlf=true`. `.gitattributes` pins LF for every text file so a
    /// fresh clone cannot reach that state; this is the one failure that
    /// names the cause if a checkout predates the attribute.
    #[test]
    fn every_inventoried_source_is_checked_out_with_lf_line_endings() {
        let crlf: Vec<&str> = SOURCES
            .iter()
            .filter(|(_, text)| text.contains('\r'))
            .map(|(name, _)| *name)
            .collect();
        assert!(
            crlf.is_empty(),
            "these sources were compiled with CRLF line endings, which hides the \
             `#[cfg(test)]` markers every source guard cuts on: {crlf:?}. The \
             repository's .gitattributes pins LF; renormalise the checkout \
             (`git add --renormalize .` or a fresh clone) rather than editing the \
             guards to tolerate it"
        );
    }

    /// Nothing follows a file's test module.
    ///
    /// `production_end` cuts at the last `#[cfg(test)] mod`, so production code
    /// appended **after** it compiles, ships, and is outside every guard that
    /// reads `production_code`. This finds the module's closing brace — one
    /// fact, covering `async fn`, `unsafe fn`, `mod` and everything else a list
    /// of item prefixes would miss — and requires only whitespace after it.
    #[test]
    fn no_production_code_follows_a_test_module() {
        for (name, source) in SOURCES {
            let end = production_end(source);
            if end == source.len() {
                continue;
            }
            let before = source[..end].lines().count();
            let tail: Vec<&str> = source[end..].lines().collect();
            let close = tail
                .iter()
                .position(|line| *line == "}")
                .unwrap_or_else(|| panic!("{name}: the test module has no column-zero close"));

            for (n, line) in tail.iter().enumerate().skip(close + 1) {
                assert!(
                    line.trim().is_empty(),
                    "{name}:{} is outside the test module and outside every guard: {line:?}",
                    before + n + 1
                );
            }
        }
    }

    /// No source is pulled in from outside the subsystem.
    ///
    /// The one thing the directory walk cannot see: `#[path = "../x.rs"]` and
    /// `include!` both make a file part of this tree while leaving it somewhere
    /// the walk never looks. `cfg_attr` is prohibited alongside them because it
    /// reaches the same place by a route no substring check on `path` can
    /// follow. `include_str!` is unaffected — it is the mechanism these guards
    /// are built on.
    #[test]
    fn no_source_is_pulled_in_from_outside_this_subsystem() {
        for (name, source) in SOURCES {
            for (n, line) in production_code(source) {
                for escape in ["#[path", "include!", "cfg_attr"] {
                    assert!(
                        !line.contains(escape),
                        "{name}:{n} uses {escape:?} to pull in a source the directory walk \
                         cannot see: {:?}",
                        line.trim()
                    );
                }
            }
        }
    }

    /// **`install_full_snapshot` has exactly one production call site**, and it
    /// is the handoff token's consuming method.
    ///
    /// Subsystem-wide rather than per-module, which is the whole point: the
    /// earlier per-module version scanned `handoff.rs` only, so a second call
    /// from the Raft service handler — the one place it would plausibly appear
    /// — would have passed.
    #[test]
    fn install_full_snapshot_has_one_production_call_site() {
        let mut sites = Vec::new();
        for (name, source) in SOURCES {
            for (n, line) in production_code(source) {
                if line.contains("install_full_snapshot(") {
                    sites.push(format!("{name}:{n}"));
                }
            }
        }
        assert_eq!(
            sites.len(),
            1,
            "install_full_snapshot must be called from exactly one place — \
             SnapshotHandoff::install — so the vote, metadata and body go in together; \
             found: {sites:?}"
        );
        assert!(
            sites[0].starts_with("network/handoff.rs"),
            "the single call site must be the handoff token's consuming method: {sites:?}"
        );
    }

    /// **Steps 1–4 of the §5.3 check order exist in exactly one place**,
    /// subsystem-wide.
    ///
    /// The order was written twice: once in `admit` for a unary envelope, once
    /// transcribed into the Raft service for a snapshot header, on the reasoning
    /// that a header is not an envelope and so cannot go through the same
    /// function. Both copies were individually correct when written. Then the
    /// snapshot path's per-chunk recheck was pointed at `admit` — the
    /// *envelope* one — which cannot decode a header frame, so it refused at
    /// step 2 every time and no node ever accepted a snapshot chunk. The
    /// per-module ordering test could not see it, because it only ever read the
    /// copy that was right.
    ///
    /// Only the decode differs between the two frames. Everything else is one
    /// function taking a `ClaimedIdentity`, and this asserts it stays that way:
    /// each question is asked from one line in the whole subsystem.
    ///
    /// A third frame type is welcome. It just has to name its claims rather
    /// than re-answer them.
    #[test]
    fn the_check_order_is_implemented_once_subsystem_wide() {
        let sites_of = |question: &str| {
            let mut sites = Vec::new();
            for (name, source) in SOURCES {
                for (n, line) in production_code(source) {
                    if line.contains(question) {
                        sites.push(format!("{name}:{n}"));
                    }
                }
            }
            sites
        };

        // The inbound questions. Nothing but the check order may ask these:
        // they are about whether to talk to a sender at all.
        for question in [
            "provenance.is_authenticated()",
            "CoordinationNodeId::parse_hex(peer)",
            "ctx.allowlist.permits(",
            "decide(ctx.admission,",
        ] {
            let sites = sites_of(question);
            assert_eq!(
                sites.len(),
                1,
                "{question:?} must be asked in exactly one place — a second copy \
                 of the check order is a second thing to get wrong, and the test \
                 that reads one copy cannot see the other; found: {sites:?}"
            );
            assert!(
                sites[0].starts_with("network/admit.rs"),
                "{question:?} belongs to the check order, not to a handler: {sites:?}"
            );
        }

        // The version check has **two** legitimate sites, and naming them is the
        // guard rather than an exception to it. `admit.rs` refuses an inbound
        // request; `client.rs` refuses a *response* from a peer we dialled.
        // Those are opposite directions with no admission or allowlist question
        // between them, and collapsing them would mean checking a reply against
        // a sender's membership — which a reply does not have.
        let versions = sites_of("!= PROTOCOL_VERSION");
        assert_eq!(
            versions.len(),
            2,
            "the protocol version is checked on the inbound request and on the \
             reply, and nowhere else; found: {versions:?}"
        );
        assert!(
            versions.iter().any(|s| s.starts_with("network/admit.rs"))
                && versions.iter().any(|s| s.starts_with("network/client.rs")),
            "the two version checks must be the request one and the reply one: \
             {versions:?}"
        );
    }

    /// **`SnapshotHandoff` has exactly one constructor**, subsystem-wide.
    ///
    /// A token assembled from three separately obtained parts moves the
    /// crossing one call earlier instead of removing it, so the property that
    /// matters is that no second constructor exists anywhere.
    #[test]
    fn the_handoff_token_has_one_constructor_subsystem_wide() {
        let (built, consumed) = sites_for("SnapshotHandoff");
        assert_eq!(
            built.len(),
            1,
            "SnapshotHandoff must be constructed exactly once — by SnapshotReceipt::prepare,              which consumes the receipt by value; found: {built:?}"
        );
        assert_eq!(
            consumed.len(),
            1,
            "and consumed exactly once, by its install; found: {consumed:?}"
        );
        for site in built.iter().chain(&consumed) {
            assert!(
                site.starts_with("network/handoff.rs"),
                "neither may live outside the handoff module: {built:?} {consumed:?}"
            );
        }
    }

    /// **`SnapshotReceipt` has exactly one constructor**, subsystem-wide, and
    /// is consumed exactly once.
    #[test]
    fn the_receipt_has_one_constructor_subsystem_wide() {
        let (built, consumed) = sites_for("SnapshotReceipt");
        assert_eq!(
            built.len(),
            1,
            "SnapshotReceipt must be constructed exactly once — by \
             SnapshotStreamReceiver::finish, which consumes the receiver; found: {built:?}"
        );
        assert_eq!(
            consumed.len(),
            1,
            "and consumed exactly once, by prepare; found: {consumed:?}"
        );
    }

    /// **`AdmittedHeader` has exactly one constructor**, and it decodes one
    /// header.
    ///
    /// This is the guard for the defect the first revision shipped: the stream
    /// receiver took a header, a decoded vote, a decoded metadata and a body as
    /// four independent arguments, so a vote from another stream compiled. The
    /// runtime cannot see that — there is nothing to compare a vote against —
    /// so the evidence is the shape of the code.
    #[test]
    fn the_admitted_header_has_one_constructor_subsystem_wide() {
        let (built, consumed) = sites_for("AdmittedHeader");
        assert!(
            built.is_empty(),
            "AdmittedHeader is built only through `Self {{ .. }}` inside its own `decode`; \
             a named struct literal elsewhere is a second constructor: {built:?}"
        );
        assert_eq!(
            consumed.len(),
            1,
            "and it is destructured exactly once, by finish; found: {consumed:?}"
        );

        // And no function anywhere takes a vote and a metadata as separate
        // arguments alongside a header — the shape that made the hole.
        for (name, source) in SOURCES {
            let production = &source[..production_end(source)];
            assert!(
                !production.contains("vote: Vote<CoordinationNodeId>,\n        meta: Meta,"),
                "{name} takes a vote and a metadata as separate arguments; they must \
                 arrive as one decoded header"
            );
        }
    }

    /// **`CoordinationSnapshotFile::Prepared` is constructed in exactly two
    /// places**, subsystem-wide: the handoff's install and the test-only suite
    /// adapter.
    ///
    /// `prepare_install` is deliberately not one of them — it returns a bare
    /// `PreparedSnapshot` and never wraps it, which is what keeps the enum
    /// variant reachable only from a proved handoff or the adapter.
    #[test]
    fn the_prepared_variant_is_constructed_in_exactly_two_places() {
        let mut sites = Vec::new();
        for (name, source) in SOURCES {
            // Production only. A test below the boundary that drives
            // `install_snapshot` with a `Prepared` it built itself is
            // exercising the rule rather than escaping it; the test-only suite
            // adapter sits *above* the boundary and is one of the two.
            for (n, line) in production_code(source) {
                let names_it = line.contains("CoordinationSnapshotFile::Prepared(");
                let is_pattern = line.contains("=>") || line.trim_start().starts_with("let ");
                let is_string = line.contains('"');
                if names_it && !is_pattern && !is_string && !line.contains("Prepared(_)") {
                    sites.push(format!("{name}:{n}"));
                }
            }
        }
        assert_eq!(
            sites.len(),
            2,
            "the Prepared variant must be constructed by SnapshotHandoff::install and the \
             test-only suite adapter, and by nothing else; found: {sites:?}"
        );
        assert!(sites[0].starts_with("network/handoff.rs"), "{sites:?}");
        assert!(sites[1].starts_with("storage/machine.rs"), "{sites:?}");
    }

    /// **No production code outside the verifier reads a raw `GenesisV1`.**
    ///
    /// Subsystem-wide now. The sealed `VerifiedGenesis` exists so a caller
    /// cannot decode the presented bytes a second time and act on content that
    /// was never verified, and a second decode usually produces the same value
    /// — so the bug is invisible until the two inputs differ.
    #[test]
    fn only_the_verifier_reads_a_raw_genesis() {
        let mut sites = Vec::new();
        for (name, source) in SOURCES {
            // The verifier is the module that does the verifying; `raft.rs`
            // declares the type; this file names it in prose.
            //
            // `testing.rs` **mints** genesis documents rather than reading
            // them, which is the inverse of the operation this guard protects:
            // the seal exists so no second decoder reads a genesis field, and a
            // provisioning fixture that builds one and signs it has no field to
            // read. It is also gated on `coordination-testing`, which no
            // shipping profile enables.
            if matches!(
                *name,
                "model/authority.rs" | "raft.rs" | "guards.rs" | "testing.rs"
            ) {
                continue;
            }
            for (n, line) in production_code(source) {
                if line.contains("GenesisV1") {
                    sites.push(format!("{name}:{n}"));
                }
            }
        }
        assert!(
            sites.is_empty(),
            "every genesis field must be read through the sealed VerifiedGenesis; \
             found raw reads at {sites:?}"
        );
    }

    /// **The three sealed values have no public fields**, which is the premise
    /// that makes "one construction site" mean "unassemblable elsewhere".
    ///
    /// ## Why this rather than a `trybuild` compile-fail case
    ///
    /// The plan asks for compile-fail evidence that no vote, `SnapshotMeta` and
    /// `PreparedSnapshot` can be assembled into a `SnapshotHandoff`. `trybuild`
    /// compiles *external* crates, and all three types are `pub(crate)` — so
    /// such a case would fail because the **type** is unnameable outside the
    /// crate, and would keep passing if every field were made `pub`. It would
    /// be evidence for a property nobody doubted, and none for the one that
    /// matters.
    ///
    /// The real risk is intra-crate: another coordination module building one.
    /// Rust's own rules make that impossible given three facts, and each is
    /// checkable here:
    ///
    /// 1. every field is private to the defining module (this test);
    /// 2. there is no constructor function taking the parts separately
    ///    (`no_constructor_assembles_the_parts_separately`, in `handoff`);
    /// 3. the struct literal appears exactly once, inside that module
    ///    (the three `*_has_one_constructor_subsystem_wide` tests above).
    ///
    /// Together those *are* the compile-fail argument, with the implication
    /// supplied by the language rather than by a harness. A `trybuild` case
    /// would restate a weaker conclusion; this states the premises of the
    /// stronger one.
    #[test]
    fn the_sealed_values_have_no_public_fields() {
        let handoff = SOURCES
            .iter()
            .find(|(name, _)| *name == "network/handoff.rs")
            .map(|(_, source)| *source)
            .expect("the handoff module is inventoried");
        let production = &handoff[..production_end(handoff)];

        for name in ["AdmittedHeader", "SnapshotReceipt", "SnapshotHandoff"] {
            let at = production
                .find(&format!("struct {name} {{"))
                .unwrap_or_else(|| panic!("{name} must be declared in the handoff module"));
            let body_end = production[at..]
                .find("\n}")
                .unwrap_or_else(|| panic!("{name} has no close"));
            let body = &production[at..at + body_end];

            for (offset, line) in body.lines().enumerate() {
                let code = line.split("//").next().unwrap_or("").trim();
                assert!(
                    !code.starts_with("pub "),
                    "{name} field on line {offset} is public: {code:?}. A public field is a \
                     constructor — a struct literal in any module can then assemble the parts \
                     separately, which is the substitution the seal exists to remove"
                );
                assert!(
                    !code.starts_with("pub(crate)") && !code.starts_with("pub(super)"),
                    "{name} field on line {offset} is visible outside its module: {code:?}"
                );
            }
            // And the type itself is not `pub`, so nothing outside the crate can
            // name it either.
            let declaration_start = production[..at].rfind('\n').map(|n| n + 1).unwrap_or(0);
            let declaration = &production[declaration_start..at + body_end];
            assert!(
                !declaration.contains("pub struct"),
                "{name} must not be publicly nameable"
            );
        }
    }

    /// **Preparation does not occupy the state actor.**
    ///
    /// A rebuild is bounded by §11.4's ceiling at 512 MiB, and the state actor
    /// is the single writer that also serves every apply and every read. An
    /// earlier revision ran preparation there, so a snapshot transfer stalled
    /// unrelated work for its whole duration — for the sake of five values
    /// (`SnapshotIdentity`) that are fixed when storage opens and cannot change
    /// while it runs.
    ///
    /// A source guard rather than a timing test: "did the actor stay free?" is
    /// answered by a race, and a concurrency test that passed on a fast machine
    /// would say nothing on a slow one. What is checkable is that the method
    /// sends no actor command at all.
    #[test]
    fn preparation_never_sends_an_actor_command() {
        let machine = SOURCES
            .iter()
            .find(|(name, _)| *name == "storage/machine.rs")
            .map(|(_, source)| *source)
            .expect("the state machine is inventoried");
        let production = &machine[..production_end(machine)];

        let at = production
            .find("pub(crate) async fn prepare_install(")
            .expect("the handle exposes preparation");
        let body_end = production[at..]
            .find("\n    /// ")
            .unwrap_or(production.len() - at);
        let body = &production[at..at + body_end];

        assert!(
            body.contains("spawn_blocking"),
            "preparation must run on a blocking worker: it is hundreds of megabytes of \
             file I/O and redb writes, and a cooperative executor thread would starve"
        );
        for actor_route in ["self.tx.send", "StateCommand::", "self.call("] {
            assert!(
                !body.contains(actor_route),
                "preparation must not reach the state actor ({actor_route:?}); everything \
                 it compares against is immutable, and queuing it behind an apply is the \
                 stall this moved off the actor to avoid"
            );
        }
    }

    /// The guards above search for tokens that exist, so one pointed at a
    /// renamed construct cannot pass by finding nothing.
    #[test]
    fn the_guards_look_for_tokens_that_exist() {
        let has = |needle: &str| SOURCES.iter().any(|(_, s)| s.contains(needle));
        assert!(has("install_full_snapshot("));
        assert!(has("SnapshotHandoff {"));
        assert!(has("SnapshotReceipt {"));
        assert!(has("AdmittedHeader"));
        assert!(has(&format!(
            "{}::{}(",
            "CoordinationSnapshotFile", "Prepared"
        )));
        assert!(has("GenesisV1"));
    }

    /// Where a named type is built and where it is consumed, subsystem-wide.
    fn sites_for(name: &str) -> (Vec<String>, Vec<String>) {
        let mut built = Vec::new();
        let mut consumed = Vec::new();
        for (file, source) in SOURCES {
            for (n, line) in production_code(source) {
                if builds(line, name) {
                    built.push(format!("{file}:{n}"));
                }
                if destructures(line, name) {
                    consumed.push(format!("{file}:{n}"));
                }
            }
        }
        (built, consumed)
    }

    /// Every `.rs` file beneath `dir`, relative to it.
    fn walk(dir: &std::path::Path, prefix: &str, into: &mut Vec<String>) {
        let entries =
            std::fs::read_dir(dir).unwrap_or_else(|err| panic!("reading {}: {err}", dir.display()));
        for entry in entries {
            let entry = entry.expect("a directory entry");
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path();
            if path.is_dir() {
                walk(&path, &format!("{prefix}{name}/"), into);
            } else if name.ends_with(".rs") {
                into.push(format!("{prefix}{name}"));
            }
        }
    }
}
