import { describe, expect, it } from "vitest";
import { webHref } from "./utils";

describe("webHref", () => {
  it("keeps web addresses", () => {
    expect(webHref("https://git.box.yolab.io/admin")).toBe(
      "https://git.box.yolab.io/admin",
    );
    expect(webHref("http://192.168.1.5:8080")).toBe("http://192.168.1.5:8080");
  });

  it("refuses a script address a service could print into its logs", () => {
    for (const evil of [
      "javascript:alert(document.cookie)",
      "JavaScript:alert(1)",
      " javascript:alert(1)",
      "java\tscript:alert(1)",
      "data:text/html,<script>alert(1)</script>",
      "vbscript:msgbox(1)",
      "file:///etc/passwd",
    ]) {
      expect(webHref(evil), evil).toBeNull();
    }
  });

  it("refuses what is not an address at all", () => {
    expect(webHref("")).toBeNull();
    expect(webHref(null)).toBeNull();
    expect(webHref(undefined)).toBeNull();
    expect(webHref("not a url")).toBeNull();
    expect(webHref("/relative/path")).toBeNull();
  });
});
