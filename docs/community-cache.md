# Community Cache

Community Cache is BowEcho's opt-in, privacy-preserving cache for immutable
Rusty Weather query results. It is **off by default**.

Hetzner is the conventional authoritative HTTPS query origin and signer; it
never serves through TURN. BowEcho looks for an origin-signed operational
object in this order:

1. the verified local BowEcho cache;
2. the configured R2 hot-object endpoint;
3. the configured Rusty Weather / Hetzner HTTPS origin.

A miss or unavailable cache tier is ordinary: BowEcho falls through to the
next tier. If public-origin federation is explicitly configured, an honest
normal Hetzner resolve miss may be followed by a request to Hetzner's
federation proxy. Hetzner alone selects and contacts an independently pinned
university/lab/public HTTPS origin, verifies the exact returned object,
re-signs it, and stages it on Hetzner's normal object endpoint. BowEcho never
contacts an institution or sends the authority bearer there. This is still the
conventional authority path, not a fourth client-side delivery tier.

Operational requests never call the relay. The separately opted-in
cold-historical path is local cache, R2, a relay-mediated community copy of an
exact signed object, then archival HTTPS origin or an honest unavailable
result. Relay-mediated peer-assisted transfers run only in that historical
ordering, behind independent client and server gates.

Direct peer connectivity is permanently excluded. The transport uses only an
audited TURN/UDP provider allocation, with end-to-end XChaCha20-Poly1305
encryption, authenticated chunk acknowledgements, bounded retransmission, and
an exact final hash. There is no STUN, ICE gathering, host or server-reflexive
candidate, LAN discovery, direct socket fallback, or peer-visible address.

## Trust boundary

Every accepted object has an origin-signed canonical manifest. The signature
binds its SHA-256 content address to the exact model, run snapshot, valid time,
grid, variables, query parameters, recipe, source provenance, schema version,
encoded/decoded sizes, compression, expiry, attribution, and modification
notices. BowEcho verifies the manifest, request identity, signature, object
hash, expiry, size, and bounded streaming decompression before parsing or
caching the payload.

Unknown versions, malformed data, expired signatures, unknown signing-key
IDs, hash mismatches,
oversized objects, and decompression bombs fail closed. Cache entries cannot
mix model runs, grids, variable sets, pressure levels, or recipes.

Origin signing-key rotation uses an explicit keyring of at most eight unique
`key_id:base64` pins. The legacy single-key setting remains `rw-origin-v1`
during migration. Old and new pins may overlap; removing a pin revokes objects
signed by it. BowEcho never auto-trusts a key advertised by a server. Expired
objects are removed from the local index and cannot be advertised or seeded.

## What is eligible

The settings allowlist contains only:

- soundings and pressure profiles;
- point time series;
- native windows and tiles;
- temporal and diurnal products;
- explicitly published case-room artifacts.

There is no arbitrary-file or private-directory route. Full-run replication is
a separate advanced feature, not Community Cache seeding. Private/local WRF
and ArWen runs are denied by default; publication requires a deliberate owner
action and confirmed redistribution rights. Passive searches do not publish a
case room.

The initial reliable datagram lane is intentionally narrow: only profiles and
point series up to 64 KiB may use it. Stop-and-wait reliability is safe but not
appropriate for large products. Native and geographic windows, temporal and
diurnal grids, and case artifacts therefore skip relay transfer and use
archival HTTPS fallback until a bounded authenticated sliding-window transport
passes the release gates.

ECMWF-derived objects must retain the required source, license, terms,
disclaimer, and affirmative modification notice through every cache tier and
case-room manifest.

## Configure BowEcho

Open **Settings > Community Cache** and configure:

- the public HTTPS Rusty Weather / Hetzner origin;
- an optional public HTTPS R2 hot-object base URL;
- one to eight explicit Ed25519 origin verification key pins (the legacy key
  maps to `rw-origin-v1`);
- the desired category allowlist and bounded disk/download/monthly/concurrency
  limits.

Cold-historical recovery additionally requires its own opt-in, one to eight
explicit relay-credential key pins (the legacy key maps to `rw-relay-v1`), and
operator-audited provider allocation ranges. Seeding has a second independent
opt-in. When metered-network pausing is enabled, the application treats an
unknown network as metered and requires a session-only unmetered confirmation
before uploading. Settings and the Data workspace show only bounded counters
and closed failure codes; they never expose participant or allocation
addresses.

If the origin requires a bearer token, save it with the in-app vault controls.
It is stored in the operating-system credential vault, never in BowEcho's JSON
settings or logs. Enabling remains unavailable until the required HTTPS URL and
public key validate.

Optional public-origin failover is configured separately under **Settings >
Public origin federation**. It requires bounded authority catalog key pins,
an explicit allowlist of origin IDs with independently pinned descriptor keys,
and explicit origin/key revocations. Those values are non-secret. Signed
catalog and health refreshes run off the UI thread, and the Model workspace can
optionally prefer one currently verified origin; `Automatic` lets the
authority choose. The preferred ID is only a hint among server-admitted
candidates and never causes a direct BowEcho request.

Operational HTTPS never uploads or seeds. Metered status does not disable its
bounded downloads; download and monthly quotas remain applicable. Cold relay
retrieval and seeding are separately off by default. Seeding accepts only an
already origin-verified eligible CAS object, pauses on metered networks by
default, and obeys local upload/download/storage/concurrency/monthly limits plus
server quotas, cost stops, and the global kill switch.

The operator must explicitly confirm current provider pricing before enabling
the server gate. Cloudflare's current published Realtime terms describe a
shared 1,000 GB/month SFU+TURN allowance, then $0.05/GB of edge-to-client
egress including TURN overhead. Those terms are mutable, account-specific in
effect, and not a permanent guarantee; production caps must be chosen from the
actual account terms.

## Privacy

Other BowEcho users never learn one another's IP addresses. The relay operator
necessarily sees connection metadata such as the connecting IP, time, and
transfer size; other users do not, and end-to-end encrypted payloads are not
readable by the relay. This is a privacy boundary, not an anonymity claim about
the service operator.

The canonical protocol and threat model live with Rusty Weather in
`docs/COMMUNITY_CACHE_PROTOCOL.md` and
`docs/COMMUNITY_CACHE_THREAT_MODEL.md`.
