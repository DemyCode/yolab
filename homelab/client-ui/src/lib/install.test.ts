import { describe, expect, it } from "vitest";
import {
  copiesDataByDefault,
  installBlocker,
  installOrigin,
  installSource,
  keepsSourceAddress,
  snapshotNamespace,
  stripInstanceId,
  seedForm,
  instanceNameFor,
} from "./install";
import type { AppDefinition } from "@/types/apps";
import type { ConfigSchema } from "./schema";

function definition(over: Partial<AppDefinition> = {}): AppDefinition {
  return {
    schema: 1,
    app_id: "gitea",
    chart_repo: "official",
    chart_version: "1.0.0",
    instance_name: "gitea-ab12",
    service_name: "",
    config: {},
    volumes: [],
    resources: {
      cpu_millicores: 0,
      memory_bytes: 0,
      gpu: 0,
      replicas: 1,
    },
    backup: { enabled: true, schedule: "", last_ok_at: null, running: false },
    ...over,
  };
}

const params = (q: string) => new URLSearchParams(q);

describe("installOrigin", () => {
  it("reads a plain install", () => {
    expect(installOrigin(params("")).mode).toBe("fresh");
  });

  it("reads a duplicate", () => {
    const origin = installOrigin(params("from=gitea-ab12"));
    expect(origin.mode).toBe("duplicate");
    expect(origin.fromInstance).toBe("gitea-ab12");
  });

  it("reads a restore", () => {
    const origin = installOrigin(
      params("restore=yolab-gitea-ab12&snapshot=d1"),
    );
    expect(origin.mode).toBe("restore");
    expect(origin.namespace).toBe("yolab-gitea-ab12");
    expect(origin.snapshot).toBe("d1");
  });

  it("is not a restore without the backup to restore from", () => {
    expect(installOrigin(params("restore=yolab-gitea-ab12")).mode).toBe(
      "fresh",
    );
  });
});

describe("copiesDataByDefault", () => {
  it("brings the files back when restoring, because that is why you restore", () => {
    expect(copiesDataByDefault("restore")).toBe(true);
  });

  it("leaves a duplicate empty until you ask for the files", () => {
    expect(copiesDataByDefault("duplicate")).toBe(false);
    expect(copiesDataByDefault("fresh")).toBe(false);
  });
});

describe("snapshotNamespace", () => {
  it("looks for a restore's backups under the namespace being restored", () => {
    expect(
      snapshotNamespace(
        installOrigin(params("restore=yolab-gitea-ab12&snapshot=d1")),
      ),
    ).toBe("yolab-gitea-ab12");
  });

  it("a duplicate copies the app's own files, not a backup, so there is nowhere to look", () => {
    expect(snapshotNamespace(installOrigin(params("from=gitea-ab12")))).toBe(
      null,
    );
  });

  it("has nowhere to look for a brand new app", () => {
    expect(snapshotNamespace(installOrigin(params("")))).toBeNull();
  });
});

describe("installSource", () => {
  it("sends nothing for a plain install", () => {
    expect(installSource(installOrigin(params("")), false, "")).toBeUndefined();
  });

  it("duplicates without data", () => {
    const source = installSource(
      installOrigin(params("from=gitea-ab12")),
      false,
      "",
    );
    expect(source).toEqual({
      kind: "duplicate",
      from_instance: "gitea-ab12",
      with_data: false,
    });
  });

  it("duplicates with data without naming any backup — the copy is direct", () => {
    const source = installSource(
      installOrigin(params("from=gitea-ab12")),
      true,
      "",
    );
    expect(source).toEqual({
      kind: "duplicate",
      from_instance: "gitea-ab12",
      with_data: true,
    });
  });

  it("restores with data", () => {
    const source = installSource(
      installOrigin(params("restore=yolab-gitea-ab12&snapshot=d1")),
      true,
      "d1",
    );
    expect(source).toEqual({
      kind: "backup",
      namespace: "yolab-gitea-ab12",
      snapshot_id: "d1",
      with_data: true,
    });
  });

  it("restores without data but still names the backup its settings come from", () => {
    const source = installSource(
      installOrigin(params("restore=yolab-gitea-ab12&snapshot=d1")),
      false,
      "d1",
    );
    expect(source).toEqual({
      kind: "backup",
      namespace: "yolab-gitea-ab12",
      snapshot_id: "d1",
      with_data: false,
    });
  });

  it("falls back to the backup the restore was started from", () => {
    const source = installSource(
      installOrigin(params("restore=yolab-gitea-ab12&snapshot=d1")),
      false,
      "",
    );
    expect(source).toMatchObject({ snapshot_id: "d1" });
  });
});

describe("keepsSourceAddress", () => {
  it("gives a restored app its old web address back", () => {
    expect(keepsSourceAddress("restore")).toBe(true);
  });

  it("never lets a duplicate take the address its original is still using", () => {
    expect(keepsSourceAddress("duplicate")).toBe(false);
  });
});

describe("instanceNameFor", () => {
  it("uses the chart's own id for a new app — nobody is asked to invent one", () => {
    expect(instanceNameFor("fresh", "gitea", null)).toBe("gitea");
  });

  it("uses the chart id for a duplicate too; the server adds the unique part", () => {
    expect(
      instanceNameFor(
        "duplicate",
        "gitea",
        definition({ instance_name: "gitea-ab12" }),
      ),
    ).toBe("gitea");
  });

  it("gives a restored app its own name back, without its generated id", () => {
    expect(
      instanceNameFor(
        "restore",
        "gitea",
        definition({ instance_name: "my-code-x7k2" }),
      ),
    ).toBe("my-code");
  });

  it("falls back to the chart id when the backup never recorded a name", () => {
    expect(
      instanceNameFor("restore", "gitea", definition({ instance_name: "" })),
    ).toBe("gitea");
  });
});
describe("stripInstanceId", () => {
  it("removes a generated id", () => {
    expect(stripInstanceId("gitea-ab23")).toBe("gitea");
  });

  it("leaves a name the user chose alone", () => {
    expect(stripInstanceId("gitea")).toBe("gitea");
    expect(stripInstanceId("gitea-staging")).toBe("gitea-staging");
  });

  it("does not mistake a letter excluded from ids for one", () => {
    expect(stripInstanceId("build-loop")).toBe("build-loop");
  });
});

describe("installBlocker", () => {
  const ok = {
    instanceName: "gitea",
    requiredMissing: false,
    withData: false,
    needsBackup: true,
    snapshot: "",
    snapshotsLoaded: true,
    snapshotCount: 0,
  };

  it("lets a complete form through", () => {
    expect(installBlocker(ok)).toBeNull();
  });

  it("will not install something with no name", () => {
    expect(installBlocker({ ...ok, instanceName: "" })).toMatch(/name/i);
  });

  it("will not install with a required field left empty", () => {
    expect(installBlocker({ ...ok, requiredMissing: true })).toMatch(
      /required/i,
    );
  });

  it("says so when a restore asked for data and there is no backup at all", () => {
    expect(
      installBlocker({
        ...ok,
        withData: true,
        needsBackup: true,
        snapshotCount: 0,
      }),
    ).toMatch(/no backup/i);
  });

  it("waits rather than complaining while the backups are still loading", () => {
    expect(
      installBlocker({
        ...ok,
        withData: true,
        needsBackup: true,
        snapshotsLoaded: false,
        snapshotCount: 0,
      }),
    ).toMatch(/pick the backup/i);
  });

  it("lets a restore through once a backup is picked", () => {
    expect(
      installBlocker({
        ...ok,
        withData: true,
        needsBackup: true,
        snapshot: "d1",
        snapshotCount: 3,
      }),
    ).toBeNull();
  });

  it("never asks a duplicate for a backup, even when copying its files", () => {
    expect(
      installBlocker({
        ...ok,
        withData: true,
        needsBackup: false,
        snapshot: "",
        snapshotCount: 0,
      }),
    ).toBeNull();
  });

  it("does not ask for a backup when no data was asked for", () => {
    expect(installBlocker({ ...ok, withData: false })).toBeNull();
  });
});

describe("seedForm", () => {
  const schema = {
    properties: {
      subdomain: { type: "string", format: "tunnel", default: "files" },
      password: {
        type: "string",
        writeOnly: true,
        generate: true,
        minLength: 32,
      },
      pin: { type: "string", writeOnly: true },
      storage_size: { type: "string", default: "50Gi" },
    },
  };
  const counter = () => {
    const lengths: number[] = [];
    const generate = (n: number) => {
      lengths.push(n);
      return "x".repeat(n);
    };
    return { lengths, generate };
  };

  it("fills defaults and generates the credentials that ask for it", () => {
    const { lengths, generate } = counter();
    const seed = seedForm(schema, null, "fresh", generate);
    expect(seed).toEqual({
      subdomain: "files",
      password: "x".repeat(32),
      storage_size: "50Gi",
    });
    expect(lengths).toEqual([32]);
  });

  it("never invents a credential the person is meant to type", () => {
    const seed = seedForm(schema, null, "fresh", counter().generate);
    expect(seed.pin).toBeUndefined();
  });

  it("generates at least 24 characters even when the schema allows fewer", () => {
    const { lengths, generate } = counter();
    seedForm(
      { properties: { key: { writeOnly: true, generate: true } } },
      null,
      "fresh",
      generate,
    );
    expect(lengths).toEqual([24]);
  });

  const explorer: ConfigSchema = {
    properties: {
      file_explorer_enabled: { type: "boolean", default: true },
    },
    dependencies: {
      file_explorer_enabled: {
        oneOf: [
          { properties: { file_explorer_enabled: { const: false } } },
          {
            properties: {
              file_explorer_enabled: { const: true },
              file_explorer_password: {
                type: "string",
                writeOnly: true,
                generate: true,
                minLength: 12,
              },
            },
          },
        ],
      },
    },
  };

  it("generates a credential that only appears once its toggle is on", () => {
    const { lengths, generate } = counter();
    const seed = seedForm(explorer, null, "fresh", generate);
    expect(seed.file_explorer_password).toBe("x".repeat(24));
    expect(lengths).toEqual([24]);
  });

  it("keeps a copied app's revealed credential instead of generating a new one", () => {
    const { lengths, generate } = counter();
    const seed = seedForm(
      explorer,
      { file_explorer_enabled: true, file_explorer_password: "__redacted__" },
      "duplicate",
      generate,
    );
    expect(seed.file_explorer_password).toBe("__redacted__");
    expect(lengths).toEqual([]);
  });

  it("keeps a copied app's settings, credentials included, instead of generating new ones", () => {
    const { lengths, generate } = counter();
    const seed = seedForm(
      schema,
      {
        subdomain: "files",
        password: "__redacted__",
        storage_size: "500Gi",
      },
      "restore",
      generate,
    );
    expect(seed.password).toBe("__redacted__");
    expect(seed.storage_size).toBe("500Gi");
    expect(lengths).toEqual([]);
  });

  it("gives a duplicate its own address rather than the original's", () => {
    const seed = seedForm(
      schema,
      { subdomain: "files", storage_size: "500Gi" },
      "duplicate",
      counter().generate,
    );
    expect(seed.subdomain).toBeUndefined();
  });

  it("fills in defaults for settings the copied app never had", () => {
    const seed = seedForm(
      schema,
      { subdomain: "files" },
      "restore",
      counter().generate,
    );
    expect(seed.storage_size).toBe("50Gi");
  });
});

describe("seedForm with the YoLab address switch", () => {
  const schema: ConfigSchema = {
    properties: { yolab_enabled: { type: "boolean", default: true } },
    dependencies: {
      yolab_enabled: {
        oneOf: [
          { properties: { yolab_enabled: { const: false } } },
          {
            properties: {
              yolab_enabled: { const: true },
              subdomain: {
                type: "string",
                format: "tunnel",
                default: "jellyfin",
              },
              yolab_token: {
                type: "string",
                format: "yolab-token",
                writeOnly: true,
              },
            },
          },
        ],
      },
    },
  };

  it("starts a new install on, with the app's subdomain and no token", () => {
    const seed = seedForm(schema, null, "fresh", () => "x");
    expect(seed).toEqual({ yolab_enabled: true, subdomain: "jellyfin" });
  });
});
