# ADR-0003: Nodes, partitions and access control

- Status: accepted
- Date: 2026-10-22
- Context: PLAN §3.11, ADR-0001 (host API), PLAN §3.4 (front-ends), §3.5 (jobs), §3.10 (lifecycle)

## Context

The owner wants:

- several **nodes** (AEs), each reachable through DIMSE, DICOMweb or both;
- **users/entities** that may **query**, **retrieve** and **send** per node (one flag each);
- **RBAC**: a user sees zero or more nodes;
- **partitioning** of the data, for multi-tenancy and data separation.

Until now medicus has had one archive (one index namespace, one default store), one DIMSE AE title,
remote peers as a separate config list, and no authentication or authorization. Host services
(`ingest`, `query`, `locate`, `open_instance`, `submit_job`) are global and don't know who is
calling.

## Decision

### 1. Three concepts: partition, node, principal

| Concept | What it is | Examples |
|---|---|---|
| **Partition** | The unit of data separation. Every patient, study, series, instance, blob, event and job belongs to exactly one partition. | `tenant-a`, `research`, `default` |
| **Node** | A named endpoint through which data is reached, exposed as a DIMSE AE title, a DICOMweb path, or both. Backed by **one local partition** or by **one remote peer** (a proxy). | `RADIOLOGY` → partition `tenant-a`; `EXTERNAL_PACS` → peer `pacs` |
| **Principal** | An authenticated user or entity. Gets permissions on nodes through roles. | user `alice` (OIDC), DIMSE entity `CT01` (calling AE + IP) |

- **Local node → partition is N:1.** A tenant can have several nodes, e.g. an ingest AE for
  modalities and a read-only AE for viewers. A node never spans partitions: reading across tenants
  would undo the separation.
- **Remote nodes** forward query, retrieve and store to their peer. They have no partition. Caching
  remote data into a partition comes later, as an explicit feature.
- The existing `[peers]` stay what they are: transport descriptions of remote AEs (address, AE
  title, protocol). A remote node refers to a peer; C-MOVE destinations and send targets are peers
  or local nodes.

### 2. Partitioning is real separation

- **Index**: every hierarchy row carries `partition_id`. UIDs are unique **per partition**, so the
  same Study Instance UID can exist independently in two partitions. Patients are keyed by
  (partition, Patient ID, issuer). Every `IndexStore` method takes the partition; there is no
  query across partitions. The conformance suite gains isolation tests (same UIDs in two
  partitions, no leaks through query, locate, counts, delete or events).
- **Storage**: blob keys are prefixed with the partition, and each partition chooses its blob store,
  so tenants can get separate buckets or volumes, and later separate encryption keys.
- **Copies, not references**: a transfer between partitions (C-MOVE from node A to local node B,
  a send job) ingests a copy into the target partition. Blobs are never shared across partitions,
  so deleting or evicting in one partition cannot affect another.
- **Per-partition settings**: duplicate policy, extra tags, quota and eviction policy (§3.10),
  default blob store. The capacity guard applies per partition (its quota) and globally (the
  disk).
- **Events and jobs** carry `partition` and `node`. Event hooks can subscribe per partition;
  `/events` and `/jobs` are filtered by what the caller may see.
- **Zero-config stays zero-config**: one partition `default` and one node `MEDICUS` (DIMSE +
  DICOMweb) are created implicitly.

### 3. Permissions

Permissions are granted **per node**:

| Permission | Allows | DIMSE | DICOMweb |
|---|---|---|---|
| `query` | searching | C-FIND | QIDO-RS |
| `retrieve` | reading instances | C-GET, C-MOVE (as source) | WADO-RS |
| `store` | putting instances into the node | C-STORE | STOW-RS |
| `send` | starting transfers from the node to elsewhere | C-MOVE (to the destination) | SEND-RS (transaction) |

So the owner's "send" is two flags: storing *into* a node (`store`) and initiating transfers *out of*
it (`send`). A C-MOVE needs `retrieve` + `send` on the source node. If the destination is a local
node, the core performs the sub-operations as that node's ingest, which needs `store` there; a
remote destination must be a peer the principal may send to. Deletion is not a user permission in
v1: it happens through eviction and retention (§3.10).

Admin permissions are separate and also node- or partition-scoped: `admin.events`, `admin.jobs`
(read/cancel), `admin.peers` (list/echo) and `admin.system`. (ADR-0004 adds `admin.users` and,
for now, grants admin permissions only on all nodes, `*`.)

### 4. Principals and roles (v1: configuration)

```toml
[partitions.tenant-a]
storage = "s3-tenant-a"
quota_bytes = 10_000_000_000_000

[nodes.RADIOLOGY]            # local node
partition = "tenant-a"
dimse = { ae_title = "RADIOLOGY" }
dicomweb = { path = "radiology" }      # /nodes/radiology/rs/...

[nodes.EXT]                  # remote node (proxy)
peer = "pacs"
dicomweb = { path = "ext" }

[roles.modality]
grants = [{ nodes = ["RADIOLOGY"], permissions = ["store"] }]

[roles.viewer]
grants = [{ nodes = ["RADIOLOGY", "EXT"], permissions = ["query", "retrieve"] }]

[principals.ct01]            # a DIMSE entity
dimse = { calling_ae = "CT01", networks = ["10.0.0.0/24"] }
roles = ["modality"]

[principals.alice]           # a user, authenticated by the HttpAuth addon
roles = ["viewer"]

[access]
anonymous_roles = []         # unauthenticated callers get these (empty: nothing)
```

Later: roles from identity-provider claims (OIDC groups → roles) through `HttpAuth`; DIMSE
User Identity negotiation (JWT, username/password) and TLS client certificates as further
principal sources; principals and roles stored in the index and managed through the admin API.

### 5. Enforcement: authenticate in the addon, authorize in the core

- **Protocol addons authenticate**: they turn wire credentials into a principal claim. DIMSE: called
  AE → node, calling AE + peer address (+ TLS certificate, User Identity) → principal. DICOMweb: path
  → node, `HttpAuth` → principal.
- **Every host call carries a call context** `{node, principal claim}` (ABI 0.6). The core resolves
  the claim against the configured principals, derives the partition from the node and checks the
  permission *before* doing anything. An addon can neither name a partition nor bypass the check.
  This keeps DIMSE and DICOMweb identical (as with events), and it matters for sandboxed WASM
  addons.
- Results are scoped by construction: the index is called with the node's partition only.
- **Denials**: DIMSE associations from principals without any permission on the called node are
  rejected (A-ASSOCIATE-RJ, called/calling AE not recognized). Operations without the permission
  are answered with 0124H (Refused: Not authorized). DICOMweb answers 401 or 403. Every denial is
  logged and published as an `access.denied` event.
- **DIMSE negotiation**: dicom-ul negotiates presentation contexts from static options before the
  addon sees the association, so contexts are not tailored per principal; permissions are checked
  per operation (0124H). Acceptable, because the result is the same and nothing leaks.

### 6. Remote nodes

*Decided 2026-10-22 (owner):* retrieval through a remote node **fetches on demand** (C-GET /
WADO-RS against the peer) and streams to the caller. **Caching is optional**: a remote node may
name a cache partition and a cache size; fetched instances are then also kept there (evicted LRU
once the size is exceeded) and served locally next time. Without a cache nothing is stored
locally. Remote nodes are scheduled after DICOMweb (M4c follows M5).


`RemoteNode` grows `find`, `move` and `get`, alongside `echo` and `send`. A query to a remote node
is a C-FIND (or QIDO-RS) against its peer, with matches streamed back through the requesting
protocol; a retrieve fetches through the peer and streams the instances to the caller without
storing them. Authorization happens in medicus, before forwarding; the peer only sees medicus's
own AE title or credentials.

## Consequences

- **ABI 0.6**: the call context on `ingest`, `call` and `open_instance`; partition-aware
  `IndexStore` requests; `RemoteNode` find/move/get.
- **Index schema**: `partitions` table and `partition_id` on patients/studies (series and instances
  inherit through their foreign keys); uniqueness per partition; partition on events and jobs.
  Nothing is released yet, so migrations 0001/0002 are rewritten instead of migrated.
- The DIMSE addon serves several AE titles on one listener, and C-MOVE sub-operations use the
  source node's AE title. *Later: a node may name its own DIMSE port; the addon then listens on
  every port its nodes name, and accepts each AE title only on its port. AE titles stay unique.*
- *Later: the DICOMweb service roots are configurable (`node_root`, `default_root`).*
- The DICOMweb addon (M5) is node-scoped from the start (`/nodes/{node}/rs/...`, plus an optional
  default node at `/rs/...`).
- Until `HttpAuth` exists, the admin API is open only on loopback (or behind a static token) and
  treats its caller as an administrator.
- **Cost**: partition copies multiply storage when data is shared on purpose between tenants. That
  is the price of real separation and is documented.

## Alternatives considered

- **Shared data with per-node views (filters)**: cheaper, no index changes, but no real separation:
  one UID namespace, shared blobs, filter bugs leak across tenants. Rejected (owner: separation
  required).
- **One index database per partition**: strongest isolation, but N connection pools, N migrations
  and no cheap global view for the core (capacity, jobs). Possible later as a deployment option
  (partition → index instance), since the `IndexStore` API is partition-aware anyway.
- **Authorization in each protocol addon**: duplicated logic, risk of DIMSE/DICOMweb drift.
  Rejected.
