// The items of the settings sections, as stored (see the server's config.rs).

export interface NodeItem {
  partition?: string;
  peer?: string;
  /** `port`: where the AE title is reachable; default, the DIMSE addon's port. */
  dimse?: { ae_title: string; port?: number };
  dicomweb?: { path: string };
}

export interface PartitionItem {
  storage?: string | null;
}

export interface PeerItem {
  protocol: "dimse";
  ae_title: string;
  host: string;
  port: number;
}

export interface Grant {
  nodes: string[];
  permissions: string[];
}

export interface RoleItem {
  grants: Grant[];
}

export interface PrincipalItem {
  dimse?: { calling_ae: string; networks: string[] } | null;
  roles: string[];
}

export const DATA_PERMISSIONS = ["query", "retrieve", "store", "send"] as const;
export const ADMIN_PERMISSIONS = ["admin.system", "admin.users", "admin.jobs", "admin.peers", "admin.events"] as const;

/** AE titles: 1–16 characters of the default character repertoire, no backslash. */
export const validAeTitle = (ae: string) => /^[\x20-\x5b\x5d-\x7e]{1,16}$/.test(ae) && ae.trim().length > 0;
