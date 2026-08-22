//! Process-death boundaries, and what a reopen must find after one.
//!
//! ## What this is evidence of, and what it is not
//!
//! The parent ends the child with [`std::process::Child::kill`] -- `SIGKILL` on
//! Unix, `TerminateProcess` on Windows; the claim this module makes is
//! unconditional termination, not a particular signal. Either way the child
//! stops without unwinding, so no destructor runs, no `Drop` closes a database,
//! and nothing Rust buffered is flushed on the way out. That is what makes this
//! evidence about a *crash* rather than about an orderly shutdown, and it is a
//! stronger statement than dropping a handle in-process, which runs every
//! cleanup path the code has.
//!
//! The harness checks that this is what happened rather than assuming it: the
//! parent requires a child that announced its boundary to have died by the
//! kill, because one that had already returned would have closed its files
//! through the ordinary paths and the store observed afterwards would be a
//! clean shutdown wearing a crash's name.
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
//! [`crash_points!`] emits the boundary list, the name each is armed by, and the
//! parser that turns a name back into a boundary -- one text, so the set the
//! harness arms and the set the code calls cannot drift apart.
//!
//! What it deliberately does **not** emit is the outcome. A boundary is a place
//! in a durable operation, and the same place means different things to
//! different operations: `transaction-commit` is the moment a vote becomes
//! durable, and it is also the moment an install's rebuilt *candidate* commits
//! under `incoming/` while `state.redb` is still the old one -- durable for the
//! first, discarded for the second. Worse, a write transaction that is thrown
//! away is not the same as an operation that did not happen: an apply crashed
//! before its commit loses the transaction and then has it replayed, because
//! the entry it covered is already committed in the log. The outcome therefore
//! belongs to the (operation, boundary) cell, and is declared beside the
//! operations in `machine.rs` rather than here.

// Compiled in every configuration, deliberately. Splitting the boundary list
// into a test copy and a production stub is the same producer/recogniser split
// that has bitten this module's neighbours: two lists drift, and the one the
// harness runs stops being the one the code calls. What is `cfg`-gated is the
// *harness* -- the environment, the ready line, the parking -- while `reached`
// compiles to an empty inline call outside tests.

/// One of the two states a reopen may be compared against.
///
/// Deliberately three-valued rather than "either": [`Expected::Either`] is for
/// the answers the protocol genuinely leaves open, and using it where the
/// answer is determined would turn a real regression into a pass. Every
/// [`Outcome`] that produces it says, in its own doc, what makes it undetermined
/// -- and none currently does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum Expected {
    /// Exactly what was there before the operation started.
    Before,
    /// Exactly what the same operation run to completion leaves behind.
    After,
    /// Undetermined, and narrowed to those two: either, and nothing else.
    Either,
}

/// What dying at one boundary of one operation means, on both axes.
///
/// Two axes because a crash has two honest answers and they are not always the
/// same one. *On disk* is what the killed process left behind, read the instant
/// the store is reopened. *Recovered* is what the voter comes back holding,
/// after `StorageHelper::get_initial_state` has done what OpenRaft does on every
/// restart. Collapsing them would force a choice between claiming a discarded
/// transaction survived and claiming a committed command was lost.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum Outcome {
    /// The operation left nothing, and nothing puts it back. redb throws away
    /// an uncommitted transaction, and there is no committed log entry standing
    /// behind this operation to make recovery redo it.
    Discarded,
    /// The operation's effect is on disk and recovery keeps it.
    Durable,
    /// The write was thrown away and OpenRaft's recovery redoes it from the
    /// log. Only reachable where the log already holds the operation's input as
    /// committed -- which, for an apply, is the ordinary production case: an
    /// entry is committed before it is applied, so a crash inside the apply
    /// transaction is repaired by replay rather than lost.
    Replayed,
    /// The protocol permits both answers at this cell. Nothing declares it: it
    /// exists so that a genuinely undetermined boundary can be stated as one
    /// instead of being hidden inside a widened comparison, and any use of it
    /// must say at the cell what makes the answer open.
    Undetermined,
}

#[allow(dead_code)]
impl Outcome {
    /// What the killed process must have left, before recovery runs.
    pub(crate) fn on_disk(self) -> Expected {
        match self {
            Outcome::Discarded | Outcome::Replayed => Expected::Before,
            Outcome::Durable => Expected::After,
            Outcome::Undetermined => Expected::Either,
        }
    }

    /// What a production reopen must come back holding.
    pub(crate) fn recovered(self) -> Expected {
        match self {
            Outcome::Discarded => Expected::Before,
            Outcome::Durable | Outcome::Replayed => Expected::After,
            Outcome::Undetermined => Expected::Either,
        }
    }
}

macro_rules! crash_points {
    ($($(#[$doc:meta])* $variant:ident = $name:literal),+ $(,)?) => {
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
        }
    };
}

crash_points! {
    /// Before `begin_write`. Nothing has been staged, let alone written.
    TransactionBegin = "transaction-begin",
    /// Rows staged inside the write transaction, before `commit`. redb discards
    /// an uncommitted transaction, so these rows are not going to survive.
    TransactionWrite = "transaction-write",
    /// `commit` has returned. The transaction was opened with
    /// `Durability::Immediate`, so returning means committed -- though *which*
    /// database committed is the operation's business: for an install this is
    /// the candidate under `incoming/`, not the live state.
    TransactionCommit = "transaction-commit",
    /// The reply has been produced for the caller, strictly after the commit
    /// that produced it. This is the lost acknowledgement: the batch is durable
    /// and the caller never learned it.
    Acknowledgement = "acknowledgement",
    /// Part-way through streaming a snapshot body into its scratch file.
    /// Nothing is published, and the state the body was taken from is
    /// untouched.
    SnapshotBodyWrite = "snapshot-body-write",
    /// The body is written and about to be, or has just been, fsynced. Still a
    /// scratch file under `snapshots/`: publication is the later rename.
    SnapshotBodyFsync = "snapshot-body-fsync",
    /// The rebuilt incoming candidate has committed. It is a complete database
    /// under `incoming/`, and this store's `state.redb` is still the old one.
    CandidateCommit = "candidate-commit",
    /// The rename has returned: `state.redb` **is** the candidate. The old
    /// database is gone and there is nothing to go back to.
    ReplacementRename = "replacement-rename",
    /// After the rename, at the directory sync that publishes it. The rename
    /// was already visible to the next open; see this module's header for why
    /// that says nothing about the sync itself.
    ReplacementDirectorySync = "replacement-directory-sync",
    /// The replaced database has been reopened and revalidated, and `CURRENT`
    /// has not yet been moved. The state is the installed one; the marker still
    /// names what the store had before, which the startup sweep is what
    /// reconciles.
    PostReplacementReopen = "post-replacement-reopen",
    /// `CURRENT` has been renamed into place and its directory fsynced, so the
    /// marker names the snapshot this operation published.
    CurrentPublication = "current-publication",
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

    /// The two axes, spelled out.
    ///
    /// `Replayed` is the one worth pinning: it is the only outcome whose axes
    /// disagree, and the whole reason the expectation had to leave `CrashPoint`.
    /// Written as literals rather than derived from the accessors, so a mutation
    /// that made replay indistinguishable from durability fails here as well as
    /// in the matrix -- and fails saying which of the two claims moved.
    #[test]
    fn a_replayed_write_is_absent_on_disk_and_present_after_recovery() {
        assert_eq!(Outcome::Discarded.on_disk(), Expected::Before);
        assert_eq!(Outcome::Discarded.recovered(), Expected::Before);

        assert_eq!(Outcome::Durable.on_disk(), Expected::After);
        assert_eq!(Outcome::Durable.recovered(), Expected::After);

        assert_eq!(Outcome::Replayed.on_disk(), Expected::Before);
        assert_eq!(Outcome::Replayed.recovered(), Expected::After);

        assert_eq!(Outcome::Undetermined.on_disk(), Expected::Either);
        assert_eq!(Outcome::Undetermined.recovered(), Expected::Either);
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
        // nothing to match, which is what keeps the seam inert for every other
        // test in this crate that drives these same production paths.
        arm();
        for point in CrashPoint::ALL {
            reached(*point);
        }
    }
}
