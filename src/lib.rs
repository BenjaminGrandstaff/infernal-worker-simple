//! Reference worker for the infernal-law kernel's governed-work vertical
//! slice: claims its own eligible work (ILK-010/ILK-011), reads the
//! request behind a claimed route (ILK-003's destination-scoped read),
//! "executes" it (a deliberately trivial placeholder -- business meaning
//! is domain-owned, never this reference service's), and completes it
//! back through the kernel. Owns no authoritative state of its own --
//! every signal it acts on comes from an authenticated kernel read or
//! write, and the kernel remains the sole arbiter of whether a claim or
//! completion actually succeeds.
//!
//! This worker claims its own eligible work directly rather than waiting
//! for a proposal from a separate scheduler process
//! (`infernal-taskmaster-simple`): the kernel ties every claim to
//! whichever caller signs the claim request, with no delegation, so
//! whatever claims a route must also be what completes it. Both
//! reference services prove the same kernel contract from different
//! vantage points.

pub mod claims;
pub mod error;
pub mod instance_lease;
pub mod kernel_client;
pub mod routed_request;
pub mod routes;
pub mod worker;

use std::env;
use std::time::Duration;

use infernal_client::ClientCredential;
use uuid::Uuid;

use crate::error::WorkerError;
use crate::instance_lease::RENEWAL_MARGIN_SECONDS;
use crate::kernel_client::KernelClient;

const KERNEL_AUTHORITY_ENV: &str = "KERNEL_AUTHORITY";
const WORKER_SERVICE_ID_ENV: &str = "WORKER_SERVICE_ID";
const CLAIM_LEASE_SECONDS_ENV: &str = "CLAIM_LEASE_SECONDS";
const POLL_INTERVAL_SECONDS_ENV: &str = "POLL_INTERVAL_SECONDS";
/// Path to a PEM-encoded certificate authority this process should trust
/// in addition to the default public root store, for a kernel reachable
/// only behind a private or self-signed CA. Optional: a kernel with an
/// ordinary publicly-trusted certificate needs no configuration here.
const KERNEL_CA_CERT_PATH_ENV: &str = "KERNEL_CA_CERT_PATH";
/// A base64url-encoded, 32-byte ADR-0008 enrollment challenge, from a
/// kernel operator's own out-of-band challenge issuance -- infernal-law has
/// no self-service HTTP call for requesting one (see
/// `infernal_client::EnrollmentSubmission`'s own documentation for why).
/// Optional: unset entirely if this process's identity was already
/// enrolled some other way (or does not need to be, for a kernel not
/// requiring ADR-0008 enrollment). When set, `SERVICE_ENDPOINT` and
/// `POD_UID` become required.
const ENROLLMENT_CHALLENGE_ENV: &str = "ENROLLMENT_CHALLENGE";
/// This process's own HTTPS endpoint, submitted as part of the enrollment
/// proof. This service has no inbound listener of its own (it only ever
/// makes outbound calls), so nothing currently connects to this address --
/// it is recorded by the kernel as instance metadata, not verified for
/// reachability at enrollment time.
const SERVICE_ENDPOINT_ENV: &str = "SERVICE_ENDPOINT";
/// This Pod's own UID, for example from the Kubernetes Downward API
/// (`fieldRef: metadata.uid`) -- must match the Pod UID Kubernetes binds to
/// the workload token at `WORKLOAD_TOKEN_PATH`.
const POD_UID_ENV: &str = "POD_UID";
/// Path to this Pod's own projected ServiceAccount token for the
/// `infernal-law-enrollment` audience.
const WORKLOAD_TOKEN_PATH_ENV: &str = "WORKLOAD_TOKEN_PATH";
const DEFAULT_WORKLOAD_TOKEN_PATH: &str = "/var/run/secrets/infernal-law-enrollment/token";
/// How long to sleep between claiming a route and completing it -- zero
/// (instant) by default, since the placeholder "execute" step has no real
/// work to do. Set this to deliberately create a window in which this
/// process can be killed mid-claim, for example to exercise reclaim and
/// fencing against a real lease expiry rather than only in a unit test
/// with synthetic timestamps.
const WORK_DURATION_SECONDS_ENV: &str = "WORK_DURATION_SECONDS";
const DEFAULT_LEASE_SECONDS: i64 = 300;
const DEFAULT_POLL_INTERVAL_SECONDS: u64 = 5;

pub struct Config {
    pub client: KernelClient,
    pub lease_seconds: i64,
    pub poll_interval: Duration,
    pub work_duration: Duration,
    /// This process's own instance lease, tracked only when this process
    /// performed its own enrollment at startup (see `run`'s renewal
    /// logic). `None` when `ENROLLMENT_CHALLENGE` was unset because this
    /// identity was already enrolled some other way -- there is no way to
    /// discover another process's enrollment's current lease state after
    /// the fact, so such a process cannot renew and simply keeps today's
    /// behavior of failing once its lease expires.
    pub instance_lease: Option<InstanceLease>,
}

/// This process's own registration lease with the kernel, tracked entirely
/// client-side from the last enrollment or renewal response so `run` knows
/// when to renew next and what revision to renew with.
#[derive(Clone, Copy, Debug)]
pub struct InstanceLease {
    pub revision: i64,
    pub expires_at: i64,
}

impl Config {
    /// `WORKER_SERVICE_ID` names a `service_id` that must already be
    /// provisioned and enrolled with the kernel (an `identities` row, plus
    /// the real ADR-0008 Kubernetes-TokenReview enrollment for this
    /// process's freshly generated instance key) before any call this
    /// process signs will be accepted, and before any route will ever be
    /// eligible for it -- deployment configuration, not something this
    /// scaffold performs itself.
    pub fn from_env() -> Result<Self, WorkerError> {
        let authority = env::var(KERNEL_AUTHORITY_ENV)
            .map_err(|_| WorkerError::MissingEnv(KERNEL_AUTHORITY_ENV))?;
        let service_id: Uuid = env::var(WORKER_SERVICE_ID_ENV)
            .map_err(|_| WorkerError::MissingEnv(WORKER_SERVICE_ID_ENV))?
            .parse()
            .map_err(|_| WorkerError::InvalidServiceId)?;
        let lease_seconds = env::var(CLAIM_LEASE_SECONDS_ENV)
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(DEFAULT_LEASE_SECONDS);
        let poll_interval_seconds = env::var(POLL_INTERVAL_SECONDS_ENV)
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(DEFAULT_POLL_INTERVAL_SECONDS);
        let work_duration_seconds = env::var(WORK_DURATION_SECONDS_ENV)
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        let credential = ClientCredential::generate(service_id);
        let client = match env::var(KERNEL_CA_CERT_PATH_ENV) {
            Ok(path) => {
                let pem = std::fs::read(&path).map_err(WorkerError::CaCertificateUnreadable)?;
                KernelClient::with_extra_root_certificate(credential, authority, &pem)?
            }
            Err(_) => KernelClient::new(credential, authority)?,
        };
        let mut instance_lease = None;
        if let Ok(challenge) = env::var(ENROLLMENT_CHALLENGE_ENV) {
            let endpoint = env::var(SERVICE_ENDPOINT_ENV)
                .map_err(|_| WorkerError::MissingEnv(SERVICE_ENDPOINT_ENV))?;
            let pod_uid =
                env::var(POD_UID_ENV).map_err(|_| WorkerError::MissingEnv(POD_UID_ENV))?;
            let token_path = env::var(WORKLOAD_TOKEN_PATH_ENV)
                .unwrap_or_else(|_| DEFAULT_WORKLOAD_TOKEN_PATH.to_owned());
            let workload_token = std::fs::read_to_string(&token_path)
                .map_err(WorkerError::EnrollmentTokenUnreadable)?
                .trim()
                .to_owned();
            let challenge = decode_challenge(&challenge)?;
            let enrolled = client.enroll(challenge, &endpoint, &pod_uid, workload_token)?;
            println!("enrolled with the kernel: {enrolled:?}");
            instance_lease = Some(InstanceLease {
                revision: enrolled.lease_revision,
                expires_at: enrolled.lease_expires_at,
            });
        }
        Ok(Self {
            client,
            lease_seconds,
            poll_interval: Duration::from_secs(poll_interval_seconds),
            work_duration: Duration::from_secs(work_duration_seconds),
            instance_lease,
        })
    }
}

fn decode_challenge(value: &str) -> Result<[u8; infernal_client::CHALLENGE_LENGTH], WorkerError> {
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| WorkerError::InvalidEnrollmentChallenge)?
        .try_into()
        .map_err(|_| WorkerError::InvalidEnrollmentChallenge)
}

/// Runs the receive/execute/complete loop forever: poll, work, sleep,
/// repeat. A failed pass is logged and retried on the next tick rather
/// than crashing the process -- a transient kernel or network hiccup
/// should not take a worker down entirely, and the kernel's own claim
/// arbitration is what actually has to be correct, not this loop's
/// uptime.
pub fn run(config: Config) -> ! {
    let Config {
        client,
        lease_seconds,
        poll_interval,
        work_duration,
        mut instance_lease,
    } = config;
    loop {
        renew_lease_if_due(&client, &mut instance_lease);
        match worker::work_once(&client, lease_seconds, work_duration) {
            Ok(worker::WorkOutcome::NothingEligible) => {}
            Ok(outcome) => println!("{outcome:?}"),
            Err(error) => eprintln!("work pass failed: {error}"),
        }
        std::thread::sleep(poll_interval);
    }
}

/// Renews this process's own instance lease well before the kernel's
/// grant expires -- see `InstanceLease`'s own documentation for why this
/// is only possible when this process performed its own enrollment at
/// startup. A failed renewal is logged and retried on the next tick, the
/// same tolerance `run`'s own work-pass loop already has for a transient
/// kernel or network hiccup; if every attempt fails before the lease
/// actually expires, every subsequent signed call -- including the next
/// renewal attempt -- starts failing until this process restarts and
/// re-enrolls, exactly as it always has.
fn renew_lease_if_due(client: &KernelClient, instance_lease: &mut Option<InstanceLease>) {
    let Some(lease) = instance_lease else {
        return;
    };
    if unix_time() < lease.expires_at - RENEWAL_MARGIN_SECONDS {
        return;
    }
    match client.renew_lease(lease.revision) {
        Ok(renewed) => {
            lease.revision = renewed.lease_revision;
            lease.expires_at = renewed.lease_expires_at;
            println!("renewed instance lease: {renewed:?}");
        }
        Err(error) => eprintln!("instance lease renewal failed: {error}"),
    }
}

fn unix_time() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    )
    .unwrap_or(i64::MAX)
}
