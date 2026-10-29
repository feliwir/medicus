# ADR-0005: Separate state store and archive index

- Status: accepted
- Date: 2026-10-29
- Context: ADR-0004 §3 (settings live in the index), PLAN §3.7 (outbox), §3.9 (scale), M6
  (`index-postgres`)
- Amends: ADR-0004 §3 and its rejected alternative "settings in a separate core database"

## Context

ADR-0004 stores settings, users and sessions in the index and makes the index backend a
bootstrap decision: the settings cannot choose the store they are read from. With `index-postgres`
(M6) this has consequences:

- The setup wizard cannot offer SQLite or PostgreSQL. Using PostgreSQL means editing the bootstrap
  TOML before the first start, which an installer who never touches a configuration file won't do.
- A wrong PostgreSQL URL, or an unreachable database, stops the server before the admin UI is up.
  Nothing in the UI can fix it.
- The index addon's configuration, which includes the database password, is part of the bootstrap
  and cannot be changed through the admin API.

ADR-0004 rejected a separate settings database because it would mean a second persistence layer
and multi-node deployments would have to share it as well. The first reason no longer holds:
`index-sql-common` already implements settings, users and sessions for every SQL backend, so a
second store is a second *instance* of the same code, not a second layer. The second reason still
holds and is addressed in §6.

## Decision

### 1. Two roles

The data the server persists is split into two roles. Both are `IndexStore` instances.

| Role | Holds | Chosen by |
|---|---|---|
| **State store** | settings revisions, users, credentials (sessions and API tokens) | the bootstrap |
| **Archive index** | patients, studies, series, instances, extra tags, the event outbox, cursors, jobs | the settings |

- The outbox stays with the archive: an event must commit in the same transaction as the instance
  it describes (PLAN §3.7). Cursors and jobs go with the events they consume and the instances
  they move.
- `config.changed` goes to the archive's outbox as before. Publishing it is already a separate,
  best-effort write after the settings write, so no transaction is lost by the split.
- When both roles name the same index store, they are the **same instance**. This is the default.

### 2. Bootstrap: the state store

```toml
[state]
index = "sqlite"   # the registered name of the index store that holds the state; default "sqlite"

[addons.index-sqlite]
config = { path = "data/index.sqlite" }   # default when not configured
```

- The bootstrap names the state store and configures its addon, exactly as it configures the index
  today. Without any configuration it is `index-sqlite` at `data_dir/index.sqlite`, so a fresh
  install still starts with zero configuration.
- Compatibility: a bootstrap without `[state]` but with `archive.index` uses that index as the state
  store. Existing configurations therefore keep both roles in one store, as today.
- With `[state]`, only the addon that registers the state store is part of the bootstrap. The loader
  finds it without loading anything: the store's name is the `name` in the addon's configuration,
  or else the default `name` in its configuration schema (both index addons have one). Without a
  match, the default provider (`index-sqlite`) is used if the bootstrap does not configure it. Any
  other index addon in the bootstrap, and a different `archive.index`, are settings and are
  imported on the first start.

### 3. Settings: the archive index

- `archive.index` becomes a setting: the registered name of the index store for the archive. When
  the settings don't set it, the archive uses the state store (the default; existing installs
  change nothing).
- An archive index other than the state store is an ordinary addon in the settings
  (`addons.index-postgres` with `enabled` and `config`), resolved and loaded with the other addons.
- Startup order:
  1. read the bootstrap;
  2. load the state store's addon and open it (today's `load_index`, restricted to that addon);
  3. read the current settings revision;
  4. resolve and load the remaining addons, including the archive index's;
  5. open the archive.
- If step 4 or 5 fails for the archive index (unreachable database, failed migration, wrong
  password), the server still starts, in **degraded mode**:
  - the admin UI and the admin API work;
  - `GET /api/v1/system` reports the error;
  - the protocol addons (DIMSE, DICOMweb) are not started.

  Because the state store is still open, an administrator can log in, correct the setting and
  restart. A bad database setting can no longer lock them out.
- Changing `archive.index` takes effect after a restart (`restart_required`, as for other addon
  changes). The admin API refuses an `archive.index` that no enabled addon registers.
- Only the archive index's own failures degrade the server: its addon failing to initialize, or the
  index failing the archive's first calls. Configuration errors (an unknown blob store, an invalid
  extra tag) still stop it, as do failures when the archive index is the state store. While
  degraded, the archive's index answers every call with `Unavailable` (HTTP 503), and the event
  deliveries, the job engine and the capacity monitor do not run.
- An addon that fails to initialize reports why: the host keeps the last error the addon logged
  during its initialization and adds it to the load error (no ABI change). That is what
  `GET /system` shows in `degraded.error`.
- One addon is one instance, so the state store and the archive index can use the same addon only
  when they are the same store. Two different PostgreSQL databases for the two roles would need
  two addon instances, which the loader does not support; nobody has asked for it.

### 4. The setup wizard gets a database step

After the administrator is created (in the state store), the wizard asks where the archive lives:

- **SQLite** (default): nothing to configure; the archive uses the state store.
- **PostgreSQL**: host, port, database, user, password and TLS mode. The wizard saves them as the
  `index-postgres` addon's configuration and sets `archive.index`.

The step is completed by the restart at the end of the wizard. There is no live "test connection":
addons cannot be unloaded, so the core cannot load `index-postgres` to try a URL and then drop it.
Instead, a failed connection after the restart is reported in degraded mode (§3), and the wizard
shows it with a link back to the database step.

The step needs `index-postgres` in the binary: `builtin-index-postgres` becomes a default feature.
The driver is pure Rust (no `libpq`), so this costs binary size, not system dependencies.

### 5. Changing the archive index later

The DICOM files in the blob stores are referenced only by the archive index. Pointing a running
archive at another, empty index orphans them. Therefore:

- the admin API refuses to change `archive.index` while the current archive index holds instances
  (`409 archive_not_empty`). Pointing it back at the index the archive runs on is always allowed,
  and so is any change while the server is degraded: whether the unavailable index holds instances
  is unknown then, and refusing would keep the administrator from correcting a wrong choice;
- moving an existing archive is the job of a separate migration tool (SQLite → PostgreSQL, PLAN
  §7), not of a settings change.

### 6. Multi-node deployments

Nodes that share an archive must share their settings too, or they would disagree about nodes,
partitions and access. They point the bootstrap's state store at the shared PostgreSQL database.
The archive index then defaults to the same store, so one database holds both roles, as under
ADR-0004. The split helps single-node installs; multi-node installs configure the bootstrap, as
before.

### 7. Secrets in settings

With the database password in the settings, secrets in settings need handling. This already
applies today: `events-webhook` keeps its `secret` and `headers` there.

- Addon configuration schemas mark secret fields with `"writeOnly": true` (JSON Schema). With
  `schemars` that is `#[schemars(extend("writeOnly" = true))]`; the SDK needs nothing of its own.
  A marked object (e.g. `events-webhook`'s `headers`) is a secret as a whole.
- The admin API never returns a secret. `GET` returns `{"$secret": true}` in its place, and so do
  the settings revisions, the addon list and `medicus settings export` (unless `--with-secrets`,
  for backups). Unset secrets (`null` or empty) are shown as they are.
- A `PUT` that sends `{"$secret": true}` back keeps the stored value; any other value replaces it.
  Values are matched by their path in the configuration (object keys and array positions); a
  placeholder without a stored value at its path is refused. `medicus settings import` restores
  placeholders the same way. The UI shows a stored secret as "set" with replace and remove actions.
- `index-postgres` gets a separate `password` field (secret). A password inside `url` is still
  accepted, but the admin API refuses such a URL; only the bootstrap may contain one. The rule is
  generic: the admin API refuses a changed addon configuration with a `scheme://user:password@`
  URL outside its secret fields.
- Secrets are stored in plain text in the state store. It lives in `data_dir`, which already holds
  the archive and must be protected anyway. Encryption at rest is out of scope.

### 8. Implementation

- No ABI change: the core holds two `Arc<dyn IndexStore>` handles, `state` and `archive.index`.
  Settings, authentication and user management use `state`; ingest, query, retrieve, events and
  jobs use `archive.index`. The `medicus settings` and `medicus users` commands open only the
  state store.
- The `IndexStore` conformance suite is unchanged. Every backend still implements both roles, so
  any backend can serve either.
- Splitting the trait into `IndexStore` and `StateStore` (ABI 0.8) would make the roles explicit
  in the type system. It is deferred until a backend wants to implement only one of them.

## Consequences

- PostgreSQL can be chosen in the setup wizard and changed in the admin UI while the archive is
  empty. The bootstrap stays small and is rarely needed on single-node installs.
- An unreachable archive database is no longer fatal; the server explains it in the UI.
- Existing installs behave exactly as before: without `[state]` and without `archive.index` in the
  settings, both roles are the one index they have today.
- `builtin-index-postgres` joins the default features.
- Admin API: `409 archive_not_empty`, the `$secret` placeholder, and degraded mode in
  `GET /system`.
- Addon authors mark secret configuration fields `writeOnly`; `events-webhook` is updated.
- ADR-0004 §3 points to this ADR.

## Alternatives considered

- **Keep ADR-0004 (bootstrap only) and document it**: no code, but PostgreSQL stays a
  configuration-file decision, and a database outage at startup still locks out the UI.
- **Wizard writes a bootstrap file into `data_dir`**: the wizard could choose the database without
  the split. Rejected: the server would rewrite its own configuration (the "configuration file
  edited by the UI" ADR-0004 rejected), the password would sit in a file outside the audit trail,
  and a wrong choice would still stop the server at the next start, with no UI to fix it.
- **Settings in a file, archive in the index**: the same split with a file as the state store.
  Rejected for the reasons ADR-0004 gave: no revisions or audit trail, and nothing to share
  between nodes.
- **Split the trait now (ABI 0.8)**: cleaner types, but an ABI bump and a rebuild of every addon,
  and no backend benefits from it yet.
