# Guarded repository provisioning

Design for fastverk/geetch#72. This proposal is not an implemented capability.

Interactive archive, deletion, and protection confirmation must reject changes
made since the user reviewed the repository. Existing provisioning requests
have no precondition, and a repository slug is reused after deletion. A read
followed by an unconditional write cannot enforce this requirement.

## Protocol

Add distinct guarded RPCs to the provisioning contract. Do not add optional
precondition fields to existing mutation RPCs: an older protobuf server would
ignore unknown fields and execute an unconditional mutation. Older servers must
answer a guarded method with UNIMPLEMENTED. Clients must not fall back to an
unguarded method after that response.

Define the wire records as canonical protobuf messages:

- `ProvisionRevision`: opaque repository incarnation plus an opaque
  configuration revision. Both are nonempty and issued by the owning service.
- `GetProvisionSnapshotRequest`: repository reference and optional branch.
- `GetProvisionSnapshotResponse`: existence, repository description, revision,
  and protection state for the requested branch, captured consistently.
- `GuardedArchiveRepoRequest`: repository, expected revision, idempotency key.
- `GuardedDeleteRepoRequest`: repository, expected revision, exact confirmation
  name, idempotency key.
- `GuardedEnsureProtectionRequest`: repository, branch, desired protection,
  expected revision, idempotency key.
- `ProvisionMutationReceipt`: immutable operation identity, repository
  incarnation, expected revision, operation outcome, and resulting revision
  where the repository survives.
- `GetProvisionMutationRequest/Response`: retrieve a receipt after an uncertain
  response without repeating an irreversible action.

This is an additive protocol. Existing automation methods keep their documented
behavior; interactive clients requiring guarded writes use only these methods.
In-process adapters expose guarded operations with a default unsupported result
until the backing forge supplies equivalent atomic guarantees. Read-and-compare
emulation is not sufficient.

## Owning-service guarantees

Repository incarnation changes on deletion/recreation, including recreation with
identical settings. Configuration revision changes whenever the observed
configuration changes; changing a value and changing it back cannot revive an
old confirmation. It covers lifecycle, repository settings, and protection
configuration. Git commit identity remains a separate revision domain.

The service authenticates and authorizes the current caller before reading or
returning a receipt. Authorization is never inherited from the original request.
It validates both expected values and performs the operation under the same
repository ownership/transaction boundary. All configuration writers, including
legacy operations, advance the revision. Missing or malformed preconditions are
INVALID_ARGUMENT; a mismatch is FAILED_PRECONDITION with no mutation.

The idempotency key binds the authenticated caller, repository incarnation,
operation kind, expected revision, and exact payload. An identical authorized
retry returns the original receipt; reuse for different input is ALREADY_EXISTS.
Deletion receipts must survive repository removal without becoming receipts for
a replacement repository. Retention and expired-key behavior must be explicit
before implementation is released; an expired key must not silently authorize a
new destructive operation against a replacement.

Geetch must coordinate its filesystem and metadata through durable operation
records and recovery. A transaction cannot atomically delete Git files and a
Postgres row. The receipt cannot claim completion until the owning service has
verified the intended postcondition. Interrupted cleanup must remain queryable
and recoverable rather than becoming an unexplained success or duplicate delete.

## Shared conformance cases required before adoption

1. Current snapshots can drive guarded archive and protection changes.
2. Another configuration writer invalidates the old revision without side
   effects from the stale request.
3. Delete/recreate with identical settings invalidates the old incarnation.
4. Configuration changes followed by reversion do not revive old revisions.
5. Concurrent confirmations against one revision cannot both apply conflicting
   mutations; the losing operation leaves no partial state.
6. Exact-name confirmation is still required in addition to a valid revision.
7. Same-key/same-payload retries return the original confirmed outcome; changed
   payloads and operations cannot reuse the key.
8. Dropped responses can be recovered through receipt lookup, including deletion.
9. A revoked or different caller cannot retrieve or retry another caller's
   operation merely by possessing its key.
10. Unsupported adapters and old servers reject guarded calls without invoking
    any legacy mutation.
11. Restart recovery completes or explicitly reports an interrupted operation
    without applying it to a replacement repository.

## Implementation sequence

Implement canonical protobuf records, adapter capability behavior, and shared
conformance cases first. Add a deterministic fake implementation to exercise
the suite, then implement Geetch's durable revisions and operation recovery.
Publish the contract and update Geetch's pinned dependency. Finally, wire the
console to snapshots and guarded methods, preserving drafts on conflicts and
requiring refreshed confirmation. Run the serving identity, concurrency, and
recovery journeys before closing the associated acceptance requirements.
