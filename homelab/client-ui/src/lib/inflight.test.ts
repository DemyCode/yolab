import { describe, expect, it } from "vitest";
import { isPending, joinOrStart, startFresh } from "./inflight";

function deferred<T>() {
  let resolve!: (v: T) => void;
  let reject!: (e: unknown) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

describe("in-flight requests", () => {
  it("a poll joins the request that is still waiting instead of sending another", async () => {
    const d = deferred<number>();
    let calls = 0;
    const run = () => {
      calls++;
      return d.promise;
    };
    const first = joinOrStart("disks", run);
    const second = joinOrStart("disks", run);
    expect(calls).toBe(1);
    d.resolve(7);
    expect(await first).toBe(7);
    expect(await second).toBe(7);
  });

  it("a new request goes out once the previous one has answered", async () => {
    let calls = 0;
    const run = async () => ++calls;
    expect(await joinOrStart("health", run)).toBe(1);
    expect(isPending("health")).toBe(false);
    expect(await joinOrStart("health", run)).toBe(2);
  });

  it("a failed request does not block the next one", async () => {
    await expect(
      joinOrStart("ceph", () => Promise.reject(new Error("down"))),
    ).rejects.toThrow("down");
    expect(isPending("ceph")).toBe(false);
    expect(await joinOrStart("ceph", async () => "up")).toBe("up");
  });

  it("an explicit refresh after a change always asks again", async () => {
    const old = deferred<string>();
    joinOrStart("apps", () => old.promise);
    let calls = 0;
    const fresh = startFresh("apps", async () => {
      calls++;
      return "after the change";
    });
    expect(calls).toBe(1);
    expect(await fresh).toBe("after the change");
    old.resolve("before the change");
    expect(await joinOrStart("apps", async () => "next")).toBe("next");
  });

  it("different resources never wait on each other", () => {
    const a = deferred<number>();
    joinOrStart("a", () => a.promise);
    let calls = 0;
    joinOrStart("b", async () => ++calls);
    expect(calls).toBe(1);
    a.resolve(0);
  });
});
