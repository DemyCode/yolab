import { describe, expect, it } from "vitest";
import {
  adoptsDefault,
  boxesOf,
  linkChoice,
  linkLabel,
  linksQuery,
  missingLabel,
  usedBy,
  usesOf,
  type LinkCandidate,
} from "./links";

const qbittorrent: LinkCandidate = {
  reference: "downloads",
  instance: "downloads",
  member: null,
  app_id: "qbittorrent",
};

const inPack: LinkCandidate = {
  reference: "media/qbittorrent",
  instance: "media",
  member: "qbittorrent",
  app_id: "movies-tv",
};

describe("choosing whose outputs an app takes", () => {
  it("an app that can run its own does so until told otherwise", () => {
    expect(linkChoice({}, [qbittorrent], true, false)).toEqual({ kind: "own" });
  });

  it("an app that needs one starts on the first installed one and saves it", () => {
    const choice = linkChoice({}, [inPack, qbittorrent], false, false);
    expect(choice).toEqual({ kind: "app", reference: "media/qbittorrent" });
    expect(adoptsDefault({}, choice)).toBe("media/qbittorrent");
    expect(adoptsDefault({ $from: "media/qbittorrent" }, choice)).toBeNull();
  });

  it("with nothing installed it asks for the values by hand", () => {
    expect(linkChoice({}, [], false, false)).toEqual({ kind: "typed" });
  });

  it("a saved choice of an app that is gone is shown as missing", () => {
    expect(linkChoice({ $from: "old" }, [qbittorrent], true, false)).toEqual({
      kind: "missing",
      reference: "old",
    });
  });

  it("values typed by hand stay typed", () => {
    expect(
      linkChoice(
        { internal_url: "http://nas:8080" },
        [qbittorrent],
        true,
        false,
      ),
    ).toEqual({ kind: "typed" });
  });

  it("a choice saved by the old connections still points at its app", () => {
    expect(
      linkChoice({ from: "yolab-downloads" }, [qbittorrent], true, false),
    ).toEqual({ kind: "app", reference: "downloads" });
  });
});

describe("how a choice is named", () => {
  it("an app inside a pack is named with its pack", () => {
    expect(linkLabel(inPack)).toBe("qbittorrent in media");
    expect(missingLabel("media/qbittorrent")).toBe("qbittorrent in media");
  });

  it("an app named after its chart is not named twice", () => {
    expect(linkLabel({ ...qbittorrent, instance: "qbittorrent" })).toBe(
      "qbittorrent",
    );
    expect(linkLabel(qbittorrent)).toBe("downloads (qbittorrent)");
  });
});

describe("what the field asks the box for", () => {
  const field = {
    type: "object",
    format: "yolab-output",
    "x-yolab-from": ["qbittorrent"],
    "x-yolab-outputs": ["internal_url", "password"],
    properties: {
      internal_url: { type: "string", title: "Address" },
      password: { type: "string", title: "Password" },
    },
  };

  it("asks for the apps that show every output the field takes", () => {
    expect(linksQuery(field)).toBe(
      "/api/links?from=qbittorrent&outputs=internal_url%2Cpassword",
    );
  });

  it("an old connection field asks for the outputs it has boxes for", () => {
    expect(
      linksQuery({
        format: "connection",
        properties: { from: {}, api: { title: "Address" } },
      }),
    ).toBe("/api/links?from=&outputs=api");
  });

  it("offers one box per output to type by hand", () => {
    expect(boxesOf(field).map(([k]) => k)).toEqual([
      "internal_url",
      "password",
    ]);
  });
});

describe("which apps an app uses and is used by", () => {
  const base = {
    app_id: "x",
    chart_version: "1",
    status: "running" as const,
    detail: "",
    outputs: [],
    backup: { enabled: false, schedule: "", last_ok_at: null, running: false },
  };
  const media = { ...base, instance_name: "media", config: {} };
  const radarr = {
    ...base,
    instance_name: "films",
    config: { download_client: { $from: "media/qbittorrent" }, subdomain: "f" },
  };
  const old = {
    ...base,
    instance_name: "chat",
    config: { ollama: { from: "yolab-media" } },
  };
  const typed = {
    ...base,
    instance_name: "notes",
    config: { download_client: { internal_url: "http://pc:8080" } },
  };

  it("an app uses the apps its links name, packs included", () => {
    expect(usesOf(radarr)).toEqual(["media"]);
    expect(usesOf(old)).toEqual(["media"]);
    expect(usesOf(typed)).toEqual([]);
  });

  it("an app is used by every app linked to it", () => {
    expect(usedBy(media, [media, radarr, old, typed])).toEqual([
      "chat",
      "films",
    ]);
    expect(usedBy(radarr, [media, radarr, old, typed])).toEqual([]);
  });
});
