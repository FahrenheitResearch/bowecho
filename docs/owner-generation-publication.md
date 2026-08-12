# Owner generation publication

BowEcho can explicitly publish one complete, processed private WRF or ArWen
`rw-store` generation to a trusted Rusty Weather origin. This is conventional
authenticated HTTPS. It is not Community Cache, peer assistance, TURN, ICE,
STUN, or direct P2P.

The feature is off by default. Configure it under **Settings > Owner generation
publication** with the trusted Hetzner origin, the opaque owner-principal
SHA-256 issued by that origin, bounded spool/retention policy, and complete
attribution. The bearer is stored separately in the operating-system credential
vault and is bound to the exact origin ID and normalized HTTPS URL. It is never
written to `config.json` or a publication job.

## Workflow

1. Select a local processed run in the Models library and open **Advanced >
   Publish processed WRF / ArWen generation**.
2. **Prepare immutable copy** works offline. It locks the source, requires deep
   `rw-store` validation, inventories only `run.json`, `grid.rwg`, and every
   manifest-listed `.rws`, and freezes bounded SHA-256 objects. Raw `wrfout`,
   arbitrary files/directories, and symlinks/reparse points are rejected or
   have no protocol representation.
3. Review the exact source/published `model/run`, provenance, attribution,
   retention, file/chunk totals, and object hash. Confirm ownership or
   authorization, redistribution rights, and that the HTTPS operator observes
   connection IP and necessary request metadata.
4. **Publish over HTTPS** is the first upload action. BowEcho first obtains the
   owner-scoped capabilities and enforces server limits/quota, then begins or
   resumes the durable upload, sends only missing declared chunks, and
   idempotently finalizes it.
5. Use **Reconcile**, **List my origin records**, **Cancel active origin
   upload**, and **Revoke publication** as explicit owner actions. A local-only
   Prepared/Confirmed job can instead be discarded without a network or vault
   credential. A finalize-uncertain job remains protected and must reconcile
   before it can be cancelled or resumed.

BowEcho never automatically begins, resumes, reconciles, cancels, or revokes a
network publication when the app starts. Loading the panel only reads redacted
local job state. Every network operation is attached to a labelled button.

## Identity and conflicts

The publication identity remains byte-exact with `run.json` and every `.rws`
hour. BowEcho does not rewrite the copied manifest or scientific bytes during
publication. `rw-store` deep validation requires the manifest and hour-file
model/run identities to match; changing only `run.json` would create an invalid
generation.

The origin enforces the live `(model, run)` namespace. If another owner already
occupies the exact identity, BowEcho reports the conflict and the run must be
re-imported under a unique run identity before publication. An import-time
opaque namespace may improve this workflow later, but the publication step must
never fabricate one or silently re-encode immutable scientific data.

## Provenance and redistribution

New WRF writer seams stamp normalized `private-wrf` provenance; a run carrying
the ArWen producer sidecar is stamped `private-arwen`. Producer classification
does not infer ownership or redistribution rights. Those remain explicit human
confirmations for the exact prepared job. Legacy hours with empty provenance
fail closed unless the owner deliberately runs the exact, locked legacy
migration seam.

The publication grant can only be `PrivateWrf`, `PrivateArwen`, or
`UserProvided`; it cannot be `PublicProvider`. Attribution is mandatory. When
ECMWF lineage appears, BowEcho installs the canonical ECMWF CC BY 4.0 notice and
a locked modification notice.

## Local durability and recovery

Jobs use a cryptographically random, persisted upload ID while their generation
content hash stays canonical. Repeated Prepare for the same origin, owner, and
nonterminal content reuses the durable job; different owners receive unlinkable
IDs. State contains no raw source path, bearer, account name, or arbitrary file
record.

Before sending the first begin request, BowEcho atomically persists
`OriginBeginUncertain`. This ensures a crash or lost response cannot make a
possible server reservation appear to be an offline-only job. Before finalize,
and after any ambiguous finalize response, active chunks remain protected until
exact owner-record reconciliation completes.

Spool admission includes existing object/state/staging usage plus the worst-case
temporary validation copy. Cleanup is manual and bounded. It never removes CAS
chunks referenced by Prepared, Confirmed, origin-begin-uncertain, Uploading,
FinalizeUncertain, or retryable jobs.

## Privacy boundary

The trusted origin operator necessarily sees the publisher's IP address and
request/transfer metadata needed to authenticate, operate, secure, and quota the
service. Other BowEcho users are never involved in this delivery path and learn
nothing from it. Private WRF/ArWen generations are not automatically advertised
or seeded through Community Cache. Publishing a generation to an HTTPS origin
does not opt it into relay-mediated community sharing.
