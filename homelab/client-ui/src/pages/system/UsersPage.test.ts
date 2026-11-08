import { describe, expect, it } from "vitest";
import { passwordProblem } from "./UsersPage";

describe("adding someone", () => {
  it("accepts the usernames the server accepts", () => {
    for (const name of ["admin", "alice", "bob-2", "a_b", "0day"]) {
      expect(passwordProblem(name, "long enough")).toBeNull();
    }
  });

  it("refuses the usernames the server refuses", () => {
    for (const name of ["", "Alice", "a.b", "-a", "a".repeat(33)]) {
      expect(passwordProblem(name, "long enough")).toMatch(/lowercase/);
    }
  });

  it("asks for a password of at least eight characters", () => {
    expect(passwordProblem("alice", "short")).toMatch(/8 characters/);
    expect(passwordProblem("alice", "12345678")).toBeNull();
  });
});
