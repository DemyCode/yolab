import { describe, expect, it } from "vitest";
import { fromOldBoxPath } from "./routes";

describe("fromOldBoxPath", () => {
  it("sends the old Box page to System", () => {
    expect(fromOldBoxPath("/box")).toBe("/system");
    expect(fromOldBoxPath("/box/")).toBe("/system");
  });

  it("keeps a bookmarked sub-page, including links in old alerts", () => {
    expect(fromOldBoxPath("/box/storage")).toBe("/system/storage");
    expect(fromOldBoxPath("/box/backups")).toBe("/system/backups");
  });

  it("sends the old Updates-and-system page to Updates, not back to System", () => {
    expect(fromOldBoxPath("/box/system")).toBe("/system/updates");
  });
});
