//! Goal: implement the one loop this reference worker exists to prove:
//! receive governed work through the kernel, "execute" it, and complete it
//! back through the kernel -- closing the minimum-viable-kernel vertical
//! slice's last open step. This worker claims its own eligible work
//! directly (FIFO, oldest first) rather than waiting for a proposal from
//! a separate scheduler process, because the kernel ties a claim to
//! whichever caller signs the claim request -- there is no delegation, so
//! whatever claims a route must also be what completes it.

use crate::claims::{ClaimOutcome, CompleteOutcome};
use crate::error::WorkerError;
use crate::kernel_client::KernelPort;
use crate::routed_request::{RoutedRequest, RoutedRequestOutcome};

/// What one receive/execute/complete pass did, for the caller (typically
/// the main loop) to log. None of the non-`Completed` variants are this
/// worker failing -- each is the kernel correctly enforcing an invariant
/// (a route already gone, a claim already lost) that this worker must
/// simply accept and move on from.
#[derive(Debug)]
pub enum WorkOutcome {
    /// No eligible route existed to claim.
    NothingEligible,
    /// The oldest eligible route was already claimed or gone by the time
    /// this worker tried to claim it -- another worker or process won the
    /// race first.
    ClaimLost { route_id: String },
    /// The route was claimed, but the request behind it was not
    /// readable -- should not happen given referential integrity, but
    /// handled without ever guessing at what to execute. The claim is
    /// left to expire rather than completed blind.
    RequestUnavailable { route_id: String },
    /// The claim was lost to fencing (superseded after this worker's
    /// lease lapsed) or disappeared between claiming and completing.
    LostBeforeCompletion { route_id: String, claim_id: String },
    /// The full loop succeeded: claimed, read, "executed", and completed.
    Completed {
        route_id: String,
        action: String,
        claim_id: String,
    },
}

/// The one placeholder "business logic" step this reference worker
/// performs. It deliberately does not interpret what `action` means --
/// that is domain-owned (infernal-law's minimum-viable-kernel spec,
/// Section 11: the kernel and its reference services own authority and
/// communication, never business semantics). This exists only to give
/// receive -> execute -> complete a real execute step to prove out, the
/// same way Inquisitor's own policy is deliberately trivial ("allow if a
/// grant matched") rather than real business policy.
fn execute(request: &RoutedRequest) -> String {
    format!(
        "acknowledged action `{}` (scope `{}`) from source {}",
        request.action, request.scope, request.source_service_id
    )
}

pub fn work_once(port: &impl KernelPort, lease_seconds: i64) -> Result<WorkOutcome, WorkerError> {
    let routes = port.eligible_routes()?;
    let Some(route) = routes.into_iter().next() else {
        return Ok(WorkOutcome::NothingEligible);
    };

    let claim = match port.propose_claim(&route.route_id, lease_seconds)? {
        ClaimOutcome::Claimed(claim) => claim,
        ClaimOutcome::AlreadyClaimed | ClaimOutcome::RouteNotFound => {
            return Ok(WorkOutcome::ClaimLost {
                route_id: route.route_id,
            });
        }
    };

    let request = match port.routed_request(&route.route_id)? {
        RoutedRequestOutcome::Found(request) => request,
        RoutedRequestOutcome::NotFound => {
            return Ok(WorkOutcome::RequestUnavailable {
                route_id: route.route_id,
            });
        }
    };

    let action = execute(&request);

    match port.complete_claim(&claim.claim_id, claim.fencing_token)? {
        CompleteOutcome::Completed(_) => Ok(WorkOutcome::Completed {
            route_id: route.route_id,
            action,
            claim_id: claim.claim_id,
        }),
        CompleteOutcome::Fenced | CompleteOutcome::NotFound => {
            Ok(WorkOutcome::LostBeforeCompletion {
                route_id: route.route_id,
                claim_id: claim.claim_id,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::claims::WorkClaim;
    use crate::routes::EligibleRoute;

    use super::*;

    #[derive(Default)]
    struct FakePort {
        routes: Vec<EligibleRoute>,
        claim_outcome: Option<ClaimOutcome>,
        request_outcome: Option<RoutedRequestOutcome>,
        complete_outcome: Option<CompleteOutcome>,
    }

    impl KernelPort for FakePort {
        fn eligible_routes(&self) -> Result<Vec<EligibleRoute>, WorkerError> {
            Ok(self.routes.clone())
        }

        fn propose_claim(
            &self,
            _route_id: &str,
            _lease_seconds: i64,
        ) -> Result<ClaimOutcome, WorkerError> {
            Ok(self
                .claim_outcome
                .clone()
                .unwrap_or(ClaimOutcome::AlreadyClaimed))
        }

        fn routed_request(&self, _route_id: &str) -> Result<RoutedRequestOutcome, WorkerError> {
            Ok(self
                .request_outcome
                .clone()
                .unwrap_or(RoutedRequestOutcome::NotFound))
        }

        fn complete_claim(
            &self,
            _claim_id: &str,
            _fencing_token: i64,
        ) -> Result<CompleteOutcome, WorkerError> {
            Ok(self
                .complete_outcome
                .clone()
                .unwrap_or(CompleteOutcome::NotFound))
        }
    }

    fn route() -> EligibleRoute {
        EligibleRoute {
            route_id: "route-1".to_owned(),
            request_id: "request-1".to_owned(),
            subscription_id: "subscription-1".to_owned(),
            destination_service_id: "destination-1".to_owned(),
            created_at: 1,
        }
    }

    fn claim() -> WorkClaim {
        WorkClaim {
            claim_id: "claim-1".to_owned(),
            route_id: "route-1".to_owned(),
            worker_service_id: "destination-1".to_owned(),
            worker_instance_id: "instance-1".to_owned(),
            fencing_token: 1,
            status: "active".to_owned(),
            claimed_at: 1,
            lease_expires_at: 301,
        }
    }

    fn routed_request() -> RoutedRequest {
        RoutedRequest {
            request_id: "request-1".to_owned(),
            source_service_id: "source-1".to_owned(),
            action: "billing.invoice.submit".to_owned(),
            scope: "invoice-42".to_owned(),
            artifact_schema_version_id: "a1".to_owned(),
            permission_policy_schema_version_id: "p1".to_owned(),
            accepted_at: 1,
        }
    }

    #[test]
    fn does_nothing_when_no_route_is_eligible() {
        let port = FakePort::default();

        let outcome = work_once(&port, 300).unwrap();

        assert!(matches!(outcome, WorkOutcome::NothingEligible));
    }

    #[test]
    fn reports_a_lost_claim_race_without_erroring() {
        let port = FakePort {
            routes: vec![route()],
            claim_outcome: Some(ClaimOutcome::AlreadyClaimed),
            ..FakePort::default()
        };

        let outcome = work_once(&port, 300).unwrap();

        assert!(matches!(outcome, WorkOutcome::ClaimLost { route_id } if route_id == "route-1"));
    }

    #[test]
    fn reports_an_unavailable_request_without_completing_blind() {
        let port = FakePort {
            routes: vec![route()],
            claim_outcome: Some(ClaimOutcome::Claimed(claim())),
            request_outcome: Some(RoutedRequestOutcome::NotFound),
            ..FakePort::default()
        };

        let outcome = work_once(&port, 300).unwrap();

        assert!(
            matches!(outcome, WorkOutcome::RequestUnavailable { route_id } if route_id == "route-1")
        );
    }

    #[test]
    fn reports_fencing_loss_before_completion_without_erroring() {
        let port = FakePort {
            routes: vec![route()],
            claim_outcome: Some(ClaimOutcome::Claimed(claim())),
            request_outcome: Some(RoutedRequestOutcome::Found(routed_request())),
            complete_outcome: Some(CompleteOutcome::Fenced),
        };

        let outcome = work_once(&port, 300).unwrap();

        assert!(matches!(
            outcome,
            WorkOutcome::LostBeforeCompletion { route_id, claim_id }
                if route_id == "route-1" && claim_id == "claim-1"
        ));
    }

    #[test]
    fn completes_the_full_receive_execute_complete_loop() {
        let port = FakePort {
            routes: vec![route()],
            claim_outcome: Some(ClaimOutcome::Claimed(claim())),
            request_outcome: Some(RoutedRequestOutcome::Found(routed_request())),
            complete_outcome: Some(CompleteOutcome::Completed(WorkClaim {
                status: "completed".to_owned(),
                ..claim()
            })),
        };

        let outcome = work_once(&port, 300).unwrap();

        match outcome {
            WorkOutcome::Completed {
                route_id,
                action,
                claim_id,
            } => {
                assert_eq!(route_id, "route-1");
                assert_eq!(claim_id, "claim-1");
                assert!(action.contains("billing.invoice.submit"));
                assert!(action.contains("invoice-42"));
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }
}
