//! Process-death boundaries, and what a reopen must find after one.
//!
//! ## What this is evidence of, and what it is not
//!
//! The parent sends `SIGKILL` through [`std::process::Child::kill`]. The child
//! stops without unwinding, so no destructor runs, no `Drop` closes a database,
//! and nothing Rust buffered is flushed on the way out. That is what makes this
//! evidence about a *crash* rather than about an orderly shutdown, and it is a
//! stronger statement than dropping a handle in-process, which runs every
//! cleanup path the code has.
//!
//! It is **not** power-loss evidence, and nothing here should be read as
//! proving a flush. The kernel page cache outlives the process: bytes already
//! written through a file descriptor are there for the next open whether or not
//! anything ever fsynced them. A boundary sitting just before an fsync
//! therefore proves nothing about that fsync -- the data survives either way,
//! and a build with every `sync_all` deleted would pass every case in this
//! file.
//!
//! What it does establish is **publication ordering and restart behaviour**:
//! that a store killed part-way through an operation reopens at all, and that
//! what it reopens holding is one of the states the protocol permits at that
//! point -- not a torn one, and not one that requires the operation to have
//! half-happened. Durability against power loss stays a property of the code
//! and the platform, argued where the fsync is written rather than measured
//! here.
//!
//! ## One declaration
//!
//! [`crash_points!`] emits the boundary list, the name each is armed by, the
//! parser that turns a name back into a boundary, and what recovery must find.
//! A boundary cannot be added to the enum without stating its expected outcome,
//! and cannot be given an outcome without joining the list the harness runs --
//! which is the property a hand-maintained table beside a hand-maintained enum
//! does not have.

// Compiled in every configuration, deliberately. Splitting the boundary list
// into a test copy and a production stub is the same producer/recogniser split
// that has bitten this module's neighbours: two lists drift, and the one the
// harness runs stops being the one the code calls. What is `cfg`-gated is the
// *harness* -- the environment, the ready line, the parking -- while `reached`
// compiles to an empty inline call outside tests.

/// What a reopen must find after the child died at a boundary.
///
/// Deliberately three-valued rather than "either": `Either` is for the
/// boundaries where the protocol genuinely permits both, and using it where the
/// answer is determined would turn a real regression into a pass. Every use of
/// it names, in its own doc, what makes that boundary undetermined.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum Expected {
    /// The operation left no trace. Recovery must find exactly the state that
    /// was there before it started.
    Before,
    /// The operation is durable. Recovery must find its effect.
    After,
    /// Undetermined *at this boundary*, and narrowed to these two: recovery may
    /// find either, and nothing else.
    Either,
}

macro_rules! crash_points {
    ($($(#[$doc:meta])* $variant:ident = $name:literal => $expected:ident),+ $(,)?) => {
        /// A point in a durable operation at which the child stops and blocks.
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        #[allow(dead_code)]
        pub(crate) enum CrashPoint {
            $($(#[$doc])* $variant,)+
        }

        #[allow(dead_code)]
        impl CrashPoint {
            /// Every boundary the harness runs, in the order they occur.
            pub(crate) const ALL: &'static [CrashPoint] = &[$(CrashPoint::$variant,)+];

            /// The name this boundary is armed by, across the process boundary.
            pub(crate) fn name(self) -> &'static str {
                match self { $(CrashPoint::$variant => $name,)+ }
            }

            /// The inverse of [`CrashPoint::name`], written from the same
            /// declaration so the two cannot describe different sets.
            pub(crate) fn parse(name: &str) -> Option<Self> {
                match name { $($name => Some(CrashPoint::$variant),)+ _ => None }
            }

            /// What a reopen must find after dying here.
            pub(crate) fn expected(self) -> Expected {
                match self { $(CrashPoint::$variant => Expected::$expected,)+ }
            }
        }
    };
}

crash_points! {
    /// Before `begin_write`. Nothing has been staged, let alone written.
    TransactionBegin = "transaction-begin" => Before,
    /// Rows staged inside the write transaction, before `commit`. redb
    /// discards an uncommitted transaction, so the entry never happened.
    TransactionWrite = "transaction-write" => Before,
    /// `commit` has returned. The transaction was opened with
    /// `Durability::Immediate`, so returning means committed.
    TransactionCommit = "transaction-commit" => After,
    /// The reply has been produced for the caller. Strictly after the commit
    /// that produced it, so anything the commit made durable is still durable.
    Acknowledgement = "acknowledgement" => After,
    /// Part-way through streaming a snapshot body into its scratch file.
    /// Nothing is published, and the state the body was taken from is
    /// untouched.
    SnapshotBodyWrite = "snapshot-body-write" => Before,
    /// The body is written and about to be, or has just been, fsynced. Still a
    /// scratch file under `snapshots/`: publication is the later rename.
    SnapshotBodyFsync = "snapshot-body-fsync" => Before,
    /// The rebuilt incoming candidate has committed. It is a complete database
    /// under `incoming/`, and this store's `state.redb` is still the old one.
    CandidateCommit = "candidate-commit" => Before,
    /// The rename has returned: `state.redb` **is** the candidate. The old
    /// database is gone and there is nothing to go back to.
    ReplacementRename = "replacement-rename" => After,
    /// After the rename, at the directory sync that publishes it. The rename
    /// was already visible to the next open; see this module's header for why
    /// that says nothing about the sync itself.
    ReplacementDirectorySync = "replacement-directory-sync" => After,
    /// The replaced database has been reopened and revalidated, and `CURRENT`
    /// has not yet been moved. The state is the installed one; the marker
    /// still names what the store had before, which the startup sweep is what
    /// reconciles.
    PostReplacementReopen = "post-replacement-reopen" => After,
    /// `CURRENT` now names the installed snapshot.
    CurrentPublication = "current-publication" => After,
}

/// The environment variable naming the boundary the child stops at.
#[cfg(test)]
pub(crate) const BOUNDARY_VAR: &str = "ASTER_COORDINATION_CRASH_AT";

/// The line the child writes before blocking, so the parent kills it at the
/// boundary rather than at some point near it.
#[cfg(test)]
pub(crate) const READY: &str = "aster-coordination-crash-ready";

/// Whether the boundary named in the environment is live yet.
///
/// Separate from *which* boundary, because the two answer different questions.
/// The environment says where to stop; this says when the harness has finished
/// setting up and the next occurrence is the one it means. Without it the seam
/// would fire during the child's own open -- `durable_write` runs there too --
/// and every transaction case would measure the open rather than the operation.
#[cfg(test)]
static ARMED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Make the next occurrence of the armed boundary the one that stops.
///
/// Called by the child once its store is open and its setup is done. In an
/// unarmed process this is inert: there is no boundary named, so nothing stops.
#[cfg(test)]
pub(crate) fn arm() {
    ARMED.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// The boundary this process was told to stop at, if any.
///
/// Read from the environment rather than set through a Rust seam, because the
/// process that must obey it is not the one that armed it. An unrecognised name
/// aborts rather than running unarmed: a harness that misspelled a boundary
/// would otherwise watch the child run to completion and report a clean pass
/// for a case it never exercised.
#[cfg(test)]
fn armed() -> Option<CrashPoint> {
    let name = std::env::var(BOUNDARY_VAR).ok()?;
    match CrashPoint::parse(&name) {
        Some(point) => Some(point),
        None => panic!("{BOUNDARY_VAR} names no boundary: {name:?}"),
    }
}

/// Announce that `point` was reached and block forever, if it is the armed one.
///
/// Blocking rather than exiting: the parent must be the one that ends this
/// process, so the exit is a kill and not a return, and the state on disk is
/// whatever the last completed syscall left. Parking also means the write
/// transaction, the open database and the scratch file are all still held --
/// exactly as they would be at the instant a real process died.
#[cfg(test)]
pub(crate) fn reached(point: CrashPoint) {
    use std::io::Write as _;

    if !ARMED.load(std::sync::atomic::Ordering::SeqCst) || armed() != Some(point) {
        return;
    }
    let mut out = std::io::stdout().lock();
    // The leading newline is load-bearing. libtest writes `test <name> ... `
    // without one, so an announcement printed straight after it lands on the
    // *end* of that line and a parent matching on a prefix never sees it --
    // which is a harness that hangs until its reader hits EOF rather than one
    // that reports a boundary was missed.
    let _ = writeln!(out, "\n{READY} {}", point.name());
    let _ = out.flush();
    loop {
        std::thread::park();
    }
}

/// Outside tests the seam is not merely unarmed, it is not there: no
/// environment read, no branch, nothing to arm.
#[cfg(not(test))]
#[inline]
#[allow(dead_code)]
pub(crate) fn reached(_point: CrashPoint) {}

#[cfg(test)]
mod tests {
    use super::*;

    /// The list and the outcomes come from one declaration, so this checks the
    /// part a macro cannot: that the names are distinct and round-trip.
    ///
    /// A duplicate name would arm two boundaries under one string and silently
    /// run one of them twice while never running the other.
    #[test]
    fn every_boundary_has_a_distinct_name_that_parses_back() {
        let mut seen = std::collections::BTreeSet::new();
        for point in CrashPoint::ALL {
            assert!(
                seen.insert(point.name()),
                "two boundaries share the name {:?}",
                point.name()
            );
            assert_eq!(CrashPoint::parse(point.name()), Some(*point));
        }
        assert_eq!(seen.len(), CrashPoint::ALL.len());
    }

    /// The handover inventory, restated as a test.
    ///
    /// Not a count: a count passes when a boundary is renamed into something
    /// nobody asked for. These are the eleven the review named, spelled out, so
    /// dropping one fails here rather than reducing the matrix quietly.
    #[test]
    fn the_boundary_list_is_the_one_that_was_agreed() {
        let names: Vec<&str> = CrashPoint::ALL.iter().map(|p| p.name()).collect();
        assert_eq!(
            names,
            vec![
                "transaction-begin",
                "transaction-write",
                "transaction-commit",
                "acknowledgement",
                "snapshot-body-write",
                "snapshot-body-fsync",
                "candidate-commit",
                "replacement-rename",
                "replacement-directory-sync",
                "post-replacement-reopen",
                "current-publication",
            ]
        );
    }

    /// An unarmed process runs straight through every boundary.
    ///
    /// The whole suite depends on it: `reached` is called from production paths
    /// that every other test drives, and a seam that blocked without being
    /// armed would hang them all.
    #[test]
    fn an_unarmed_process_passes_through_every_boundary() {
        assert!(
            std::env::var(BOUNDARY_VAR).is_err(),
            "the suite itself must never run armed"
        );
        // Armed, and still passing through: with no boundary named there is
        // nothing to match, which is what keeps the seam inert for the 692
        // other tests that drive these same production paths.
        arm();
        for point in CrashPoint::ALL {
            reached(*point);
        }
    }
}
