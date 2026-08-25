//! §11.5's host-local pressure monitor.
//!
//! One task per runtime samples two facts about the disk under the voter
//! directory and publishes them as the two [`PressureState`] tiers the Store
//! admission path reads:
//!
//! * **Growth** — the state database has reached its §12.2 high-water mark
//!   ([`LOCAL_HIGH_WATER`]). Growth-class mutations and new-session creation
//!   are refused; reads and safety-reducing operations continue.
//! * **Hard** — free space on the volume is below the larger of one GiB or
//!   twice the latest snapshot, the room §11 needs to write a snapshot and
//!   replace it atomically. This tier is a **latch**: once hit it never
//!   clears, every new mutation is refused, and the monitor runs the §11.5
//!   shutdown sequence itself — "shut down the local coordination runtime
//!   cleanly so another voter can lead". A pressured leader must step aside,
//!   not retain leadership while refusing every mutation.
//!
//! The monitor is deliberately a *sampler*: admission reads two atomics, never
//! the filesystem, so the §12.2 promise that cheap rejection precedes
//! expensive work holds on the hot path. The tiers are host-local facts and
//! never consulted by apply — a proposal admitted elsewhere still applies
//! here, which is what keeps the replicas identical.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::owner::ShutdownSequence;
use crate::coordination::model::limits::LOCAL_HIGH_WATER;
use crate::coordination::store::host::{PressureState, StoreMetrics};

/// The floor never sinks below one GiB, whatever the snapshot history says.
pub(crate) const HARD_FLOOR_MIN: u64 = 1024 * 1024 * 1024;

/// How often the two tiers are re-sampled.
const SAMPLE_EVERY: std::time::Duration = std::time::Duration::from_secs(2);

/// The §11.5 free-space floor: room for one snapshot write plus its atomic
/// replacement, but at least [`HARD_FLOOR_MIN`].
pub(crate) fn hard_floor(latest_snapshot: u64) -> u64 {
    HARD_FLOOR_MIN.max(latest_snapshot.saturating_mul(2))
}

/// The size of the largest file in the snapshot directory — the conservative
/// stand-in for "the latest snapshot" that stays correct while a new, larger
/// snapshot is mid-write next to the current one.
pub(crate) fn largest_snapshot(snapshots: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(snapshots) else {
        return 0;
    };
    entries
        .flatten()
        .filter_map(|entry| entry.metadata().ok())
        .filter(|meta| meta.is_file())
        .map(|meta| meta.len())
        .max()
        .unwrap_or(0)
}

/// Free bytes available to this process on the volume holding `path`.
///
/// `None` means the measurement itself failed; the monitor treats that as
/// no-signal rather than latching a drain off a broken probe.
#[cfg(unix)]
pub(crate) fn free_space(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    Some(stat.f_bavail as u64 * stat.f_frsize as u64)
}

#[cfg(windows)]
pub(crate) fn free_space(path: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    let mut available: u64 = 0;
    let ok = unsafe {
        windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        None
    } else {
        Some(available)
    }
}

/// One sampling pass, factored out so the loop and the tests share it.
///
/// Returns whether the hard tier is (now) hit. The hard tier **latches** in
/// [`PressureState`] itself: once a pass observes free space below the floor
/// — or the test seam forces the tier — no later pass clears it. §11.5 makes
/// the tier a one-way trip into evacuation, never a reversible refusal gate:
/// space freed after the latch does not un-drain a node whose runtime is
/// already stepping aside.
pub(crate) fn sample(
    state_db: &Path,
    raft_db: &Path,
    snapshots: &Path,
    volume: &Path,
    pressure: &PressureState,
) -> bool {
    use std::sync::atomic::Ordering;
    let file_len = |path: &Path| std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let growth = growth_pressure_at(state_db, LOCAL_HIGH_WATER);
    pressure.growth.store(growth, Ordering::Relaxed);
    let snapshot_len = largest_snapshot(snapshots);
    let floor = hard_floor(snapshot_len);
    let free = free_space(volume);
    let hard = pressure.hard.load(Ordering::Relaxed)
        || pressure.force_hard.load(Ordering::Relaxed)
        || free.is_some_and(|free| free < floor);
    pressure.hard.store(hard, Ordering::Relaxed);
    // The §14 disk gauges ride the same pass: sizes and free-space state,
    // never file contents.
    pressure
        .state_db_bytes
        .store(file_len(state_db), Ordering::Relaxed);
    pressure
        .raft_db_bytes
        .store(file_len(raft_db), Ordering::Relaxed);
    pressure
        .snapshot_bytes
        .store(snapshot_len, Ordering::Relaxed);
    pressure
        .free_space_bytes
        .store(free.unwrap_or(0), Ordering::Relaxed);
    hard
}

/// Whether the state database has reached `high_water` bytes on disk. An
/// absent file is no pressure: the store has not been created yet.
pub(crate) fn growth_pressure_at(state_db: &Path, high_water: u64) -> bool {
    std::fs::metadata(state_db).is_ok_and(|meta| meta.len() >= high_water)
}

/// The monitor task. Samples until the hard tier latches, then evacuates:
/// §11.5's "reject all new Store mutations, emit storage-pressure health,
/// and shut down the local coordination runtime cleanly so another voter can
/// lead". The shutdown sequence marks the node draining (the storage-pressure
/// health signal, beside the latched free-space gauge), drains in-flight
/// Store calls, and stops the Raft — which is what actually lets another
/// voter win the next election. The monitor then returns; the latched tier
/// and gauges keep reporting the pressure that ended the runtime.
pub(crate) async fn run(
    state_db: PathBuf,
    raft_db: PathBuf,
    snapshots: PathBuf,
    pressure: Arc<PressureState>,
    metrics: Arc<StoreMetrics>,
    shutdown: ShutdownSequence,
) {
    // The volume is probed at the voter directory, the parent that must hold
    // both databases and the snapshot being written.
    let volume = state_db.parent().map(Path::to_path_buf).unwrap_or_default();
    let mut ticks = tokio::time::interval(SAMPLE_EVERY);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticks.tick().await;
        let hard = sample(&state_db, &raft_db, &snapshots, &volume, &pressure);
        if hard {
            StoreMetrics::bump(&metrics.pressure_refusals);
            shutdown.run().await;
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The floor is one GiB until snapshots outgrow half of it, then twice
    /// the snapshot — §11.5's "room to write and replace one snapshot".
    #[test]
    fn the_floor_is_the_larger_of_a_gigabyte_and_two_snapshots() {
        assert_eq!(hard_floor(0), HARD_FLOOR_MIN);
        assert_eq!(hard_floor(HARD_FLOOR_MIN / 2), HARD_FLOOR_MIN);
        assert_eq!(hard_floor(HARD_FLOOR_MIN), 2 * HARD_FLOOR_MIN);
        assert_eq!(hard_floor(u64::MAX), u64::MAX);
    }

    /// The largest file in the snapshot directory stands in for the latest
    /// snapshot; markers and absent directories read as zero.
    #[test]
    fn the_largest_snapshot_is_measured_from_the_directory() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        assert_eq!(largest_snapshot(&dir.path().join("absent")), 0);
        assert_eq!(largest_snapshot(dir.path()), 0);
        std::fs::write(dir.path().join("a.snap"), vec![0u8; 100]).expect("write");
        std::fs::write(dir.path().join("b.snap"), vec![0u8; 4096]).expect("write");
        assert_eq!(largest_snapshot(dir.path()), 4096);
    }

    /// A real volume answers the probe, and the answer is in plausible bytes.
    #[test]
    fn free_space_answers_on_a_real_volume() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let free = free_space(dir.path()).expect("a real volume answers");
        assert!(free > 0);
        assert!(free_space(&dir.path().join("no-such-child")).is_none());
    }

    /// One pass publishes both tiers: growth from the database size, hard
    /// from free space against the floor. A scratch volume has far more than
    /// the tiny floor implied by no snapshots and far less than `u64::MAX`.
    #[test]
    fn a_sample_publishes_both_tiers() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let db = dir.path().join("state.redb");
        std::fs::write(&db, vec![0u8; 2048]).expect("write");
        let pressure = PressureState::default();

        // Not at the high-water mark, plenty of space: both tiers off.
        assert!(!sample(&db, &db, dir.path(), dir.path(), &pressure));
        assert!(
            pressure
                .state_db_bytes
                .load(std::sync::atomic::Ordering::Relaxed)
                == 2048,
            "the gauges ride the sampling pass"
        );
        assert!(
            pressure
                .free_space_bytes
                .load(std::sync::atomic::Ordering::Relaxed)
                > 0
        );
        assert!(!pressure.growth.load(std::sync::atomic::Ordering::Relaxed));
        assert!(!pressure.hard.load(std::sync::atomic::Ordering::Relaxed));

        // Growth is a pure function of the database size against the limit,
        // pinned separately against the shared helper.
        assert!(growth_pressure_at(&db, 1024));
        assert!(!growth_pressure_at(&db, 1024 * 1024));
    }

    /// §11.5's hard tier is a latch: a pass that observes it (here through
    /// the forced seam, as a full disk would in production) leaves it set,
    /// and no later pass — however much space is free — clears it.
    #[test]
    fn the_hard_tier_latches_across_passes() {
        use std::sync::atomic::Ordering;
        let dir = tempfile::tempdir().expect("a scratch directory");
        let db = dir.path().join("state.redb");
        std::fs::write(&db, vec![0u8; 16]).expect("write");
        let pressure = PressureState::default();

        pressure.force_hard.store(true, Ordering::Relaxed);
        assert!(sample(&db, &db, dir.path(), dir.path(), &pressure));

        // The force is withdrawn; the latch is not.
        pressure.force_hard.store(false, Ordering::Relaxed);
        assert!(
            sample(&db, &db, dir.path(), dir.path(), &pressure),
            "a hard tier that clears when space returns is a reversible \
             refusal gate, which §11.5 forbids"
        );
        assert!(pressure.hard.load(Ordering::Relaxed));
    }
}
