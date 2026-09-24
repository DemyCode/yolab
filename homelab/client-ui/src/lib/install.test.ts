import { describe, expect, it } from "vitest";
import {
  addressTakenBy,
  copiesDataByDefault,
  installBlocker,
  installOrigin,
  installSource,
  keepsSourceAddress,
  snapshotNamespace,
  stripInstanceId,
  phaseFrom,
  instanceNameFor,
} from "./install";
import type { AppDefinition, AppInfo } from "@/types/apps";

function app(over: Partial<AppInfo> = {}): AppInfo {
  return {
    app_id: "gitea",
    instance_name: "gitea-ab12",
    status: "running",
    detail: "",
    outputs: [],
    outputs_spec: [],
    config: {},
    backup: { enabled: true, schedule: "", last_ok_at: null, running: false },
    ...over,
  };
}

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

describe("addressTakenBy", () => {
  it("names the app already answering on that address", () => {
    expect(addressTakenBy("git", [app({ config: { subdomain: "git" } })])).toBe(
      "gitea-ab12",
    );
  });

  it("says nothing when the address is free", () => {
    expect(
      addressTakenBy("git", [app({ config: { subdomain: "wiki" } })]),
    ).toBe(null);
    expect(addressTakenBy("", [app({ config: { subdomain: "" } })])).toBe(null);
  });
});

describe("installBlocker", () => {
  const ok = {
    instanceName: "gitea",
    addressTakenBy: null,
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

  it("will not install onto a web address already in use", () => {
    expect(installBlocker({ ...ok, addressTakenBy: "gitea-ab12" })).toContain(
      "gitea-ab12",
    );
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

describe("phaseFrom", () => {
  it("takes the server's own wording when it announces a step", () => {
    expect(phaseFrom("Copying this app’s files…")).toBe(
      "Copying this app’s files",
    );
    expect(phaseFrom("Reading the backup…")).toBe("Reading the backup");
  });

  it("recognises helm's own output once the chart goes in", () => {
    expect(
      phaseFrom('Release "gitea" does not exist. Installing it now.'),
    ).toBe(null);
    expect(phaseFrom("STATUS: pending-install")).toBe("Installing");
    expect(phaseFrom("STATUS: deployed")).toBe("Almost there");
  });

  it("leaves ordinary chatter alone rather than inventing a step", () => {
    expect(phaseFrom("NOTES:")).toBeNull();
    expect(phaseFrom("")).toBeNull();
  });
});
