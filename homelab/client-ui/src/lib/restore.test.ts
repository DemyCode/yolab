import { describe, expect, it } from "vitest";
import {
  NO_CHOICES,
  choicesFor,
  dataOptions,
  type DataRow,
  type RestorePlan,
} from "./restore";

const data: DataRow = {
  claim: "jellyfin-data",
  capacity: "50Gi",
  backed_up: true,
};

const values = (options: { value: string }[]) => options.map((o) => o.value);

describe("what can happen to the app's own data", () => {
  it("a reinstall brings the app's own data back from the backup", () => {
    expect(values(dataOptions(data, "reinstall"))).toEqual([
      "restore",
      "empty",
    ]);
  });

  it("restoring a running app replaces its data unless told to keep it", () => {
    expect(values(dataOptions(data, "in-place"))).toEqual(["replace", "use"]);
  });

  it("data that was never saved is left as it is", () => {
    expect(
      values(dataOptions({ ...data, backed_up: false }, "in-place")),
    ).toEqual(["use"]);
  });

  it("a reinstall of data that was never saved starts it empty", () => {
    expect(
      values(dataOptions({ ...data, backed_up: false }, "reinstall")),
    ).toEqual(["empty"]);
  });
});

describe("the choices sent with a restore", () => {
  const plan: RestorePlan = {
    data: [data, { ...data, claim: "jellyfin-cache", backed_up: false }],
  };

  it("every volume gets its first option unless the person picked another", () => {
    expect(choicesFor(plan, "reinstall", NO_CHOICES)).toEqual({
      data: { "jellyfin-data": "restore", "jellyfin-cache": "empty" },
    });
    expect(
      choicesFor(plan, "reinstall", { data: { "jellyfin-data": "empty" } }),
    ).toEqual({
      data: { "jellyfin-data": "empty", "jellyfin-cache": "empty" },
    });
  });

  it("a pick that no longer fits the volume falls back to its first option", () => {
    expect(
      choicesFor(plan, "in-place", { data: { "jellyfin-data": "restore" } })
        .data["jellyfin-data"],
    ).toBe("replace");
  });
});
