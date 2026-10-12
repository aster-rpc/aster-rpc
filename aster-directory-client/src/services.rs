//! The three @aster services (`aster-app/services/aster/services/`), all
//! unary. `#[aster::service]` generates the typed clients
//! (`ProfileServiceClient`, …); this crate never implements the servers.

#![allow(clippy::too_many_arguments)]

use aster::Result;

use crate::wire::*;

#[aster::service(name = "ProfileService", version = 1)]
pub trait ProfileService {
    async fn check_availability(
        &self,
        req: CheckAvailabilityRequest,
    ) -> Result<CheckAvailabilityResult>;
    async fn join(&self, req: SignedRequest) -> Result<JoinResult>;
    async fn resend_verification(&self, req: SignedRequest) -> Result<ResendResult>;
    async fn verify(&self, req: SignedRequest) -> Result<VerifyResult>;
    async fn handle_status(&self, req: SignedRequest) -> Result<HandleStatusResult>;
    async fn update_profile(&self, req: SignedRequest) -> Result<UpdateProfileResult>;
    async fn get_profile(&self, req: GetProfileRequest) -> Result<PublicProfileResult>;
}

#[aster::service(name = "PublicationService", version = 1)]
pub trait PublicationService {
    async fn publish(&self, req: SignedRequest) -> Result<PublishResult>;
    async fn discover(&self, req: DiscoverRequest) -> Result<DiscoverResult>;
    async fn list_directory_handles(
        &self,
        req: ListDirectoryHandlesRequest,
    ) -> Result<ListDirectoryHandlesResult>;
    async fn get_manifest(&self, req: GetManifestRequest) -> Result<GetManifestResult>;
    async fn resolve(&self, req: ResolveRequest) -> Result<ResolveResult>;
    async fn list_services(&self, req: ListServicesRequest) -> Result<ListServicesResult>;
    async fn set_visibility(&self, req: SignedRequest) -> Result<VisibilityResult>;
    async fn update_service(&self, req: SignedRequest) -> Result<UpdateServiceResult>;
    async fn unpublish(&self, req: SignedRequest) -> Result<UnpublishResult>;
}

#[aster::service(name = "AccessService", version = 1)]
pub trait AccessService {
    async fn update_delegation(&self, req: SignedRequest) -> Result<UpdateDelegationResult>;
    async fn grant_access(&self, req: SignedRequest) -> Result<GrantAccessResult>;
    async fn revoke_access(&self, req: SignedRequest) -> Result<RevokeAccessResult>;
    async fn list_access(&self, req: SignedRequest) -> Result<ListAccessResult>;
}
