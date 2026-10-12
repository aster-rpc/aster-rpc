//! Shared monotonic revision and denial-barrier rules for root-authored leaves.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootRevisionError {
    Stale,
    Conflict,
    ReauthorizationRequired,
}

/// `same_content` is relevant only at equal revisions. A consumer determines
/// when a new positive record reopens a prior denial or expiry and passes the
/// exact revision that must be named by `reauthorizes_revision`.
pub fn check_root_revision(
    previous_revision: Option<u64>,
    denial_watermark: Option<u64>,
    incoming_revision: u64,
    same_content: bool,
    required_predecessor: Option<u64>,
    reauthorizes_revision: Option<u64>,
) -> Result<bool, RootRevisionError> {
    if incoming_revision == 0
        || denial_watermark.is_some_and(|known| incoming_revision <= known)
        || previous_revision.is_some_and(|known| incoming_revision < known)
    {
        return Err(RootRevisionError::Stale);
    }
    if previous_revision == Some(incoming_revision) {
        return if same_content {
            Ok(false)
        } else {
            Err(RootRevisionError::Conflict)
        };
    }
    if required_predecessor.is_some() && reauthorizes_revision != required_predecessor {
        return Err(RootRevisionError::ReauthorizationRequired);
    }
    Ok(true)
}

pub fn should_record_rejection(
    previous_revision: Option<u64>,
    denial_watermark: Option<u64>,
    rejected_revision: u64,
) -> bool {
    rejected_revision > 0
        && previous_revision.is_none_or(|known| known <= rejected_revision)
        && denial_watermark.is_none_or(|known| known < rejected_revision)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monotonic_and_exact_reauthorization_rules() {
        assert_eq!(
            check_root_revision(Some(3), None, 3, true, None, None),
            Ok(false)
        );
        assert_eq!(
            check_root_revision(Some(3), None, 3, false, None, None),
            Err(RootRevisionError::Conflict)
        );
        assert_eq!(
            check_root_revision(Some(3), Some(4), 4, false, Some(4), None),
            Err(RootRevisionError::Stale)
        );
        assert_eq!(
            check_root_revision(Some(3), None, 5, false, Some(3), None),
            Err(RootRevisionError::ReauthorizationRequired)
        );
        assert_eq!(
            check_root_revision(Some(3), None, 5, false, Some(3), Some(3)),
            Ok(true)
        );
        assert_eq!(
            check_root_revision(Some(3), Some(4), 5, false, Some(4), Some(3)),
            Err(RootRevisionError::ReauthorizationRequired)
        );
        assert_eq!(
            check_root_revision(Some(3), Some(4), 5, false, Some(4), Some(4)),
            Ok(true)
        );
    }
}
