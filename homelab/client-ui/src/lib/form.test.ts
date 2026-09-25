import { describe, expect, it } from "vitest";
import { showIfMet } from "./form";

describe("showIfMet", () => {
  it("shows a field with no condition attached", () => {
    expect(showIfMet(undefined, {})).toBe(true);
    expect(showIfMet({}, {})).toBe(true);
  });

  it("shows the field when the controlling value matches", () => {
    expect(
      showIfMet(
        { file_explorer_enabled: true },
        { file_explorer_enabled: true },
      ),
    ).toBe(true);
    expect(showIfMet({ mode: "advanced" }, { mode: "advanced" })).toBe(true);
  });

  it("hides the field when the controlling value differs", () => {
    expect(
      showIfMet(
        { file_explorer_enabled: true },
        { file_explorer_enabled: false },
      ),
    ).toBe(false);
    expect(showIfMet({ mode: "advanced" }, { mode: "simple" })).toBe(false);
  });

  it("hides the field while the controlling value is still unset", () => {
    expect(showIfMet({ file_explorer_enabled: true }, {})).toBe(false);
  });

  it("requires every condition to hold", () => {
    expect(
      showIfMet({ a: true, b: "x" }, { a: true, b: "x", c: "anything" }),
    ).toBe(true);
    expect(showIfMet({ a: true, b: "x" }, { a: true, b: "y" })).toBe(false);
  });
});
