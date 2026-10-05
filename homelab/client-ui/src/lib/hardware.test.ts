import { describe, expect, it } from "vitest";
import { hardwareDetail, hardwareLabel } from "./hardware";

const machine = {
  accelerator: "nvidia" as const,
  vram_gib: 24,
  ram_gib: 64,
  game_input: true,
};

describe("hardwareLabel", () => {
  it("names the accelerator a machine offers", () => {
    expect(hardwareLabel(machine)).toBe("NVIDIA GPU");
    expect(hardwareLabel({ ...machine, accelerator: "cpu" })).toBe("CPU only");
  });

  it("says nothing for a machine that was never inspected rather than claiming CPU only", () => {
    expect(hardwareLabel(undefined)).toBeNull();
    expect(hardwareLabel({ ...machine, accelerator: null })).toBeNull();
  });
});

describe("hardwareDetail", () => {
  it("shows graphics memory before system memory", () => {
    expect(hardwareDetail(machine)).toBe("24 GB VRAM · 64 GB RAM");
  });

  it("leaves out what is unknown", () => {
    expect(hardwareDetail({ ...machine, vram_gib: null })).toBe("64 GB RAM");
    expect(
      hardwareDetail({ ...machine, vram_gib: null, ram_gib: null }),
    ).toBeNull();
  });
});
