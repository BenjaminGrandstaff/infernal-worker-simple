# infernal-worker-simple

A minimal reference worker for the
[infernal-law](https://github.com/BenjaminGrandstaff/infernal-law) governance
kernel's governed-work vertical slice.

## What this is

The infernal-law kernel routes accepted Requests to destination services
through durable, claimable, fenced work items (ILK-003/ILK-010/ILK-011). It
deliberately does not know what any of that work *means* — a worker receives
governed work through the kernel, decides what to do with it, and reports
completion back through the kernel. `infernal-worker-simple` is the reference
worker that proves this loop end to end without encoding any real business
domain logic, the same way
[`infernal-inquisitor-simple`](https://github.com/BenjaminGrandstaff/infernal-inquisitor-simple)'s
policy is deliberately trivial ("allow if a grant matched") rather than real
policy.

It repeatedly:

1. calls `GET /v1/routes/eligible`, which returns every route currently
   assigned to this service's own verified identity that has no live,
   unexpired claim, already ordered oldest-first;
2. proposes a claim for the oldest one via
   `POST /v1/routes/{route_id}/claims`;
3. reads the request behind that route via
   `GET /v1/routes/{route_id}/request` — this is what actually tells the
   worker what it was asked to do, since neither the eligible-route listing
   nor a claim response carries the request's action, scope, or schema;
4. "executes" it — a deliberately trivial placeholder that only
   acknowledges what it received, since business meaning is domain-owned,
   never this reference service's; and
5. completes the claim via `POST /v1/claims/{id}/complete`.

A lost claim race (`409`), an unowned/unknown route (`404` on any of the
above), or a fencing loss between claiming and completing are not errors —
they are the kernel correctly enforcing an invariant this worker must simply
accept and move on from.

## Why this worker claims its own work

`infernal-taskmaster-simple` is the reference *scheduler*: it also calls
`GET /v1/routes/eligible` and proposes claims, deciding which eligible route
should run next (see
[ADR-0011](https://github.com/BenjaminGrandstaff/infernal-law/blob/main/docs/architecture/decisions/0011-move-scheduling-policy-outside-the-kernel.md)).
But the kernel takes both `worker_service` and `worker_instance` from
whichever caller signs the claim request — never from a body field — so
there is no way for one process to claim work and hand it to a different
process to complete. Whatever claims a route must also be what completes it.

This worker therefore performs the full loop itself rather than waiting for
a proposal from a separate scheduler process. Both reference services prove
the same kernel contract, from two different vantage points: Taskmaster
proves an external scheduler can call it, and this worker proves a worker
can receive, execute, and complete its own claimed work. A future kernel
capability that lets a scheduler propose a claim on a worker's behalf (and a
worker separately confirm it) would let the two compose directly; today's
minimum-viable-kernel does not have that contract.

## Protocol

Every call this service makes into the kernel is signed with its own
long-lived instance credential
([ADR-0003](https://github.com/BenjaminGrandstaff/infernal-law/blob/main/docs/architecture/decisions/0003-direct-signed-service-rest.md),
[ADR-0005](https://github.com/BenjaminGrandstaff/infernal-law/blob/main/docs/architecture/decisions/0005-use-ephemeral-per-instance-service-keys.md))
using [`infernal-client-rs`](https://github.com/BenjaminGrandstaff/infernal-client-rs)'s
`SignedRequest::sign`. Per the same "only the outbound call is signed"
design [ADR-0013](https://github.com/BenjaminGrandstaff/infernal-law/blob/main/docs/architecture/decisions/0013-external-stateless-policy-evaluator-for-authority.md)
uses for the kernel's own call to a policy evaluator: the kernel's JSON
response is trusted over the same HTTPS connection this service itself
opened, not by a second signature. `src/kernel_client.rs` splits building a
signed request from sending it, so the signing logic is independently
verified (against `infernal-client-rs`'s own `verify_incoming`) without a
live kernel connection; `src/worker.rs`'s receive/execute/complete loop sits
behind a `KernelPort` trait so it is proven against a fake kernel, not a
real one.

## Configuration

- `KERNEL_AUTHORITY` (required) — the kernel's host (and, if needed, port),
  for example `kernel.example.test`. Never a scheme or path; this is also
  the actual address `infernal-client-rs` connects to (always over HTTPS).
- `WORKER_SERVICE_ID` (required) — this service's own `service_id`, as a
  UUID. Must already be provisioned as an `identities` row and enrolled with
  the kernel (ADR-0008) before any call this process signs will be accepted,
  and before any route will ever be eligible for it — deployment
  configuration, not something this scaffold performs itself.
- `CLAIM_LEASE_SECONDS` (default `300`) — the lease duration proposed with
  each claim.
- `POLL_INTERVAL_SECONDS` (default `5`) — how often to poll
  `GET /v1/routes/eligible`.

## Status

The eligible-route, claim, routed-request, and completion contracts
(ILK-003/ILK-010/ILK-011) are implemented kernel-side, and this worker's
client, receive/execute/complete loop, and wire formats are implemented and
tested against them: `src/routes.rs`, `src/routed_request.rs`, and
`src/claims.rs` mirror the kernel's actual JSON shapes field-for-field;
`src/kernel_client.rs`'s signed requests are proven correct without a live
connection; and `src/worker.rs`'s loop is proven against a fake kernel port,
including that a lost claim race, an unavailable request, and a fencing loss
before completion are all reported, not treated as errors. Not yet
exercised: an actual signed round trip against a live, enrolled kernel
process (this requires real ADR-0008 Kubernetes-TokenReview enrollment and a
reachable HTTPS kernel, neither of which a unit-test sandbox can provide).

## Development

```sh
cargo build
cargo test
```

## License

MIT. See [LICENSE](LICENSE).
