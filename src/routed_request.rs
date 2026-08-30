//! Goal: mirror the kernel's `GET /v1/routes/{route_id}/request` wire
//! format (`AcceptedRequestResponse` in `src/http/request_dto.rs` in
//! infernal-law) field-for-field -- this is the "receive" half of
//! receive -> execute -> complete: what tells this worker what it was
//! actually asked to do, since neither the eligible-route listing nor a
//! claim response carries the request's action, scope, or schema.

use serde::Deserialize;

use crate::error::WorkerError;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct RoutedRequest {
    pub request_id: String,
    pub source_service_id: String,
    pub action: String,
    pub scope: String,
    pub artifact_schema_version_id: String,
    pub permission_policy_schema_version_id: String,
    pub accepted_at: i64,
}

/// The kernel hides a route belonging to another service, or one that
/// does not exist, behind a plain `404` -- indistinguishable from any
/// other not-found, matching the convention every other governed route in
/// this contract uses.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RoutedRequestOutcome {
    Found(RoutedRequest),
    NotFound,
}

pub fn parse_routed_request_response(
    status: u16,
    body: &[u8],
) -> Result<RoutedRequestOutcome, WorkerError> {
    match status {
        200 => {
            let request: RoutedRequest = serde_json::from_slice(body)
                .map_err(|error| WorkerError::MalformedResponse(error.to_string()))?;
            Ok(RoutedRequestOutcome::Found(request))
        }
        404 => Ok(RoutedRequestOutcome::NotFound),
        other => Err(WorkerError::UnexpectedStatus(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_kernels_actual_routed_request_wire_shape() {
        let body = br#"{"request_id":"q1","source_service_id":"s1","action":"billing.invoice.submit","scope":"invoice-42","artifact_schema_version_id":"a1","permission_policy_schema_version_id":"p1","accepted_at":10}"#;

        let outcome = parse_routed_request_response(200, body).unwrap();

        assert_eq!(
            outcome,
            RoutedRequestOutcome::Found(RoutedRequest {
                request_id: "q1".to_owned(),
                source_service_id: "s1".to_owned(),
                action: "billing.invoice.submit".to_owned(),
                scope: "invoice-42".to_owned(),
                artifact_schema_version_id: "a1".to_owned(),
                permission_policy_schema_version_id: "p1".to_owned(),
                accepted_at: 10,
            })
        );
    }

    #[test]
    fn classifies_a_hidden_or_missing_route_as_not_found() {
        assert_eq!(
            parse_routed_request_response(404, br#"{"code":"claim_not_found"}"#).unwrap(),
            RoutedRequestOutcome::NotFound
        );
    }

    #[test]
    fn surfaces_any_other_status_as_an_error() {
        assert!(matches!(
            parse_routed_request_response(503, b"{}"),
            Err(WorkerError::UnexpectedStatus(503))
        ));
    }

    #[test]
    fn rejects_a_malformed_success_body() {
        assert!(matches!(
            parse_routed_request_response(200, b"not json"),
            Err(WorkerError::MalformedResponse(_))
        ));
    }
}
