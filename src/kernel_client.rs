//! Goal: implement the outbound signed calls this worker makes into the
//! kernel's route/claim contract (ADR-0011, ILK-003/ILK-010/ILK-011),
//! signing with this process's own long-lived instance credential -- the
//! mirror image of how the kernel itself signs its outbound call to a
//! policy evaluator
//! (`src/infrastructure/http_policy_evaluator.rs` in infernal-law).
//! Building a signed request is split from sending it, so the signing
//! logic is independently verifiable without a live kernel connection, and
//! the actual calls sit behind [`KernelPort`] so the receive/execute/
//! complete loop can be proven against a fake.
//!
//! Per ADR-0013's design (mirrored here in the opposite direction): only
//! this service's outbound call is signed at the application layer. The
//! kernel's JSON response is trusted over the same HTTPS connection this
//! service itself opened, not by a second signature -- exactly like the
//! kernel trusts a policy evaluator's response.
//!
//! Every call here uses this worker's own verified identity as both
//! `worker_service` and `worker_instance` -- the kernel takes both from
//! the caller's signed request, never a body field, so there is no way
//! for one process to claim work on another's behalf. A route is only
//! ever eligible for, claimable by, and completable by the destination
//! service that is also the caller.

use std::time::{SystemTime, UNIX_EPOCH};

use infernal_client::{Client, ClientCredential, RequestParts, SignedRequest};
use uuid::Uuid;

use crate::claims::{
    ClaimOutcome, ClaimRequest, CompleteOutcome, FencedActionRequest, parse_claim_response,
    parse_complete_response,
};
use crate::error::WorkerError;
use crate::routed_request::{RoutedRequestOutcome, parse_routed_request_response};
use crate::routes::{ELIGIBLE_ROUTES_PATH, EligibleRoute, parse_eligible_routes};

const SIGNATURE_VALIDITY_SECONDS: i64 = 30;

/// The kernel operations this worker needs -- an interface boundary so
/// [`crate::worker::work_once`] can be proven against a fake, the same
/// way infernal-law's own `PolicyEvaluator` trait separates
/// `AuthorityService` from a specific transport.
pub trait KernelPort {
    fn eligible_routes(&self) -> Result<Vec<EligibleRoute>, WorkerError>;

    fn propose_claim(
        &self,
        route_id: &str,
        lease_seconds: i64,
    ) -> Result<ClaimOutcome, WorkerError>;

    fn routed_request(&self, route_id: &str) -> Result<RoutedRequestOutcome, WorkerError>;

    fn complete_claim(
        &self,
        claim_id: &str,
        fencing_token: i64,
    ) -> Result<CompleteOutcome, WorkerError>;
}

pub struct KernelClient {
    client: Client,
    credential: ClientCredential,
    authority: String,
}

impl KernelClient {
    /// `authority` is the kernel's host (and, if needed, port), for example
    /// `kernel.example.test` -- the same shape as an HTTP `Host` header,
    /// never including a scheme or path.
    pub fn new(
        credential: ClientCredential,
        authority: impl Into<String>,
    ) -> Result<Self, WorkerError> {
        Ok(Self {
            client: Client::new()?,
            credential,
            authority: authority.into(),
        })
    }
}

impl KernelPort for KernelClient {
    fn eligible_routes(&self) -> Result<Vec<EligibleRoute>, WorkerError> {
        let signed = build_get(
            &self.credential,
            &self.authority,
            ELIGIBLE_ROUTES_PATH,
            Uuid::new_v4(),
            unix_time(),
        )?;
        let response = self.client.send(&signed)?;
        parse_eligible_routes(response.status, &response.body)
    }

    fn propose_claim(
        &self,
        route_id: &str,
        lease_seconds: i64,
    ) -> Result<ClaimOutcome, WorkerError> {
        let path = format!("/v1/routes/{route_id}/claims");
        let body = serde_json::to_vec(&ClaimRequest { lease_seconds })
            .map_err(|error| WorkerError::MalformedResponse(error.to_string()))?;
        let signed = build_post(
            &self.credential,
            &self.authority,
            &path,
            &body,
            Uuid::new_v4(),
            unix_time(),
        )?;
        let response = self.client.send(&signed)?;
        parse_claim_response(response.status, &response.body)
    }

    fn routed_request(&self, route_id: &str) -> Result<RoutedRequestOutcome, WorkerError> {
        let path = format!("/v1/routes/{route_id}/request");
        let signed = build_get(
            &self.credential,
            &self.authority,
            &path,
            Uuid::new_v4(),
            unix_time(),
        )?;
        let response = self.client.send(&signed)?;
        parse_routed_request_response(response.status, &response.body)
    }

    fn complete_claim(
        &self,
        claim_id: &str,
        fencing_token: i64,
    ) -> Result<CompleteOutcome, WorkerError> {
        let path = format!("/v1/claims/{claim_id}/complete");
        let body = serde_json::to_vec(&FencedActionRequest { fencing_token })
            .map_err(|error| WorkerError::MalformedResponse(error.to_string()))?;
        let signed = build_post(
            &self.credential,
            &self.authority,
            &path,
            &body,
            Uuid::new_v4(),
            unix_time(),
        )?;
        let response = self.client.send(&signed)?;
        parse_complete_response(response.status, &response.body)
    }
}

fn build_get(
    credential: &ClientCredential,
    authority: &str,
    path: &str,
    request_id: Uuid,
    now: i64,
) -> Result<SignedRequest, WorkerError> {
    let parts = RequestParts::new("GET", authority, path, "application/json", &[], request_id)?;
    sign(credential, parts, now)
}

fn build_post(
    credential: &ClientCredential,
    authority: &str,
    path: &str,
    body: &[u8],
    request_id: Uuid,
    now: i64,
) -> Result<SignedRequest, WorkerError> {
    let parts = RequestParts::new(
        "POST",
        authority,
        path,
        "application/json",
        body,
        request_id,
    )?;
    sign(credential, parts, now)
}

fn sign(
    credential: &ClientCredential,
    parts: RequestParts,
    now: i64,
) -> Result<SignedRequest, WorkerError> {
    let nonce = infernal_client::generate_nonce()?;
    Ok(SignedRequest::sign(
        parts,
        credential,
        now,
        now + SIGNATURE_VALIDITY_SECONDS,
        &nonce,
    )?)
}

fn unix_time() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    )
    .unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use infernal_client::{IncomingRequest, verify_incoming};

    use super::*;

    fn incoming_from(signed: &SignedRequest) -> IncomingRequest {
        IncomingRequest::from_wire(
            signed.parts().clone(),
            &signed.service_id().to_string(),
            &signed.instance_id().to_string(),
            signed.content_digest(),
            signed.signature_input(),
            signed.signature(),
        )
        .unwrap()
    }

    #[test]
    fn the_eligible_routes_request_verifies_under_its_own_public_key() {
        let credential = ClientCredential::generate(Uuid::new_v4());

        let signed = build_get(
            &credential,
            "kernel.example.test",
            ELIGIBLE_ROUTES_PATH,
            Uuid::new_v4(),
            1_000,
        )
        .unwrap();

        assert_eq!(signed.parts().method(), "GET");
        assert_eq!(signed.parts().path_and_query(), ELIGIBLE_ROUTES_PATH);
        assert!(signed.parts().body().is_empty());
        let verified =
            verify_incoming(&incoming_from(&signed), credential.public_key(), 1_005).unwrap();
        assert_eq!(verified.service_id(), credential.public_key().service_id());
    }

    #[test]
    fn the_claim_request_targets_the_right_route_and_carries_the_lease() {
        let credential = ClientCredential::generate(Uuid::new_v4());
        let body = serde_json::to_vec(&ClaimRequest { lease_seconds: 300 }).unwrap();

        let signed = build_post(
            &credential,
            "kernel.example.test",
            "/v1/routes/route-42/claims",
            &body,
            Uuid::new_v4(),
            1_000,
        )
        .unwrap();

        assert_eq!(signed.parts().method(), "POST");
        assert_eq!(
            signed.parts().path_and_query(),
            "/v1/routes/route-42/claims"
        );
        assert_eq!(signed.parts().body(), br#"{"lease_seconds":300}"#);
        let verified =
            verify_incoming(&incoming_from(&signed), credential.public_key(), 1_005).unwrap();
        assert_eq!(verified.service_id(), credential.public_key().service_id());
    }

    #[test]
    fn the_routed_request_read_targets_the_right_route() {
        let credential = ClientCredential::generate(Uuid::new_v4());

        let signed = build_get(
            &credential,
            "kernel.example.test",
            "/v1/routes/route-42/request",
            Uuid::new_v4(),
            1_000,
        )
        .unwrap();

        assert_eq!(signed.parts().method(), "GET");
        assert_eq!(
            signed.parts().path_and_query(),
            "/v1/routes/route-42/request"
        );
        verify_incoming(&incoming_from(&signed), credential.public_key(), 1_005).unwrap();
    }

    #[test]
    fn the_complete_request_targets_the_right_claim_and_carries_the_fencing_token() {
        let credential = ClientCredential::generate(Uuid::new_v4());
        let body = serde_json::to_vec(&FencedActionRequest { fencing_token: 7 }).unwrap();

        let signed = build_post(
            &credential,
            "kernel.example.test",
            "/v1/claims/claim-9/complete",
            &body,
            Uuid::new_v4(),
            1_000,
        )
        .unwrap();

        assert_eq!(signed.parts().method(), "POST");
        assert_eq!(
            signed.parts().path_and_query(),
            "/v1/claims/claim-9/complete"
        );
        assert_eq!(signed.parts().body(), br#"{"fencing_token":7}"#);
        verify_incoming(&incoming_from(&signed), credential.public_key(), 1_005).unwrap();
    }

    #[test]
    fn a_tampered_body_fails_verification() {
        let credential = ClientCredential::generate(Uuid::new_v4());
        let body = serde_json::to_vec(&FencedActionRequest { fencing_token: 7 }).unwrap();
        let signed = build_post(
            &credential,
            "kernel.example.test",
            "/v1/claims/claim-9/complete",
            &body,
            Uuid::new_v4(),
            1_000,
        )
        .unwrap();
        let tampered_parts = RequestParts::new(
            signed.parts().method(),
            signed.parts().authority(),
            signed.parts().path_and_query(),
            signed.parts().content_type(),
            br#"{"fencing_token":999}"#,
            signed.parts().request_id(),
        )
        .unwrap();
        let tampered = IncomingRequest::from_wire(
            tampered_parts,
            &signed.service_id().to_string(),
            &signed.instance_id().to_string(),
            signed.content_digest(),
            signed.signature_input(),
            signed.signature(),
        )
        .unwrap();

        assert!(verify_incoming(&tampered, credential.public_key(), 1_005).is_err());
    }
}
