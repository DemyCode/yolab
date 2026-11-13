import { describe, expect, it } from "vitest";
import { keyName, looksLikePrivateKey, sshCommand } from "./ssh";

describe("sshCommand", () => {
  it("names the port only when it is not the default", () => {
    expect(sshCommand("homelab", "app.7.yolab.io", 2222)).toBe(
      "ssh -p 2222 homelab@app.7.yolab.io",
    );
    expect(sshCommand("homelab", "app.7.yolab.io", 22)).toBe(
      "ssh homelab@app.7.yolab.io",
    );
    expect(sshCommand("homelab", "app.7.yolab.io", null)).toBe(
      "ssh homelab@app.7.yolab.io",
    );
  });

  it("brackets a bare IPv6 address", () => {
    expect(sshCommand("homelab", "2a01:4f8::12", 2222)).toBe(
      "ssh -p 2222 homelab@[2a01:4f8::12]",
    );
  });
});

describe("keyName", () => {
  const key = {
    id: "a",
    kind: "ssh-ed25519",
    fingerprint: "SHA256:x",
    comment: "me@laptop",
    label: "",
  };

  it("prefers the name given here, then the key's own comment, then its type", () => {
    expect(keyName({ ...key, label: "Work laptop" })).toBe("Work laptop");
    expect(keyName(key)).toBe("me@laptop");
    expect(keyName({ ...key, comment: "" })).toBe("ssh-ed25519");
  });
});

describe("looksLikePrivateKey", () => {
  it("spots a private key before it is sent anywhere", () => {
    expect(
      looksLikePrivateKey("-----BEGIN OPENSSH PRIVATE KEY-----\nabc"),
    ).toBe(true);
    expect(looksLikePrivateKey("ssh-ed25519 AAAA me@laptop")).toBe(false);
  });
});
