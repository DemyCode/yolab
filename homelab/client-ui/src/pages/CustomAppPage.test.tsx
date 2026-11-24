import { fireEvent, screen, waitFor } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import CustomAppPage from "./CustomAppPage";
import { fakeServer, reply } from "@/test/server";
import { renderAt } from "@/test/render";

const where = () => screen.getByTestId("where").textContent;
const field = (label: string) =>
  screen
    .getByText(label, { selector: "span" })
    .parentElement!.querySelector("input")!;

function choose(file: File | null) {
  const input = document.querySelector(
    'input[type="file"]',
  ) as HTMLInputElement;
  Object.defineProperty(input, "files", {
    configurable: true,
    value: file ? [file] : [],
  });
  fireEvent.change(input);
}

const chart = new File(["x"], "chart.tgz");

describe("CustomAppPage, uploading a chart", () => {
  it("adds an uploaded chart with its own settings, and sets it up", async () => {
    let listed: unknown[] = [];
    const server = fakeServer({
      "GET /api/apps/custom": () => listed,
      "POST /api/apps/custom/chart": () => {
        listed = [
          { id: "thing", display_name: "Thing", icon: "", description: "" },
        ];
        return { app: { id: "thing", display_name: "Thing" }, has_form: true };
      },
    });
    renderAt(<CustomAppPage />);
    choose(chart);
    expect(screen.getByText("Checking…", { selector: "span" })).toBeTruthy();
    expect(
      await screen.findByText(
        "Thing is in your catalog, with its own settings.",
      ),
    ).toBeTruthy();
    expect(await screen.findByText("Your own services")).toBeTruthy();
    expect(screen.getByText("🔧", { selector: "span.text-lg" })).toBeTruthy();
    expect(screen.getByText("thing", { selector: "p" })).toBeTruthy();
    expect(server.sent("POST", "/api/apps/custom/chart")[0].body).toBe(chart);
    fireEvent.click(screen.getByRole("button", { name: "Set it up" }));
    expect(where()).toBe("/add/thing");
  });

  it("adds a chart without settings", async () => {
    fakeServer({
      "GET /api/apps/custom": [],
      "POST /api/apps/custom/chart": { app: { id: "t", display_name: "T" } },
    });
    renderAt(<CustomAppPage />);
    choose(chart);
    expect(await screen.findByText("T is in your catalog.")).toBeTruthy();
  });

  it("does nothing when no file was chosen", () => {
    const server = fakeServer({ "GET /api/apps/custom": [] });
    renderAt(<CustomAppPage />);
    choose(null);
    expect(server.sent("POST", "/api/apps/custom/chart")).toHaveLength(0);
  });

  it("says why a chart was not added", async () => {
    const server = fakeServer({
      "GET /api/apps/custom": [],
      "POST /api/apps/custom/chart": {},
    });
    renderAt(<CustomAppPage />);
    choose(chart);
    expect(
      await screen.findByText("The server did not return the uploaded chart"),
    ).toBeTruthy();
    server.routes["POST /api/apps/custom/chart"] = () => Promise.reject("x");
    choose(chart);
    expect(await screen.findByText("Could not read that chart")).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Set it up" })).toBeNull();
  });
});

describe("CustomAppPage, pasting YAML", () => {
  it("needs a name, an id and YAML, deriving the id from the name", () => {
    fakeServer({ "GET /api/apps/custom": [] });
    renderAt(<CustomAppPage />);
    const add = screen.getByRole("button", { name: "Add to catalog" });
    expect(add).toHaveProperty("disabled", true);
    fireEvent.change(field("Name"), { target: { value: "  My Cool Thing! " } });
    expect(field("Short id").value).toBe("my-cool-thing");
    fireEvent.change(field("Name"), { target: { value: "Renamed" } });
    expect(field("Short id").value).toBe("my-cool-thing");
    fireEvent.click(screen.getByRole("button", { name: "Use an example" }));
    expect(
      (screen.getByLabelText("Kubernetes YAML") as HTMLTextAreaElement).value,
    ).toContain("kind: Service");
    expect(add).toHaveProperty("disabled", false);
  });

  it("adds the YAML with its port and service, and offers to install it", async () => {
    const server = fakeServer({
      "GET /api/apps/custom": [],
      "POST /api/apps/custom": { documents: 2 },
    });
    renderAt(<CustomAppPage />);
    fireEvent.change(field("Short id"), { target: { value: " hello " } });
    fireEvent.change(field("Name"), { target: { value: " Hello " } });
    fireEvent.change(field("Icon"), { target: { value: " 👋 " } });
    fireEvent.change(field("Web port"), { target: { value: " 8080 " } });
    fireEvent.change(field("Service name"), { target: { value: " web " } });
    fireEvent.change(field("What it is"), { target: { value: " Says hi " } });
    fireEvent.change(screen.getByLabelText("Kubernetes YAML"), {
      target: { value: "kind: X" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Add to catalog" }));
    expect(
      await screen.findByText("Added. 2 objects — it is in the catalog now."),
    ).toBeTruthy();
    expect(server.sent("POST", "/api/apps/custom")[0].body).toEqual({
      id: "hello",
      display_name: "Hello",
      icon: "👋",
      description: "Says hi",
      yaml: "kind: X",
      port: 8080,
      service: "web",
    });
    fireEvent.click(screen.getByRole("button", { name: "Install it" }));
    expect(where()).toBe("/add/ hello ");
  });

  it("adds YAML without a web port", async () => {
    const server = fakeServer({
      "GET /api/apps/custom": [],
      "POST /api/apps/custom": { documents: 1 },
    });
    renderAt(<CustomAppPage />);
    fireEvent.change(field("Name"), { target: { value: "a" } });
    fireEvent.change(screen.getByLabelText("Kubernetes YAML"), {
      target: { value: "x" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Add to catalog" }));
    expect(
      await screen.findByText("Added. 1 object — it is in the catalog now."),
    ).toBeTruthy();
    expect(server.sent("POST", "/api/apps/custom")[0].body).not.toHaveProperty(
      "port",
    );
  });

  it("says why the YAML was refused", async () => {
    const server = fakeServer({
      "GET /api/apps/custom": [],
      "POST /api/apps/custom": reply(400, '{"error":"line 3 reaches outside"}'),
    });
    renderAt(<CustomAppPage />);
    fireEvent.change(field("Name"), { target: { value: "a" } });
    fireEvent.change(screen.getByLabelText("Kubernetes YAML"), {
      target: { value: "x" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Add to catalog" }));
    expect(await screen.findByText("line 3 reaches outside")).toBeTruthy();
    server.routes["POST /api/apps/custom"] = () => Promise.reject("x");
    fireEvent.click(screen.getByRole("button", { name: "Add to catalog" }));
    expect(await screen.findByText("Could not add that")).toBeTruthy();
  });
});

describe("CustomAppPage, your own services", () => {
  const mine = [
    { id: "a", display_name: "Alpha", icon: "🅰", description: "First" },
  ];

  it("installs or removes one of your own services", async () => {
    let listed: unknown[] = mine;
    const server = fakeServer({
      "GET /api/apps/custom": () => listed,
      "DELETE /api/apps/custom/a": () => {
        listed = [];
        return undefined;
      },
    });
    renderAt(<CustomAppPage />);
    expect(await screen.findByText("First")).toBeTruthy();
    expect(screen.getByText("🅰")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Remove Alpha" }));
    await waitFor(() =>
      expect(screen.queryByText("Your own services")).toBeNull(),
    );
    expect(server.sent("DELETE", "/api/apps/custom/a")).toHaveLength(1);
  });

  it("opens the install page of one of your own services", async () => {
    fakeServer({ "GET /api/apps/custom": mine });
    renderAt(<CustomAppPage />);
    fireEvent.click(await screen.findByRole("button", { name: "Install" }));
    expect(where()).toBe("/add/a");
  });

  it("says why it was not removed", async () => {
    const server = fakeServer({
      "GET /api/apps/custom": mine,
      "DELETE /api/apps/custom/a": reply(409, '{"error":"installed"}'),
    });
    renderAt(<CustomAppPage />);
    fireEvent.click(
      await screen.findByRole("button", { name: "Remove Alpha" }),
    );
    expect(await screen.findByText("installed")).toBeTruthy();
    server.routes["DELETE /api/apps/custom/a"] = () => Promise.reject("x");
    await waitFor(() =>
      expect(
        screen.getByRole("button", { name: "Remove Alpha" }),
      ).toHaveProperty("disabled", false),
    );
    fireEvent.click(screen.getByRole("button", { name: "Remove Alpha" }));
    expect(await screen.findByText("Could not remove that")).toBeTruthy();
  });
});

describe("CustomAppPage, the YAML box", () => {
  it("clicking its heading does not paste the example", () => {
    fakeServer({ "GET /api/apps/custom": [] });
    renderAt(<CustomAppPage />);
    fireEvent.click(screen.getByText("Kubernetes YAML"));
    expect(
      (screen.getByLabelText("Kubernetes YAML") as HTMLTextAreaElement).value,
    ).toBe("");
  });
});
