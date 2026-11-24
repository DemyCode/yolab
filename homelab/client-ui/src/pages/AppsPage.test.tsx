import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { AppsPage } from "./AppsPage";
import { fakeServer, never } from "@/test/server";
import { renderAt } from "@/test/render";
import { appInfo, catalogApp } from "@/test/fixtures";

const catalog = [
  catalogApp({
    id: "immich",
    name: "Immich",
    collections: ["start-here", "replace-google"],
  }),
  catalogApp({
    id: "jellyfin",
    name: "Jellyfin",
    description: "Films",
    collections: ["start-here"],
  }),
  catalogApp({ id: "zz", name: "Zeta jelly", tagline: "  " }),
  catalogApp({ id: "yy", name: "Yak", description: "Spreads jelly" }),
  catalogApp({ id: "mine", name: "Mine", repo: "custom" }),
  catalogApp({ id: "other", name: "Other", repo: "friends" }),
];

function serve(over: Record<string, unknown> = {}) {
  return fakeServer({
    "GET /api/apps/catalog": catalog,
    "GET /api/apps": [
      appInfo({ app_id: "immich" }),
      appInfo({ app_id: "immich", instance_name: "immich-2" }),
    ],
    "GET /api/store/stats": [
      { app_id: "jellyfin", installs: 5, hearts: 0, comment_count: 0 },
    ],
    "GET /api/apps/repos": never(),
    ...over,
  });
}

const names = (root: HTMLElement | Document = document) =>
  [...root.querySelectorAll("a span.truncate.font-medium")].map(
    (s) => s.textContent,
  );
const search = () => screen.getByLabelText("Search services");
const section = (title: string) =>
  within(screen.getByRole("heading", { name: title }).closest("section")!);

describe("AppsPage", () => {
  it("shows placeholders while the catalog loads", () => {
    serve({ "GET /api/apps/catalog": never() });
    const { container } = renderAt(<AppsPage />);
    expect(container.querySelectorAll(".shimmer")).toHaveLength(7);
    expect(search().getAttribute("placeholder")).toBe("Search services");
  });

  it("says when there is nothing to install", async () => {
    serve({ "GET /api/apps/catalog": [] });
    renderAt(<AppsPage />);
    expect(await screen.findByText("No services yet")).toBeTruthy();
  });

  it("shows the featured services, the collections and every service by group", async () => {
    serve();
    renderAt(<AppsPage />);
    expect(
      await screen.findByRole("heading", { name: "Start here" }),
    ).toBeTruthy();
    expect(search().getAttribute("placeholder")).toBe(
      "Search 6 services, like photos, films or passwords",
    );
    expect(
      screen.getByText(/What most people set up first\. Pick one/),
    ).toBeTruthy();
    expect(screen.getByRole("heading", { name: "Leave Google" })).toBeTruthy();
    expect(screen.queryByRole("heading", { name: "Family" })).toBeNull();
    const all = section("All services");
    expect(
      all.getAllByRole("heading", { level: 3 }).map((h) => h.textContent),
    ).toEqual(["Photos", "Films & TV", "Tools"]);
    expect(all.getByText("2 installed")).toBeTruthy();
    expect(all.queryByRole("button", { name: /Clear/ })).toBeNull();
    fireEvent.click(screen.getByRole("link", { name: /Add from backup/ }));
    expect(screen.getByTestId("where").textContent).toBe("/system/backups");
  });

  it("filters by group with the chips", async () => {
    serve();
    renderAt(<AppsPage />);
    const chip = await screen.findByRole("button", { name: /^Films & TV/ });
    expect(chip.textContent).toBe("Films & TV1");
    fireEvent.click(chip);
    expect(chip.getAttribute("aria-pressed")).toBe("true");
    expect(chip.querySelector("span")!.className).toContain("text-bg/70");
    expect(
      names(section("All services").getByText("Jellyfin").closest("section")!),
    ).toEqual(["Jellyfin"]);
    expect(screen.getByRole("heading", { name: "Start here" })).toBeTruthy();
    fireEvent.click(chip);
    expect(chip.getAttribute("aria-pressed")).toBe("false");
    fireEvent.click(chip);
    fireEvent.click(screen.getByRole("button", { name: /^Everything/ }));
    expect(chip.getAttribute("aria-pressed")).toBe("false");
    fireEvent.click(chip);
    fireEvent.click(screen.getByRole("button", { name: "Clear" }));
    expect(
      screen
        .getByRole("button", { name: /^Everything/ })
        .getAttribute("aria-pressed"),
    ).toBe("true");
  });

  it("searches names first, then other words, hiding the collections", async () => {
    serve();
    renderAt(<AppsPage />);
    await screen.findByRole("heading", { name: "Start here" });
    fireEvent.change(search(), { target: { value: " Jel " } });
    expect(
      screen.getByRole("heading", { name: "Results for “Jel”" }),
    ).toBeTruthy();
    expect(screen.queryByRole("heading", { name: "Start here" })).toBeNull();
    expect(names()).toEqual(["Jellyfin", "Zeta jelly", "Yak"]);
    expect(screen.getByLabelText("Sort")).toHaveProperty("disabled", true);
    fireEvent.change(search(), { target: { value: "y" } });
    expect(names()).toEqual(["Yak", "Jellyfin", "Zeta jelly"]);
    fireEvent.click(screen.getByRole("button", { name: "Clear" }));
    expect((search() as HTMLInputElement).value).toBe("");
  });

  it("says when nothing matches, and clears everything", async () => {
    serve();
    renderAt(<AppsPage />);
    await screen.findByRole("heading", { name: "Start here" });
    fireEvent.change(search(), { target: { value: "nothing like this" } });
    fireEvent.click(
      screen.getByRole("button", { name: "Clear search and filters" }),
    );
    expect(screen.getByRole("heading", { name: "Start here" })).toBeTruthy();
  });

  it("filters by installed or not, and by source", async () => {
    serve();
    renderAt(<AppsPage />);
    await screen.findByRole("heading", { name: "Start here" });
    fireEvent.change(screen.getByLabelText("Installed or not"), {
      target: { value: "installed" },
    });
    expect(names()).toEqual(["Immich"]);
    expect(screen.getByRole("heading", { name: "All services" })).toBeTruthy();
    fireEvent.change(screen.getByLabelText("Installed or not"), {
      target: { value: "not-installed" },
    });
    expect(names()).not.toContain("Immich");
    fireEvent.change(screen.getByLabelText("Installed or not"), {
      target: { value: "any" },
    });
    const source = screen.getByLabelText("Source");
    expect(
      [...source.querySelectorAll("option")].map((o) => o.textContent),
    ).toEqual(["Every source", "Your own", "friends", "YoLab catalog"]);
    fireEvent.change(source, { target: { value: "custom" } });
    expect(names()).toEqual(["Mine"]);
  });

  it("offers no source filter with a single source, and sorts", async () => {
    serve({ "GET /api/apps/catalog": catalog.slice(0, 4) });
    renderAt(<AppsPage />);
    await screen.findByRole("heading", { name: "Start here" });
    expect(screen.queryByLabelText("Source")).toBeNull();
    fireEvent.change(screen.getByLabelText("Sort"), {
      target: { value: "name" },
    });
    expect((screen.getByLabelText("Sort") as HTMLSelectElement).value).toBe(
      "name",
    );
  });

  it("reloads the catalog after its sources are refreshed", async () => {
    const server = serve({
      "GET /api/apps/repos": [],
      "POST /api/apps/repos/sync": { official: { ok: true, charts: 6 } },
    });
    renderAt(<AppsPage />);
    await screen.findByRole("heading", { name: "Start here" });
    fireEvent.click(screen.getByRole("button", { name: "Refresh" }));
    await waitFor(() =>
      expect(server.sent("GET", "/api/apps/catalog")).toHaveLength(2),
    );
  });
});
