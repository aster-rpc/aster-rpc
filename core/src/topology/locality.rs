//! Declared locality: operator-known zone and region, signed by the swarm's
//! root (directly or through a delegated issuer) as a locality statement
//! bound to one endpoint, published in the shared topology doc, and folded
//! into the ladder as a bound on the measured level.
//!
//! Design: `docs/_internal/aster-network-topology.md` §"Declared locality".
//! This module is the pure part: value rules, the ladder bound, and the
//! callback types the shared engine uses to read this node's statement and
//! verify a peer's. Verification itself lives with enrollment, which supplies
//! the callbacks, so topology does not depend on the enrollment feature.

use std::fmt;
use std::sync::Arc;

use super::CoreLadderLevel;

/// Reserved enrollment attribute naming a node's region.
pub const REGION_ATTRIBUTE: &str = "aster.locality.region";
/// Reserved enrollment attribute naming a node's zone within its region.
pub const ZONE_ATTRIBUTE: &str = "aster.locality.zone";
/// Longest accepted region or zone value, in bytes.
pub const MAX_LOCALITY_VALUE_LEN: usize = 64;

/// A verified zone/region declaration. A zone is meaningful only within its
/// region.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoreDeclaredLocality {
    pub region: String,
    pub zone: Option<String>,
}

impl CoreDeclaredLocality {
    /// Build a declaration from the two attribute values. A region is
    /// required; any malformed value makes the whole declaration absent, so a
    /// bad zone can never silently widen to a region-only declaration.
    pub fn from_values(region: Option<&str>, zone: Option<&str>) -> Option<Self> {
        let region = region.filter(|r| is_valid_locality_value(r))?;
        match zone {
            Some(z) if !is_valid_locality_value(z) => None,
            zone => Some(Self {
                region: region.to_string(),
                zone: zone.map(str::to_string),
            }),
        }
    }
}

/// 1–64 bytes of `[a-z0-9._-]`: wide enough for cloud zone names
/// (`eu-west-1a`, `europe-west4-b`), narrow enough to compare exactly.
pub fn is_valid_locality_value(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_LOCALITY_VALUE_LEN
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
}

/// Reserved locality attributes are present but malformed: not strings, an
/// invalid value, or a zone without a region.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MalformedLocality;

impl fmt::Display for MalformedLocality {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("malformed aster.locality.* attributes")
    }
}

impl std::error::Error for MalformedLocality {}

/// Where a reported level came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoreLevelSource {
    /// This node's own measurement of the path.
    Measured,
    /// Both ends' verified declarations, which bounded or replaced the
    /// measurement.
    Declared,
    /// Shared cluster membership (topology v2), for a peer this node has not
    /// measured and that has no declaration.
    Shared,
}

impl CoreLevelSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Measured => "measured",
            Self::Declared => "declared",
            Self::Shared => "shared",
        }
    }
}

/// A declaration that verified for one endpoint, and until when.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoreVerifiedLocality {
    pub declared: CoreDeclaredLocality,
    /// Unix seconds at which the evidence stops being valid (the
    /// statement's earliest `exp`); `None` for evidence without expiry,
    /// such as an open-ended registration record.
    pub expires_at: Option<u64>,
}

impl CoreVerifiedLocality {
    /// Whether the evidence is still valid at `now` (Unix seconds).
    pub fn is_current(&self, now: u64) -> bool {
        self.expires_at.is_none_or(|exp| now < exp)
    }
}

/// Returns this node's current locality statement (an `aster-chain-v1`
/// chain whose leaf is a locality link), or `None` when it holds none (no
/// declaration in its credential, or admitted by a registration record).
pub type OwnStatementFn = Arc<dyn Fn() -> Option<String> + Send + Sync>;

/// Verifies `statement` for the endpoint `node_hex` and returns its
/// declaration. An empty `statement` asks for the node's root-authored
/// registration record instead. Anything that does not verify is `None`.
pub type LocalityVerifyFn = Arc<dyn Fn(&str, &str) -> Option<CoreVerifiedLocality> + Send + Sync>;

/// How the shared engine reads this node's statement and verifies peers'.
#[derive(Clone)]
pub struct CoreLocalitySource {
    pub own_statement: OwnStatementFn,
    pub verify: LocalityVerifyFn,
}

impl fmt::Debug for CoreLocalitySource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CoreLocalitySource(<fn>, <fn>)")
    }
}

/// Fold both ends' declarations into a measured level.
///
/// Applies only when both ends declare. Different regions are `Far`; a
/// declared different zone in the same region is exactly `SameRegion`; the
/// same zone is at least `SameSite`; the same region with a zone missing on
/// either side is at least `SameRegion`. A measurement closer than the bound
/// is kept (it can only refine within the bound). With no measurement the
/// declaration alone gives the level.
pub fn bound_level(
    measured: Option<(CoreLadderLevel, String)>,
    mine: Option<&CoreDeclaredLocality>,
    theirs: Option<&CoreDeclaredLocality>,
) -> Option<(CoreLadderLevel, String, CoreLevelSource)> {
    let measured_only = || {
        measured
            .clone()
            .map(|(level, reason)| (level, reason, CoreLevelSource::Measured))
    };
    let (Some(mine), Some(theirs)) = (mine, theirs) else {
        return measured_only();
    };
    let declared = |level: CoreLadderLevel, reason: &str| {
        Some((level, reason.to_string(), CoreLevelSource::Declared))
    };
    if mine.region != theirs.region {
        return declared(CoreLadderLevel::Far, "declared different region");
    }
    let (cap, reason) = match (&mine.zone, &theirs.zone) {
        (Some(a), Some(b)) if a != b => {
            return declared(
                CoreLadderLevel::SameRegion,
                "declared same region, different zone",
            );
        }
        (Some(_), Some(_)) => (CoreLadderLevel::SameSite, "declared same zone"),
        _ => (CoreLadderLevel::SameRegion, "declared same region"),
    };
    match measured {
        Some((level, _)) if level < cap => measured_only(),
        _ => declared(cap, reason),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use CoreLadderLevel::*;

    fn loc(region: &str, zone: Option<&str>) -> CoreDeclaredLocality {
        CoreDeclaredLocality::from_values(Some(region), zone).unwrap()
    }

    fn level(
        measured: Option<CoreLadderLevel>,
        mine: Option<CoreDeclaredLocality>,
        theirs: Option<CoreDeclaredLocality>,
    ) -> Option<(CoreLadderLevel, CoreLevelSource)> {
        bound_level(
            measured.map(|l| (l, "measured".to_string())),
            mine.as_ref(),
            theirs.as_ref(),
        )
        .map(|(l, _, s)| (l, s))
    }

    #[test]
    fn values_are_validated() {
        assert!(is_valid_locality_value("eu-west-1a"));
        assert!(is_valid_locality_value("europe-west4-b"));
        assert!(!is_valid_locality_value(""));
        assert!(!is_valid_locality_value("EU-West"));
        assert!(!is_valid_locality_value("zone a"));
        assert!(!is_valid_locality_value(&"a".repeat(65)));
        assert_eq!(CoreDeclaredLocality::from_values(None, Some("a")), None);
        // A malformed zone voids the declaration rather than widening it.
        assert_eq!(
            CoreDeclaredLocality::from_values(Some("eu-west-1"), Some("Bad Zone")),
            None
        );
        assert_eq!(
            CoreDeclaredLocality::from_values(Some("eu-west-1"), None),
            Some(CoreDeclaredLocality {
                region: "eu-west-1".into(),
                zone: None
            })
        );
    }

    #[test]
    fn without_both_declarations_the_measurement_stands() {
        let a = loc("r1", Some("a"));
        assert_eq!(
            level(Some(SameLan), Some(a.clone()), None),
            Some((SameLan, CoreLevelSource::Measured))
        );
        assert_eq!(
            level(Some(Far), None, Some(a)),
            Some((Far, CoreLevelSource::Measured))
        );
        assert_eq!(level(None, None, None), None);
    }

    #[test]
    fn different_region_is_far_however_fast() {
        assert_eq!(
            level(Some(SameLan), Some(loc("r1", None)), Some(loc("r2", None))),
            Some((Far, CoreLevelSource::Declared))
        );
    }

    #[test]
    fn different_zone_is_exactly_same_region() {
        let mine = Some(loc("r1", Some("a")));
        let theirs = Some(loc("r1", Some("b")));
        // A fast private path to a cross-zone peer cannot measure closer...
        assert_eq!(
            level(Some(SameLan), mine.clone(), theirs.clone()),
            Some((SameRegion, CoreLevelSource::Declared))
        );
        // ...and a slow one is still in-region.
        assert_eq!(
            level(Some(Far), mine.clone(), theirs.clone()),
            Some((SameRegion, CoreLevelSource::Declared))
        );
        assert_eq!(
            level(None, mine, theirs),
            Some((SameRegion, CoreLevelSource::Declared))
        );
    }

    #[test]
    fn same_zone_is_at_least_same_site_and_refines_closer() {
        let mine = Some(loc("r1", Some("a")));
        let theirs = Some(loc("r1", Some("a")));
        assert_eq!(
            level(Some(SameRegion), mine.clone(), theirs.clone()),
            Some((SameSite, CoreLevelSource::Declared))
        );
        assert_eq!(
            level(None, mine.clone(), theirs.clone()),
            Some((SameSite, CoreLevelSource::Declared))
        );
        assert_eq!(
            level(Some(SameLan), mine, theirs),
            Some((SameLan, CoreLevelSource::Measured))
        );
    }

    #[test]
    fn same_region_without_zones_is_at_least_same_region() {
        let mine = Some(loc("r1", Some("a")));
        let theirs = Some(loc("r1", None));
        assert_eq!(
            level(Some(Far), mine.clone(), theirs.clone()),
            Some((SameRegion, CoreLevelSource::Declared))
        );
        assert_eq!(
            level(Some(SameSite), mine, theirs),
            Some((SameSite, CoreLevelSource::Measured))
        );
    }
}
