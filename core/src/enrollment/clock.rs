//! Local clock synchronization status. Enrollment compares signed times and
//! peer timestamps against this clock, so an unsynchronized host misjudges
//! credential activation and expiry.

/// What the kernel reports about NTP discipline of the local clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClockSynchronization {
    Synchronized,
    /// The kernel marks the clock unsynchronized (`STA_UNSYNC`/`TIME_ERROR`).
    /// Enable `systemd-timesyncd` or `chronyd`.
    Unsynchronized,
    /// This platform offers no check, or the status could not be read.
    NotChecked,
}

#[cfg(any(test, target_os = "linux"))]
const STA_UNSYNC: i32 = 0x0040;
#[cfg(any(test, target_os = "linux"))]
const TIME_ERROR: i32 = 5;

/// Read-only query: `adjtimex` with no mode bits changes nothing and needs no
/// privilege.
pub fn local_clock_synchronization() -> ClockSynchronization {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: a zeroed timex is a valid read-only request (modes == 0).
        let mut timex: libc::timex = unsafe { std::mem::zeroed() };
        let state = unsafe { libc::adjtimex(&mut timex) };
        from_adjtimex(state, timex.status)
    }
    #[cfg(not(target_os = "linux"))]
    {
        ClockSynchronization::NotChecked
    }
}

#[cfg(any(test, target_os = "linux"))]
fn from_adjtimex(state: i32, status: i32) -> ClockSynchronization {
    if state < 0 {
        ClockSynchronization::NotChecked
    } else if state == TIME_ERROR || status & STA_UNSYNC != 0 {
        ClockSynchronization::Unsynchronized
    } else {
        ClockSynchronization::Synchronized
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kernel_status_maps_to_synchronization() {
        assert_eq!(from_adjtimex(0, 0), ClockSynchronization::Synchronized);
        assert_eq!(
            from_adjtimex(0, STA_UNSYNC),
            ClockSynchronization::Unsynchronized
        );
        assert_eq!(
            from_adjtimex(TIME_ERROR, 0),
            ClockSynchronization::Unsynchronized
        );
        assert_eq!(from_adjtimex(-1, 0), ClockSynchronization::NotChecked);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_query_reads_kernel_status_without_privilege() {
        assert_ne!(
            local_clock_synchronization(),
            ClockSynchronization::NotChecked
        );
    }
}
