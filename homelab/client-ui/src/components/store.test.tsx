import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { AppComments, HeartButton, StoreSharing } from "./StoreCommunity";
import { fakeServer, never, reply, settle } from "@/test/server";
import { catalogApp } from "@/test/fixtures";
import type { StoreComment } from "@/lib/store";

const official = catalogApp({ id: "gitea", name: "Gitea", repo: "official" });
const custom = catalogApp({ id: "mine", repo: "community" });

describe("HeartButton", () => {
  it("is not offered for services outside the official catalog", () => {
    const server = fakeServer({});
    const { container } = render(<HeartButton app={custom} count={3} />);
    expect(container.innerHTML).toBe("");
    expect(server.calls).toHaveLength(0);
  });

  it("gives a heart, counting it at once, and takes it back", async () => {
    const server = fakeServer({
      "GET /api/store/apps/gitea/heart": { hearted: false },
      "PUT /api/store/apps/gitea/heart": undefined,
    });
    render(<HeartButton app={official} count={2} />);
    const give = await screen.findByRole("button", {
      name: "Give Gitea a heart",
    });
    expect(give.getAttribute("aria-pressed")).toBe("false");
    fireEvent.click(give);
    const back = await screen.findByRole("button", {
      name: "Take back your heart for Gitea",
    });
    expect(back.textContent).toBe("3");
    expect(back.className).toContain("border-fg");
    await waitFor(() => expect(back).toHaveProperty("disabled", false));
    expect(server.sent("PUT", "/api/store/apps/gitea/heart")[0].body).toEqual({
      hearted: true,
    });
    fireEvent.click(back);
    expect(
      (await screen.findByRole("button", { name: "Give Gitea a heart" }))
        .textContent,
    ).toBe("2");
  });

  it("undoes the heart and says why when the platform refuses", async () => {
    const server = fakeServer({
      "GET /api/store/apps/gitea/heart": { hearted: true },
      "PUT /api/store/apps/gitea/heart": reply(
        503,
        '{"error":"platform busy"}',
      ),
    });
    render(<HeartButton app={official} count={0} />);
    fireEvent.click(await screen.findByRole("button", { name: /Take back/ }));
    expect(await screen.findByRole("alert")).toBeTruthy();
    expect(screen.getByText("platform busy")).toBeTruthy();
    expect(screen.getByRole("button", { name: /Take back/ }).textContent).toBe(
      "0",
    );
    server.routes["PUT /api/store/apps/gitea/heart"] = () =>
      Promise.reject(new Error("x"));
    fireEvent.click(screen.getByRole("button", { name: /Take back/ }));
    expect(
      await screen.findByText("The YoLab platform did not answer."),
    ).toBeTruthy();
  });

  it("cannot be used while the server is not connected to YoLab", async () => {
    fakeServer({
      "GET /api/store/apps/gitea/heart": reply(502, "no platform"),
    });
    render(<HeartButton app={official} count={1} />);
    await waitFor(() =>
      expect(screen.getByRole("button").title).toBe(
        "Hearts need this server to be connected to YoLab",
      ),
    );
    expect(screen.getByRole("button")).toHaveProperty("disabled", true);
  });
});

const comment = (over: Partial<StoreComment> = {}): StoreComment => ({
  id: 1,
  author: "sam",
  body: "Works great",
  created_at: new Date().toISOString(),
  installed: false,
  mine: false,
  ...over,
});

describe("AppComments", () => {
  it("is not offered outside the official catalog", () => {
    const { container } = render(<AppComments app={custom} />);
    expect(container.innerHTML).toBe("");
  });

  it("invites the first comment, and shows nothing more while loading", async () => {
    const server = fakeServer({ "GET /api/store/apps/gitea/comments": [] });
    render(<AppComments app={official} />);
    expect(screen.queryByText("No comments yet")).toBeNull();
    expect(await screen.findByText("No comments yet")).toBeTruthy();
    expect(server.calls).toHaveLength(1);
  });

  it("says why comments are not available", async () => {
    fakeServer({
      "GET /api/store/apps/gitea/comments": reply(502, '{"error":"offline"}'),
    });
    render(<AppComments app={official} />);
    expect(
      await screen.findByText("Comments are not available right now"),
    ).toBeTruthy();
    expect(screen.getByText("offline")).toBeTruthy();
  });

  it("lists comments, deletes your own and reports others", async () => {
    const server = fakeServer({
      "GET /api/store/apps/gitea/comments": [
        comment({ id: 1, installed: true }),
        comment({ id: 2, author: "me", body: "line1\nline2", mine: true }),
      ],
      "POST /api/store/comments/1/report": undefined,
      "DELETE /api/store/comments/2": undefined,
    });
    render(<AppComments app={official} />);
    expect(await screen.findByText("installed it")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Report" }));
    expect(
      await screen.findByText(
        "Reported. It is hidden once three people report it.",
      ),
    ).toBeTruthy();
    server.routes["GET /api/store/apps/gitea/comments"] = [comment({ id: 1 })];
    fireEvent.click(screen.getByRole("button", { name: "Delete" }));
    await waitFor(() => expect(screen.queryByText(/line1/)).toBeNull());
    expect(server.sent("DELETE", "/api/store/comments/2")).toHaveLength(1);
  });

  it("says why a comment was not deleted or reported", async () => {
    fakeServer({
      "GET /api/store/apps/gitea/comments": [
        comment({ id: 1 }),
        comment({ id: 2, mine: true }),
      ],
      "POST /api/store/comments/1/report": reply(429, '{"error":"slow down"}'),
      "DELETE /api/store/comments/2": () => Promise.reject("x"),
    });
    render(<AppComments app={official} />);
    fireEvent.click(await screen.findByRole("button", { name: "Report" }));
    expect(await screen.findByText("slow down")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Delete" }));
    expect(
      await screen.findByText("The YoLab platform did not answer."),
    ).toBeTruthy();
  });

  it("posts a comment, refusing an empty or too long one", async () => {
    const server = fakeServer({
      "GET /api/store/apps/gitea/comments": [],
      "POST /api/store/apps/gitea/comments": undefined,
    });
    render(<AppComments app={official} />);
    const post = screen.getByRole("button", { name: "Post" });
    const box = screen.getByLabelText("Your comment");
    expect(post).toHaveProperty("disabled", true);
    fireEvent.change(box, { target: { value: "x".repeat(1001) } });
    expect(screen.getByRole("alert").textContent).toBe(
      "A comment is at most 1000 characters.",
    );
    expect(post).toHaveProperty("disabled", true);
    fireEvent.change(box, { target: { value: "Nice" } });
    server.routes["GET /api/store/apps/gitea/comments"] = [
      comment({ body: "Nice", mine: true }),
    ];
    fireEvent.click(post);
    expect(screen.getByRole("button", { name: "Posting…" })).toBeTruthy();
    expect(await screen.findByText("Nice")).toBeTruthy();
    expect((box as HTMLTextAreaElement).value).toBe("");
    expect(
      server.sent("POST", "/api/store/apps/gitea/comments")[0].body,
    ).toEqual({ body: "Nice" });
  });

  it("says why a comment was not posted", async () => {
    fakeServer({
      "GET /api/store/apps/gitea/comments": never(),
      "POST /api/store/apps/gitea/comments": reply(400, '{"error":"too rude"}'),
    });
    render(<AppComments app={official} />);
    fireEvent.change(screen.getByLabelText("Your comment"), {
      target: { value: "x" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Post" }));
    expect(await screen.findByText("too rude")).toBeTruthy();
  });
});

describe("StoreSharing", () => {
  it("is on until the server says otherwise, and cannot change before it answers", () => {
    fakeServer({ "GET /api/store/settings": never() });
    render(<StoreSharing />);
    const s = screen.getByRole("switch");
    expect(s.getAttribute("aria-checked")).toBe("true");
    expect(s).toHaveProperty("disabled", true);
    expect(screen.getByText(/counted once for your account/)).toBeTruthy();
  });

  it("saves a change and says so, or says why not", async () => {
    const server = fakeServer({
      "GET /api/store/settings": { share_installs: false },
      "PUT /api/store/settings": undefined,
    });
    render(<StoreSharing />);
    await waitFor(() =>
      expect(screen.getByRole("switch")).toHaveProperty("disabled", false),
    );
    expect(screen.getByRole("switch").getAttribute("aria-checked")).toBe(
      "false",
    );
    fireEvent.click(screen.getByRole("switch"));
    expect(await screen.findByText("Saved")).toBeTruthy();
    expect(screen.getByRole("switch").getAttribute("aria-checked")).toBe(
      "true",
    );
    expect(server.sent("PUT", "/api/store/settings")[0].body).toEqual({
      share_installs: true,
    });
    server.routes["PUT /api/store/settings"] = reply(
      502,
      '{"error":"no platform"}',
    );
    fireEvent.click(screen.getByRole("switch"));
    expect(await screen.findByText("no platform")).toBeTruthy();
    await settle();
    expect(screen.getByRole("switch").getAttribute("aria-checked")).toBe(
      "true",
    );
  });
});

describe("HeartButton refusal when giving", () => {
  it("takes the new heart away again", async () => {
    fakeServer({
      "GET /api/store/apps/gitea/heart": { hearted: false },
      "PUT /api/store/apps/gitea/heart": reply(503, '{"error":"busy"}'),
    });
    render(<HeartButton app={official} count={4} />);
    fireEvent.click(
      await screen.findByRole("button", { name: "Give Gitea a heart" }),
    );
    expect(await screen.findByText("busy")).toBeTruthy();
    expect(
      screen.getByRole("button", { name: "Give Gitea a heart" }).textContent,
    ).toBe("4");
  });
});
