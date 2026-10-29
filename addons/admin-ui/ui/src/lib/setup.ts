// What the setup wizard does with the operator's choices: one settings change, then a restart.

import type { Settings } from "./api";

export interface SetupChoices {
  serverName: string;
  /** Where the blob store keeps files; empty: inside the server's data directory. */
  storagePath: string;
  dimse: { enabled: boolean; port: number };
  dicomweb: { enabled: boolean; port: number };
  node: { name: string; aeTitle: string; dicomwebPath: string };
}

export const defaultChoices: SetupChoices = {
  serverName: "medicus",
  storagePath: "",
  dimse: { enabled: true, port: 11112 },
  dicomweb: { enabled: true, port: 8080 },
  node: { name: "ARCHIVE", aeTitle: "ARCHIVE", dicomwebPath: "archive" },
};

/** The problems with the choices, by field; empty when they can be applied. */
export function validate(c: SetupChoices): Record<string, string> {
  const errors: Record<string, string> = {};
  if (!c.serverName.trim()) errors.serverName = "Give the server a name.";
  for (const [key, p] of [
    ["dimse", c.dimse],
    ["dicomweb", c.dicomweb],
  ] as const) {
    if (p.enabled && !(Number.isInteger(p.port) && p.port > 0 && p.port < 65536)) {
      errors[`${key}Port`] = "Use a port between 1 and 65535.";
    }
  }
  if (c.dimse.enabled && c.dicomweb.enabled && c.dimse.port === c.dicomweb.port) {
    errors.dicomwebPort = "DIMSE and DICOMweb need different ports.";
  }
  if (!c.node.name.trim()) errors.nodeName = "Give the node a name.";
  if (c.dimse.enabled && !/^[\x20-\x7e]{1,16}$/.test(c.node.aeTitle.trim())) {
    errors.aeTitle = "AE titles have 1 to 16 characters.";
  }
  if (c.dicomweb.enabled && !/^[A-Za-z0-9_-]+$/.test(c.node.dicomwebPath)) {
    errors.dicomwebPath = "Use letters, digits, - and _.";
  }
  return errors;
}

/** The settings after the setup: `current` with the choices applied. */
export function planSettings(current: Settings, c: SetupChoices): Settings {
  const next = structuredClone(current);
  next.server = { ...next.server, name: c.serverName.trim() };
  const addons = { ...next.addons };
  if (c.storagePath.trim()) {
    addons["storage-fs"] = { enabled: true, config: { root: c.storagePath.trim(), name: "local" } };
  }
  addons.dimse = c.dimse.enabled
    ? { enabled: true, config: { listen: `0.0.0.0:${c.dimse.port}` } }
    : { enabled: false };
  addons.dicomweb = c.dicomweb.enabled
    ? { enabled: true, config: { listen: `0.0.0.0:${c.dicomweb.port}` } }
    : { enabled: false };
  next.addons = addons;
  const node: Record<string, unknown> = { partition: "default" };
  if (c.dimse.enabled) node.dimse = { ae_title: c.node.aeTitle.trim() };
  if (c.dicomweb.enabled) node.dicomweb = { path: c.node.dicomwebPath };
  next.nodes = { [c.node.name.trim()]: node };
  return next;
}

/** The addons that will run after the setup, and why. */
export function plannedAddons(c: SetupChoices): { name: string; why: string }[] {
  const out = [
    { name: "admin-ui", why: "always enabled" },
    { name: "index-sqlite", why: "the index, required by admin-ui and the archive" },
    { name: "storage-fs", why: "the blob store, required by the archive" },
  ];
  if (c.dimse.enabled) out.push({ name: "dimse", why: `DIMSE on port ${c.dimse.port}` });
  if (c.dicomweb.enabled) out.push({ name: "dicomweb", why: `DICOMweb on port ${c.dicomweb.port}` });
  return out;
}
