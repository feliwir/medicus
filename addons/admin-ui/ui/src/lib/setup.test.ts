import { describe, expect, it } from "vitest";
import type { Settings } from "./api";
import { defaultChoices, planSettings, plannedAddons, validate } from "./setup";

const current: Settings = {
  server: { name: "medicus" },
  addons: { "events-webhook": { enabled: false } },
  nodes: { MEDICUS: { partition: "default" } },
  jobs: { concurrency: 4 },
};

describe("setup", () => {
  it("applies the choices as one settings change", () => {
    const next = planSettings(current, { ...defaultChoices, storagePath: "/srv/blobs" });
    expect(next.server.name).toBe("medicus");
    expect(next.addons["events-webhook"]).toEqual({ enabled: false });
    expect(next.addons["storage-fs"]).toEqual({ enabled: true, config: { root: "/srv/blobs", name: "local" } });
    expect(next.addons.dimse).toEqual({ enabled: true, config: { listen: "0.0.0.0:11112" } });
    expect(next.nodes).toEqual({
      ARCHIVE: { partition: "default", dimse: { ae_title: "ARCHIVE" }, dicomweb: { path: "archive" } },
    });
    expect(next.jobs).toEqual({ concurrency: 4 });
    expect(current.nodes).toHaveProperty("MEDICUS");
  });

  it("leaves out what is disabled", () => {
    const c = { ...defaultChoices, dimse: { enabled: false, port: 11112 } };
    const next = planSettings(current, c);
    expect(next.addons.dimse).toEqual({ enabled: false });
    expect(next.nodes.ARCHIVE).toEqual({ partition: "default", dicomweb: { path: "archive" } });
    expect(next.addons["storage-fs"]).toBeUndefined();
    expect(plannedAddons(c).map((a) => a.name)).toEqual(["admin-ui", "index-sqlite", "storage-fs", "dicomweb"]);
  });

  it("validates", () => {
    expect(validate(defaultChoices)).toEqual({});
    const bad = validate({
      ...defaultChoices,
      dicomweb: { enabled: true, port: 11112 },
      node: { name: "", aeTitle: "WAY_TOO_LONG_AE_TITLE", dicomwebPath: "a b" },
    });
    expect(Object.keys(bad).sort()).toEqual(["aeTitle", "dicomwebPath", "dicomwebPort", "nodeName"]);
  });
});
