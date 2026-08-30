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
pub mod kernel_client;
pub mod routed_request;
pub mod routes;
pub mod worker;

use std::env;
use std::time::Duration;

use infernal_client::ClientCredential;
use uuid::Uuid;

use crate::error::WorkerError;
use crate::kernel_client::KernelClient;

const KERNEL_AUTHORITY_ENV: &str = "KERNEL_AUTHORITY";
const WORKER_SERVICE_ID_ENV: &str = "WORKER_SERVICE_ID";
const CLAIM_LEASE_SECONDS_ENV: &str = "CLAIM_LEASE_SECONDS";
const POLL_INTERVAL_SECONDS_ENV: &str = "POLL_INTERVAL_SECONDS";
const DEFAULT_LEASE_SECONDS: i64 = 300;
const DEFAULT_POLL_INTERVAL_SECONDS: u64 = 5;

pub struct Config {
    pub client: KernelClient,
    pub lease_seconds: i64,
    pub poll_interval: Duration,
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
        let credential = ClientCredential::generate(service_id);
        let client = KernelClient::new(credential, authority)?;
        Ok(Self {
            client,
            lease_seconds,
            poll_interval: Duration::from_secs(poll_interval_seconds),
        })
    }
}

/// Runs the receive/execute/complete loop forever: poll, work, sleep,
/// repeat. A failed pass is logged and retried on the next tick rather
/// than crashing the process -- a transient kernel or network hiccup
/// should not take a worker down entirely, and the kernel's own claim
/// arbitration is what actually has to be correct, not this loop's
/// uptime.
pub fn run(config: Config) -> ! {
    loop {
        match worker::work_once(&config.client, config.lease_seconds) {
            Ok(worker::WorkOutcome::NothingEligible) => {}
            Ok(outcome) => println!("{outcome:?}"),
            Err(error) => eprintln!("work pass failed: {error}"),
        }
        std::thread::sleep(config.poll_interval);
    }
}
