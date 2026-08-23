//! **Actual process loss**: three voters in three processes, one killed
//! abruptly (§S3-F).
//!
//! ## Why this is not the in-process test again
//!
//! `coordination_cluster.rs` already shows a three-voter group surviving the
//! loss of its leader. It does so with `AsterServer::shutdown()`, which closes
//! the endpoint, releases the storage lock, drains both actors and lets
//! outstanding calls finish — a *cooperative* departure, and every one of those
//! steps is something a lost process does not do.
//!
//! The contract matrix asks for the other thing: real Iroh loopback subprocess
//! voters and actual process loss. `Child::kill` is `SIGKILL` on Unix and
//! `TerminateProcess` on Windows. Neither runs a destructor, so the survivors
//! see exactly what they would see if the machine had gone: connections that
//! stop answering, a lock file still on disk, and no notice.
//!
//! ## What it proves that the graceful test cannot
//!
//! 1. The **leader** is the one killed, and killed without warning.
//! 2. The survivors elect among themselves and **commit a real write**, so the
//!    group is working rather than merely reporting health.
//! 3. The killed voter **restarts onto its own storage** and rejoins — which is
//!    also the only place the storage lock left behind by a killed process is
//!    exercised end to end, rather than in isolation.

#![cfg(feature = "coordination-testing")]

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

/// Generous: three processes have to build nothing but do have to bind, find
/// each other and elect, and a loaded machine running the rest of the suite
/// alongside is normal.
const PATIENCE: Duration = Duration::from_secs(90);
const SIZE: usize = 3;
const SEED: u8 = 0x6B;

/// What a voter last reported.
#[derive(Debug, Clone, PartialEq)]
struct Status {
    lifecycle: i32,
    quorum: String,
    /// The voter index this node believes leads, or `-1`.
    leader: i64,
    applied: i64,
}

fn read_status(control: &Path, index: usize) -> Option<Status> {
    let text = std::fs::read_to_string(control.join(format!("status-{index}"))).ok()?;
    let mut parts = text.split_whitespace();
    Some(Status {
        lifecycle: parts.next()?.parse().ok()?,
        quorum: parts.next()?.to_string(),
        leader: parts.next()?.parse().ok()?,
        applied: parts.next()?.parse().ok()?,
    })
}

struct Voters {
    control: PathBuf,
    children: Vec<Option<Child>>,
    _dir: tempfile::TempDir,
}

impl Voters {
    fn spawn(control: &Path, index: usize) -> Child {
        Command::new(env!("CARGO_BIN_EXE_coordination-voter"))
            .arg(control)
            .arg(index.to_string())
            .arg(SIZE.to_string())
            .arg(SEED.to_string())
            .spawn()
            .expect("the voter binary starts")
    }

    fn start() -> Self {
        let dir = tempfile::tempdir().expect("a control directory");
        let control = dir.path().to_path_buf();
        let children = (0..SIZE).map(|i| Some(Self::spawn(&control, i))).collect();
        Self {
            control,
            children,
            _dir: dir,
        }
    }

    /// Wait until `predicate` holds over every voter's last report.
    fn until(
        &self,
        what: &str,
        predicate: impl Fn(&[Option<Status>]) -> bool,
    ) -> Vec<Option<Status>> {
        let deadline = Instant::now() + PATIENCE;
        loop {
            let observed: Vec<Option<Status>> =
                (0..SIZE).map(|i| read_status(&self.control, i)).collect();
            if predicate(&observed) {
                return observed;
            }
            if Instant::now() >= deadline {
                panic!("{what}: not reached within {PATIENCE:?}; observed {observed:?}");
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Kill voter `index` with no warning at all.
    fn kill(&mut self, index: usize) {
        let child = self.children[index]
            .as_mut()
            .expect("the voter is still running");
        child.kill().expect("the voter is killable");
        child.wait().expect("reap the killed voter");
        self.children[index] = None;
    }

    fn restart(&mut self, index: usize) {
        assert!(
            self.children[index].is_none(),
            "voter {index} is already running"
        );
        self.children[index] = Some(Self::spawn(&self.control, index));
    }

    /// Ask voter `index` to commit a write, and return the index it landed at.
    fn propose(&self, index: usize, key: &str) -> Result<i64, String> {
        let answer_path = self.control.join(format!("proposed-{index}"));
        let _ = std::fs::remove_file(&answer_path);
        std::fs::write(self.control.join(format!("propose-{index}")), key)
            .expect("write a proposal request");

        let deadline = Instant::now() + PATIENCE;
        loop {
            if let Ok(answer) = std::fs::read_to_string(&answer_path) {
                let mut parts = answer.splitn(2, ' ');
                return match (parts.next(), parts.next()) {
                    (Some("ok"), Some(index)) => Ok(index.trim().parse().expect("an index")),
                    (Some("err"), Some(reason)) => Err(reason.to_string()),
                    _ => panic!("voter {index} answered {answer:?}"),
                };
            }
            if Instant::now() >= deadline {
                panic!("voter {index} never answered a proposal");
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

impl Drop for Voters {
    fn drop(&mut self) {
        for child in self.children.iter_mut().flatten() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// **A three-voter group survives its leader being killed outright, and the
/// killed voter rejoins.**
///
/// Not `#[tokio::test]`: everything the parent does is blocking file and
/// process work, and the voters have their own runtimes in their own
/// processes — which is the entire point.
#[test]
fn a_group_survives_losing_its_leader_to_a_kill_and_takes_it_back() {
    let mut voters = Voters::start();

    // A leader, agreed by everyone. Not just "someone claims to lead": a
    // majority reporting the same leader is what makes killing it meaningful.
    let observed = voters.until("a leader is elected", |statuses| {
        let leaders: Vec<i64> = statuses.iter().flatten().map(|s| s.leader).collect();
        leaders.len() == SIZE && leaders[0] >= 0 && leaders.iter().all(|l| *l == leaders[0])
    });
    let leader = observed[0].as_ref().expect("a status").leader as usize;

    // The control: with all three up, a write commits.
    let control_index = voters
        .propose(leader, "before-the-kill")
        .expect("a healthy group commits");

    // No notice, no shutdown, no lock release.
    voters.kill(leader);

    // The survivors must elect among *themselves*. Requiring a new leader that
    // is not the dead one is the assertion; a test that only waited for "some
    // leader" would pass on stale status files the killed voter left behind.
    voters.until("the survivors elect a new leader", |statuses| {
        (0..SIZE)
            .filter(|i| *i != leader)
            .filter_map(|i| statuses[i].as_ref())
            .filter(|s| s.leader >= 0 && s.leader as usize != leader)
            .count()
            == SIZE - 1
    });

    // And the group still *works*. Two of three is a quorum, and this is the
    // difference between a group that survived and one that merely re-elected.
    let survivor = (0..SIZE).find(|i| *i != leader).expect("a survivor");
    let after_index = voters
        .propose(survivor, "after-the-kill")
        .expect("two of three commit");
    assert!(
        after_index > control_index,
        "the post-loss write must land past the pre-loss one: \
         {control_index} then {after_index}"
    );

    // The killed voter comes back onto its own storage — including the lock
    // file it never released — and catches up to what it missed.
    voters.restart(leader);
    voters.until("the restarted voter rejoins and catches up", |statuses| {
        statuses[leader]
            .as_ref()
            .is_some_and(|s| s.applied >= after_index && s.lifecycle == 3)
    });
}
