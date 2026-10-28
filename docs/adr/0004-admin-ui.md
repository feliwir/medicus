# ADR-0004: Admin UI, addon dependencies and runtime configuration

- Status: accepted
- Date: 2026-10-28
- Context: PLAN §1 (non-goals), §3.6 (admin API), §4 (addons), ADR-0001 (ABI), ADR-0003 (nodes, partitions, access)

## Context

The owner wants:

- an **admin UI**: setup on first launch, enabling and disabling addons, adding nodes, partitions
  and DICOMweb endpoints, and the other configuration;
- a **UI to browse and view** the stored DICOM data.

Until now:

- configuration has been read once, from TOML and environment variables. Nothing can change while
  the server runs;
- the admin API has been read-only and unauthenticated (open on loopback only);
- addons have been unable to declare what they need, and their info has been known only after
  `init` has run;
- a bundled web UI or viewer has been a v1 non-goal (PLAN §1).

## Decision

### 1. The admin UI is an addon, and the only one enabled at first

- `admin-ui` is a static (built-in) addon. It serves the single-page application through an
  `HttpRoutes` capability mounted into the core's HTTP server, claiming every path the core does
  not reserve.
- The core reserves `/api/v1`, `/health` and `/rs`.
- `admin-ui` is **required**: it is always enabled, and `enabled = false` is a configuration error.
- On a fresh install it is the only *functional* addon enabled. Everything else is enabled in the
  setup wizard or later in the UI.
- A server built without the `admin-ui` feature, for headless installs, serves the API only.

### 2. Addons declare dependencies

- Each addon exports a **manifest**, read before `init`. It holds:
  - `name` and `version`;
  - the capability kinds it **provides**;
  - the capability kinds it **requires**, optionally qualified by an addon name;
  - whether it is **required**;
  - an optional JSON Schema of its configuration.
- The server resolves the enabled addons:
  1. Every unmet requirement is satisfied by the provider named in the bootstrap configuration
     (`archive.index`, `archive.storage`), else by the built-in default (`index-sqlite`,
     `storage-fs`).
  2. That provider is pulled in as an implicit dependency.
  3. Addons are initialised in topological order.
- Cycles, missing providers and ambiguous providers are configuration errors that name the chain.
- This replaces the unconditional fallback that loads `storage-fs` and `index-sqlite`.
- Disabling an addon that an enabled addon depends on is refused.
- Index and blob stores are **infrastructure**: `admin-ui` requires an index, so a fresh install
  runs `admin-ui` and `index-sqlite`.

### 3. Settings live in the index; the configuration file only bootstraps

- **Bootstrap** (TOML and environment) holds only what is needed before settings can be read:
  - data directory, HTTP listen address and logging;
  - the index addon and its configuration;
  - addon search paths, library paths and checksums.
  Choosing the index backend is a deployment decision, not a UI setting: the settings are stored in
  it.
- **Settings** are one document, stored in the index as numbered **revisions** (author, time). It
  holds:
  - each addon's enabled flag and configuration;
  - nodes, partitions and peers;
  - roles, principals and access;
  - archive and job settings.
- Revisions give history and rollback.
- Writes use optimistic concurrency.
- Every change emits a `config.changed` event.
- `IndexStore` stores settings, users and sessions. Every index backend implements them, and the
  conformance suite checks them. Multi-node deployments on a shared index therefore share settings.
- On first start with an empty settings store, the configuration file's sections are imported as
  revision 1, so existing installs keep working.
- `medicus settings export/import` allows settings to be kept in version control.

### 4. Applying changes

- **Hot**: nodes, partitions, peers, roles, principals and access.
  - The new document is validated with the same code as at startup.
  - It is committed, the access directory and the peers are swapped, and the protocol listeners
    are restarted so they re-read the nodes.
- **On restart**: addon enable/disable, addon configuration, archive and job settings.
  - Addons cannot be unloaded (ADR-0001).
  - The UI says a restart is required. The admin API restarts the process gracefully by
    re-executing it.

### 5. Authentication: local accounts now, OIDC later

- **Users**: local accounts with argon2id password hashes.
- **Sessions**: server-side, carried in an HttpOnly, SameSite=Strict cookie. Mutating requests
  must also carry an `X-Requested-With` header (CSRF).
- **API tokens**: for scripts and external viewers, sent as bearer tokens.
- **Permissions**: users are principals (ADR-0003). Roles grant them data permissions per node and
  the admin permissions of ADR-0003: `admin.system`, `admin.users`, `admin.jobs`, `admin.peers`
  and `admin.events`.
- **First launch**: while no user exists, the server is in **setup mode**.
  - It prints a one-time setup token to the console and log.
  - It serves only the UI and the setup API, which requires the token.
  - The wizard creates the administrator, enables the chosen addons, creates the first node and
    restarts the server.
  - Headless installs use `medicus users create --admin` instead.
- **Protocol addons**: the host gains `authenticate(credentials) → principal`, so DICOMweb accepts
  the same sessions and tokens. OIDC (`HttpAuth`, M7) plugs in behind the same call.

### 6. The browser and viewer use DICOMweb

- The admin API still never moves data (PLAN §3.6). The data browser and the viewer read through
  QIDO-RS and WADO-RS.
- The core reverse-proxies `/rs` and `/nodes/{path}/rs` to the DICOMweb listener, so the UI is
  same-origin and needs no CORS. The proxy forwards the caller's credentials, and the core
  authorizes every call as for any other DICOMweb client.
- The viewer is a stack viewer on cornerstone3D. MPR and measurements come later.

### 7. Frontend technology

- **Build and core libraries**: React and TypeScript, built with Vite.
- **Styling**: Tailwind CSS v4, with shadcn/ui component sources kept in the repository.
- **Routing, data and tables**: TanStack Router, Query and Table.
- **API client**: generated from the admin API's OpenAPI document (`utoipa`).
- **Embedding**: the built bundle is embedded in the `admin-ui` addon. Without a bundle a placeholder
  page is served, so building the server never requires Node.

## Consequences

- PLAN §1's non-goal "bundled web UI or viewer" is dropped. The work becomes milestone M8, which
  brings local authentication and admin permissions forward from M7.
- ABI 0.7 adds the manifest, settings, users and sessions on `IndexStore`, `authenticate`, and
  `HttpRoutes` v1. Addons built for 0.6 must be rebuilt.
- The configuration file shrinks to a bootstrap. Configurations that still set nodes, roles or
  addons there are imported once, and later edits to those sections draw a warning.
- `HttpRoutes` v1 buffers request bodies, which is enough for the UI. DICOMweb keeps its own
  listener behind the proxy until streamed request bodies arrive.

## Alternatives considered

- **Settings in a separate core database**: this would have kept the index out of fresh installs.
  It was rejected because it means a second persistence layer, and multi-node deployments would
  need it shared as well.
- **Hot reload of everything**: not possible, because addons cannot be unloaded. A restart is
  honest about that.
- **Configuration file edited by the UI**: this keeps a hand-editable file, but conflicts with
  multi-node deployments and with an audit trail of who changed what.
