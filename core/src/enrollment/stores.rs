//! One fleet store plus a store per bound membership namespace, resolved as
//! one policy. The fleet namespace binds the others; a namespace the root has
//! not bound confers nothing. For one subject, the highest revision across
//! namespaces wins and a denial wins a tie, so a denial mirrored into the
//! fleet namespace is enforced by verifiers that never received the
//! subject's membership namespace.
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use super::records::{NamespaceBindingV1, NamespacePurpose};
use super::store::{namespace_id, RegistrationView};
use super::{
    AdmissionEvidenceSource, AdmissionGuard, DenialReason, EnrollmentPolicyStore, RecordError,
    VerifiedAdmissionEvidence,
};
use crate::delegation::RevokedDelegates;

pub struct PolicyStores {
    fleet: Arc<EnrollmentPolicyStore>,
    /// Stores for the membership namespaces the fleet currently binds.
    membership: RwLock<BTreeMap<[u8; 32], Arc<EnrollmentPolicyStore>>>,
}

impl PolicyStores {
    pub fn new(fleet: Arc<EnrollmentPolicyStore>) -> Self {
        Self {
            fleet,
            membership: RwLock::new(BTreeMap::new()),
        }
    }

    pub fn fleet(&self) -> &Arc<EnrollmentPolicyStore> {
        &self.fleet
    }

    /// Open a store for every active membership binding the fleet store has
    /// accepted and drop those no longer bound. Returns the active bindings
    /// of every purpose.
    pub fn sync_bindings(&self) -> Result<Vec<NamespaceBindingV1>, RecordError> {
        let bindings = self.fleet.bindings();
        let wanted: Vec<[u8; 32]> = bindings
            .iter()
            .filter(|binding| binding.purpose == NamespacePurpose::Membership)
            .map(|binding| namespace_id(&binding.namespace))
            .collect::<Result<_, _>>()?;
        let mut membership = self.membership.write().unwrap_or_else(|e| e.into_inner());
        membership.retain(|namespace, _| wanted.contains(namespace));
        for namespace in wanted {
            if let std::collections::btree_map::Entry::Vacant(entry) = membership.entry(namespace) {
                entry.insert(Arc::new(self.fleet.open_bound(namespace)?));
            }
        }
        Ok(bindings)
    }

    /// The store for a bound membership namespace.
    pub fn membership(&self, namespace: &[u8; 32]) -> Option<Arc<EnrollmentPolicyStore>> {
        self.membership
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(namespace)
            .cloned()
    }

    fn stores(&self) -> Vec<Arc<EnrollmentPolicyStore>> {
        let mut stores = vec![self.fleet.clone()];
        stores.extend(
            self.membership
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .values()
                .cloned(),
        );
        stores
    }

    /// The winning statement about `peer` across every bound namespace.
    /// An active record that outranks a denial held in another namespace must
    /// name that denial in `reauthorizes_revision`, as it would in one
    /// namespace.
    fn resolve(
        &self,
        peer: &[u8; 32],
    ) -> Result<Option<(Arc<EnrollmentPolicyStore>, RegistrationView)>, RecordError> {
        let stores = self.stores();
        if stores.iter().any(|store| store.failed()) {
            return Err(RecordError::Denied(DenialReason::PolicyStateUnavailable));
        }
        let views: Vec<_> = stores
            .into_iter()
            .filter_map(|store| store.registration_view(peer).map(|view| (store, view)))
            .collect();
        let rank = |view: &RegistrationView| {
            (
                view.revision(),
                matches!(view, RegistrationView::Denied { .. }),
            )
        };
        let Some(winner) = views.iter().max_by_key(|(_, view)| rank(view)).cloned() else {
            return Ok(None);
        };
        if let RegistrationView::Active {
            reauthorizes_revision,
            ..
        } = winner.1
        {
            let latest_denial = views
                .iter()
                .filter_map(|(_, view)| match view {
                    RegistrationView::Denied { revision, reason } => Some((*revision, reason)),
                    RegistrationView::Active { .. } => None,
                })
                .max_by_key(|(revision, _)| *revision);
            if let Some((revision, reason)) = latest_denial {
                if reauthorizes_revision != Some(revision) {
                    return Ok(Some((
                        winner.0,
                        RegistrationView::Denied {
                            revision,
                            reason: reason.clone(),
                        },
                    )));
                }
            }
        }
        Ok(Some(winner))
    }

    /// [`EnrollmentPolicyStore::select_evidence`] across namespaces: the
    /// winning registration replaces a presented chain, a winning denial is
    /// final, and with no registration anywhere the chain is checked against
    /// the fleet's credential and issuer revocations.
    pub fn select_evidence(
        &self,
        peer: &[u8; 32],
        credential: Option<&VerifiedAdmissionEvidence>,
        now: u64,
    ) -> Result<VerifiedAdmissionEvidence, RecordError> {
        match self.resolve(peer)? {
            Some((store, RegistrationView::Active { .. })) => {
                store.select_evidence(peer, None, now)
            }
            Some((_, RegistrationView::Denied { reason, .. })) => Err(RecordError::Denied(reason)),
            None => self.fleet.select_evidence(peer, credential, now),
        }
    }
}

impl AdmissionGuard for PolicyStores {
    fn denial(&self, evidence: &VerifiedAdmissionEvidence, now: u64) -> Option<DenialReason> {
        let resolved = match self.resolve(evidence.subject()) {
            Ok(resolved) => resolved,
            Err(RecordError::Denied(reason)) => return Some(reason),
            Err(_) => return Some(DenialReason::PolicyStateUnavailable),
        };
        match (evidence.source(), resolved) {
            (_, Some((_, RegistrationView::Denied { reason, .. }))) => Some(reason),
            (
                AdmissionEvidenceSource::RootAuthoredRecord {
                    document_id,
                    revision,
                },
                Some((
                    store,
                    RegistrationView::Active {
                        revision: current, ..
                    },
                )),
            ) => {
                if store.namespace() != &document_id || current != revision {
                    return Some(DenialReason::RegistrationSuperseded);
                }
                store.denial(evidence, now)
            }
            (AdmissionEvidenceSource::RootAuthoredRecord { .. }, None) => {
                Some(DenialReason::MissingEvidence)
            }
            // A registration now outranks the chain this session rests on.
            (
                AdmissionEvidenceSource::PresentedChain { .. },
                Some((_, RegistrationView::Active { .. })),
            ) => Some(DenialReason::RegistrationSuperseded),
            (AdmissionEvidenceSource::PresentedChain { .. }, None) => {
                self.fleet.denial(evidence, now)
            }
        }
    }
}

/// Delegate revocations live only in the fleet namespace.
impl RevokedDelegates for PolicyStores {
    fn is_revoked(&self, key: &[u8; 32]) -> bool {
        self.fleet.issuer_revoked(key)
    }
}
