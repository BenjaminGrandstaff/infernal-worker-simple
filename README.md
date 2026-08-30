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
- `KERNEL_CA_CERT_PATH` (optional) — path to a PEM-encoded certificate
  authority to trust in addition to the default public root store, for a
  kernel reachable only behind a private or self-signed certificate (for
  example `infernal-law`'s own TLS-terminating sidecar in a local or test
  cluster — see that repo's README). Omit for a kernel with an ordinary
  publicly-trusted certificate.

## Status

The eligible-route, claim, routed-request, and completion contracts
(ILK-003/ILK-010/ILK-011) are implemented kernel-side, and this worker's
client, receive/execute/complete loop, and wire formats are implemented and
tested against them: `src/routes.rs`, `src/routed_request.rs`, and
`src/claims.rs` mirror the kernel's actual JSON shapes field-for-field;
`src/kernel_client.rs`'s signed requests are proven correct without a live
connection; and `src/worker.rs`'s loop is proven against a fake kernel port,
including that a lost claim race, an unavailable request, and a fencing loss
before completion are all reported, not treated as errors.

Deployed into a real Kubernetes cluster alongside `infernal-law` and
`infernal-inquisitor-simple` and confirmed to complete a real signed
HTTPS call to the kernel end to end: with `KERNEL_CA_CERT_PATH` pointed at
`infernal-law`'s TLS-terminating sidecar certificate (see that repo's
README), this service's request reaches the kernel, passes signature
verification, and receives the kernel's correct, well-formed `401` for an
identity that is not yet enrolled — not a transport or TLS error. The only
remaining gap before a full round trip is genuine infrastructure, not
code: real ADR-0008 Kubernetes TokenReview enrollment for this service's
identity has not been performed in this test cluster.

## Development

```sh
cargo build
cargo test
```

## Podman

```sh
podman build -t localhost/infernal-worker-simple:latest .
podman run --rm --network infernal-law \
  --env KERNEL_AUTHORITY='infernal-law' \
  --env WORKER_SERVICE_ID='00000000-0000-4000-8000-000000000003' \
  localhost/infernal-worker-simple:latest
```

Join it to the same Podman network as a locally running `infernal-law` (see
that repo's own `README.md`) to reach it by container name. If that
kernel's own TLS-terminating layer uses a self-signed certificate, also
set `KERNEL_CA_CERT_PATH` to a mounted copy of it — otherwise the call
fails at the TLS layer rather than reaching the kernel at all.

## Kubernetes

The base manifests are in [`k8s/base`](k8s/base). Preview or apply them with
the Kustomize support built into `kubectl`:

```sh
kubectl kustomize k8s/base
kubectl apply -k k8s/base
```

There is no `Service`: this process only ever makes outbound calls, so it
has nothing to be reached on. `KERNEL_AUTHORITY` and `WORKER_SERVICE_ID` in
[`k8s/base/deployment.yaml`](k8s/base/deployment.yaml) default to
`infernal-law`'s own in-cluster `Service` name and a placeholder service
ID — adjust both for your deployment, and provision/enroll that service ID
with the kernel out of band (ADR-0008), and make it a route destination via
an active inclusive subscription, before expecting a claim to succeed. No
Kubernetes RBAC is needed here: this service never calls the Kubernetes API
itself.

## License

MIT. See [LICENSE](LICENSE).
