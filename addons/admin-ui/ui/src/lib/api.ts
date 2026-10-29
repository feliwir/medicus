// The admin API client (`/api/v1`, same origin). Sessions are cookies the browser sends by
// itself; requests that change something carry `X-Requested-With`, which the server requires for
// cookie-authenticated changes (CSRF).

export class ApiError extends Error {
  constructor(
    readonly status: number,
    readonly code: string,
    message: string,
  ) {
    super(message);
  }
}

async function call<T>(method: string, path: string, body?: unknown, headers: Record<string, string> = {}): Promise<T> {
  const init: RequestInit = { method, credentials: "same-origin", headers: { ...headers } };
  if (method !== "GET") {
    (init.headers as Record<string, string>)["X-Requested-With"] = "medicus-ui";
  }
  if (body !== undefined) {
    (init.headers as Record<string, string>)["Content-Type"] = "application/json";
    init.body = JSON.stringify(body);
  }
  const response = await fetch(`/api/v1${path}`, init);
  const text = await response.text();
  const data: unknown = text ? JSON.parse(text) : undefined;
  if (!response.ok) {
    const e = (data ?? {}) as { error?: string; message?: string };
    throw new ApiError(response.status, e.error ?? "error", e.message ?? response.statusText);
  }
  return data as T;
}

export interface User {
  name: string;
  display_name?: string;
  roles: string[];
  disabled: boolean;
  password?: boolean;
}

export interface Me {
  user: User;
  /** Admin permissions, e.g. `admin.system`. */
  permissions: string[];
}

export interface AddonStatus {
  name: string;
  version: string;
  provides: string[];
  loaded_by: { by: "config" | "required" | "archive" | "dependency"; of?: string; capability?: string };
}

export interface SystemInfo {
  name: string;
  version: string;
  abi_version: string;
  storage: string;
  settings_revision: number;
  restart_required: string[];
  /** Set when the archive index is unavailable: only the admin UI and API work (ADR-0005). */
  degraded?: { index: string; error: string } | null;
  addons: AddonStatus[];
}

export interface NodeInfo {
  name: string;
  ae_title?: string;
  dicomweb_path?: string;
  remote: boolean;
}

export interface Job {
  id: string;
  kind: string;
  peer?: string;
  node: string;
  state: "queued" | "running" | "succeeded" | "failed" | "cancelled";
  attempts: number;
  progress: { total: number; completed: number; failed: number; warning: number };
  error?: string;
  created_at: string;
  updated_at: string;
}

export interface MedicusEvent {
  seq: number;
  kind: string;
  time: string;
  node?: string;
  partition?: string;
  origin: { type: string; [key: string]: unknown };
  resource?: { study?: string; series?: string; instance?: string; patient_id?: string };
}

/** The settings document; its sections are edited as plain JSON. */
export type Settings = Record<string, unknown> & {
  server: { name: string };
  addons: Record<string, { enabled?: boolean; config?: unknown }>;
  nodes: Record<string, unknown>;
};

export interface SettingsState {
  revision: number;
  settings: Settings;
  restart_required: string[];
}

export interface Applied {
  revision: number;
  restart_required: string[];
}

export interface RevisionSummary {
  revision: number;
  author?: string;
  comment?: string;
  created_at: string;
}

export interface AddonInfo {
  name: string;
  version: string;
  provides: string[];
  requires: { capability: string; addon?: string }[];
  required: boolean;
  config_schema?: JsonSchema;
  loaded_by?: AddonStatus["loaded_by"];
  enabled?: boolean;
  config: unknown;
  bootstrap: boolean;
  selected: boolean;
  required_by: string[];
}

/** The subset of JSON Schema the configuration forms understand. */
export interface JsonSchema {
  type?: string | string[];
  description?: string;
  default?: unknown;
  properties?: Record<string, JsonSchema>;
  required?: string[];
  items?: JsonSchema;
  anyOf?: JsonSchema[];
  oneOf?: JsonSchema[];
  enum?: unknown[];
  format?: string;
  minimum?: number;
  maximum?: number;
  $ref?: string;
  $defs?: Record<string, JsonSchema>;
  additionalProperties?: boolean | JsonSchema;
  /** A secret: the API shows `{"$secret": true}` instead of its value. */
  writeOnly?: boolean;
}

/** What the API shows in place of a stored secret; sent back, it keeps the secret. */
export const SECRET = { $secret: true } as const;

export const isSecret = (v: unknown): boolean =>
  typeof v === "object" && v !== null && (v as Record<string, unknown>).$secret === true;

export interface TokenInfo {
  id: string;
  name?: string;
  created_at: string;
  expires_at?: string;
  last_used_at?: string;
  /** Only in the answer that created it. */
  token?: string;
}

export interface UserInfo extends User {
  created_at: string;
  updated_at: string;
}

export interface EventPage {
  events: MedicusEvent[];
  next: number;
}

/** The settings sections that are maps of named items. */
export type Section = "nodes" | "partitions" | "peers" | "roles" | "principals";

const q = encodeURIComponent;

export const api = {
  setupStatus: () => call<{ required: boolean }>("GET", "/setup"),
  setup: (r: { token: string; name: string; password: string; display_name?: string }) =>
    call<User>("POST", "/setup", r),
  login: (name: string, password: string) => call<User>("POST", "/auth/login", { name, password }),
  logout: () => call<void>("POST", "/auth/logout"),
  me: () => call<Me>("GET", "/auth/me"),
  system: () => call<SystemInfo>("GET", "/system"),
  nodes: () => call<NodeInfo[]>("GET", "/nodes"),
  jobs: (limit = 10) => call<Job[]>("GET", `/jobs?limit=${limit}`),
  settings: () => call<SettingsState>("GET", "/settings"),
  putSettings: (revision: number, settings: Settings, comment: string) =>
    call<Applied>("PUT", `/settings?comment=${encodeURIComponent(comment)}`, settings, {
      "If-Match": `"${revision}"`,
    }),
  restart: () => call<{ restarting: boolean }>("POST", "/system/restart"),
  section: <T>(section: Section) => call<Record<string, T>>("GET", `/settings/${section}`),
  putItem: (section: Section, name: string, item: unknown) =>
    call<Applied>("PUT", `/settings/${section}/${q(name)}`, item),
  deleteItem: (section: Section, name: string) => call<Applied>("DELETE", `/settings/${section}/${q(name)}`),
  history: (limit = 50) => call<RevisionSummary[]>("GET", `/settings/history?limit=${limit}`),
  revision: (n: number) => call<RevisionSummary & { settings: Settings }>("GET", `/settings/revisions/${n}`),
  rollback: (n: number) => call<Applied>("POST", `/settings/revisions/${n}/rollback`),
  addons: () => call<AddonInfo[]>("GET", "/addons"),
  putAddon: (name: string, change: { enabled?: boolean; config?: unknown }) =>
    call<Applied>("PUT", `/addons/${q(name)}`, change),
  users: () => call<UserInfo[]>("GET", "/users"),
  createUser: (u: { name: string; password?: string; display_name?: string; roles: string[] }) =>
    call<UserInfo>("POST", "/users", u),
  updateUser: (name: string, change: { display_name?: string; roles?: string[]; disabled?: boolean; password?: string }) =>
    call<UserInfo>("PATCH", `/users/${q(name)}`, change),
  deleteUser: (name: string) => call<void>("DELETE", `/users/${q(name)}`),
  tokens: () => call<TokenInfo[]>("GET", "/auth/tokens"),
  createToken: (name?: string, days?: number) => call<TokenInfo>("POST", "/auth/tokens", { name, days }),
  deleteToken: (id: string) => call<void>("DELETE", `/auth/tokens/${id}`),
  changePassword: (current: string, next: string) => call<void>("PUT", "/auth/password", { current, new: next }),
  echoPeer: (name: string) => call<void>("POST", `/peers/${q(name)}/echo`),
  jobsByState: (state: string | undefined, limit: number, offset: number) =>
    call<Job[]>("GET", `/jobs?limit=${limit}&offset=${offset}${state ? `&state=${state}` : ""}`),
  cancelJob: (id: string) => call<Job>("POST", `/jobs/${id}/cancel`),
  events: (params: { tail?: number; since?: number; limit?: number; kind?: string }) =>
    call<EventPage>(
      "GET",
      `/events?${new URLSearchParams(
        Object.entries(params)
          .filter(([, v]) => v !== undefined && v !== "")
          .map(([k, v]) => [k, String(v)]),
      )}`,
    ),
};

/** A DICOM JSON object (PS3.18 F): `{"GGGGEEEE": {"vr", "Value"}}`. */
export type DicomJson = Record<string, { vr: string; Value?: unknown[]; BulkDataURI?: string }>;

/** QIDO-RS and WADO-RS of a node, through the core's DICOMweb proxy. */
export async function dicomweb<T>(path: string, accept = "application/dicom+json"): Promise<T> {
  const response = await fetch(path, { credentials: "same-origin", headers: { Accept: accept } });
  if (response.status === 204) return [] as T;
  const text = await response.text();
  if (!response.ok) {
    let message = response.statusText;
    let code = "error";
    try {
      const e = JSON.parse(text) as { message?: string; error?: string };
      message = e.message ?? message;
      code = e.error ?? code;
    } catch {
      // Not JSON.
    }
    throw new ApiError(response.status, code, message);
  }
  return (text ? JSON.parse(text) : []) as T;
}

/** Resolves once the server answers again after a restart (it first has to go away). */
export async function waitForRestart(timeoutMs = 60_000): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
  await sleep(750);
  while (Date.now() < deadline) {
    try {
      const response = await fetch("/health/ready", { cache: "no-store" });
      if (response.ok) return;
    } catch {
      // Not back yet.
    }
    await sleep(500);
  }
  throw new Error("the server did not come back within a minute");
}
