import {
  act,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { AppCard, FeaturedAppCard } from "./AppCard";
import { RestoreData } from "./RestoreData";
import { RemoveDataChoice } from "./RemoveDataChoice";
import { NotificationsCard } from "./NotificationsCard";
import { LoginPage } from "@/pages/LoginPage";
import { NO_CHOICES } from "@/lib/restore";
import { fakeServer, never, reply } from "@/test/server";
import { renderAt } from "@/test/render";
import { catalogApp } from "@/test/fixtures";

const stats = (hearts: number) => ({
  app_id: "gitea",
  installs: 0,
  hearts,
  comment_count: 0,
});

describe("AppCard", () => {
  for (const [name, Card] of [
    ["AppCard", AppCard],
    ["FeaturedAppCard", FeaturedAppCard],
  ] as const) {
    it(`${name} links to the install page with its tagline`, () => {
      renderAt(<Card app={catalogApp({ tagline: "Code" })} count={0} />);
      expect(screen.getByText("Code")).toBeTruthy();
      expect(screen.queryByText("Installed")).toBeNull();
      expect(screen.queryByTitle(/heart|stars/)).toBeNull();
      fireEvent.click(screen.getByRole("link"));
      expect(screen.getByTestId("where").textContent).toBe("/add/gitea");
    });

    it(`${name} counts installs, hearts and stars`, () => {
      const { unmount } = renderAt(
        <Card app={catalogApp({ stars: 1500 })} count={1} stats={stats(1)} />,
      );
      expect(screen.getByText("Installed")).toBeTruthy();
      expect(screen.getByTitle("1 YoLab heart")).toBeTruthy();
      expect(screen.getByTitle("1,500 stars on GitHub").textContent).toBe(
        "1.5k",
      );
      unmount();
      renderAt(<Card app={catalogApp()} count={3} stats={stats(2)} />);
      expect(screen.getByText("3 installed")).toBeTruthy();
      expect(screen.getByTitle("2 YoLab hearts")).toBeTruthy();
      expect(screen.queryByTitle(/stars/)).toBeNull();
    });
  }
});

describe("RestoreData", () => {
  const plan = {
    data: [
      { claim: "data", capacity: "1Gi", backed_up: true },
      { claim: "cache", capacity: "1Gi", backed_up: false },
    ],
  };

  it("says why the backup could not be read", () => {
    render(
      <RestoreData
        plan={null}
        error="gone"
        mode="reinstall"
        app="Gitea"
        picked={NO_CHOICES}
        onPick={() => {}}
      />,
    );
    expect(screen.getByText("gone")).toBeTruthy();
  });

  it("waits while the backup is read", () => {
    render(
      <RestoreData
        plan={null}
        error={null}
        mode="reinstall"
        app="Gitea"
        picked={NO_CHOICES}
        onPick={() => {}}
      />,
    );
    expect(screen.getByText("Reading the backup…")).toBeTruthy();
  });

  it("offers a choice per volume, and a plain label when there is only one", () => {
    const onPick = vi.fn();
    render(
      <RestoreData
        plan={plan}
        error={null}
        mode="reinstall"
        app="Gitea"
        picked={{ data: { cache: "empty" } }}
        onPick={onPick}
      />,
    );
    const select = screen.getByLabelText(
      "Gitea's own data",
    ) as HTMLSelectElement;
    expect(select.value).toBe("restore");
    expect(screen.getByText("Not in this backup")).toBeTruthy();
    expect(
      screen.getByText("Start it empty", { selector: "span" }),
    ).toBeTruthy();
    fireEvent.change(select, { target: { value: "empty" } });
    expect(onPick).toHaveBeenCalledWith({
      data: { cache: "empty", data: "empty" },
    });
  });
});

const verdict = (over = {}) => ({ delete: [], shared: [], kept: [], ...over });

describe("RemoveDataChoice", () => {
  it("asks nothing while closed", () => {
    const server = fakeServer({});
    render(
      <RemoveDataChoice
        instance="gitea"
        open={false}
        deleteShared={false}
        onChange={() => {}}
      />,
    );
    expect(server.calls).toHaveLength(0);
  });

  it("looks at the folders, then says there are none", async () => {
    fakeServer({ "GET /api/apps/gitea/data": verdict() });
    render(
      <RemoveDataChoice
        instance="gitea"
        open
        deleteShared={false}
        onChange={() => {}}
      />,
    );
    expect(screen.getByText("Looking at its folders…")).toBeTruthy();
    expect(
      await screen.findByText("It keeps no data in your file tree."),
    ).toBeTruthy();
  });

  it("lists what is deleted, shared and kept, and asks about the shared data", async () => {
    const onChange = vi.fn();
    fakeServer({
      "GET /api/apps/gitea/data": verdict({
        delete: ["gitea", ""],
        shared: [{ folder: "media", used_by: ["jellyfin", "immich"] }],
        kept: [{ folder: "photos", used_by: ["immich"] }],
      }),
    });
    render(
      <RemoveDataChoice
        instance="gitea"
        open
        deleteShared
        onChange={onChange}
      />,
    );
    expect(await screen.findByText("/gitea")).toBeTruthy();
    expect(screen.getByText("/ (the whole file tree)")).toBeTruthy();
    expect(
      screen.getByText(/still read and written by immich, jellyfin/),
    ).toBeTruthy();
    expect(screen.getByText("/media")).toBeTruthy();
    expect(screen.getByText("/photos, used by immich")).toBeTruthy();
    expect(screen.queryByText(/keeps no data/)).toBeNull();
    const [yes, no] = screen.getAllByRole("radio") as HTMLInputElement[];
    expect(yes.checked).toBe(true);
    fireEvent.click(no);
    expect(onChange).toHaveBeenCalledWith(false);
    fireEvent.click(yes);
    expect(onChange).toHaveBeenCalledTimes(1);
  });

  it("says the folders could not be read", async () => {
    fakeServer({
      "GET /api/apps/a%20b/data": reply(500, '{"error":"no tree"}'),
    });
    render(
      <RemoveDataChoice
        instance="a b"
        open
        deleteShared={false}
        onChange={() => {}}
      />,
    );
    expect(
      await screen.findByText(/Its folders could not be read: no tree/),
    ).toBeTruthy();
  });
});

const subscription = {
  topic: "t",
  subscribe_url: "ntfy://n.example/t",
  web_url: "https://n.example/t",
};

describe("NotificationsCard", () => {
  it("shows nothing more until it knows", () => {
    fakeServer({ "GET /api/notifications": never() });
    render(<NotificationsCard />);
    expect(screen.getByText("Phone notifications")).toBeTruthy();
    expect(screen.queryByRole("img")).toBeNull();
  });

  it("says why notifications are not available", async () => {
    fakeServer({
      "GET /api/notifications": { available: false, reason: "No relay" },
    });
    render(<NotificationsCard />);
    expect(await screen.findByText("No relay")).toBeTruthy();
  });

  it("shows a QR code and links to subscribe", async () => {
    fakeServer({ "GET /api/notifications": { available: true, subscription } });
    render(<NotificationsCard />);
    const qr = await screen.findByRole("img", { name: /QR code/ });
    expect(qr.querySelector("path")!.getAttribute("d")).toMatch(
      /^M\d+ \d+h1v1h-1z/,
    );
    expect(
      screen.getByRole("link", { name: "the link" }).getAttribute("href"),
    ).toBe(subscription.subscribe_url);
    expect(
      screen.getByRole("link", { name: subscription.web_url }),
    ).toBeTruthy();
  });

  it("sends a test and says how it went", async () => {
    const server = fakeServer({
      "GET /api/notifications": { available: true, subscription },
      "POST /api/notifications/test": undefined,
    });
    render(<NotificationsCard />);
    fireEvent.click(
      await screen.findByRole("button", { name: "Send a test notification" }),
    );
    expect(await screen.findByText(/^Sent\./)).toBeTruthy();
    expect(server.sent("POST", "/api/notifications/test")).toHaveLength(1);
    server.routes["POST /api/notifications/test"] = reply(
      502,
      '{"error":"relay down"}',
    );
    fireEvent.click(
      screen.getByRole("button", { name: "Send a test notification" }),
    );
    expect(await screen.findByText("relay down")).toBeTruthy();
    server.routes["POST /api/notifications/test"] = () => Promise.reject("x");
    fireEvent.click(
      screen.getByRole("button", { name: "Send a test notification" }),
    );
    expect(await screen.findByText("Could not send it")).toBeTruthy();
  });
});

describe("LoginPage", () => {
  function signIn(password: string) {
    fireEvent.change(screen.getByLabelText("Password"), {
      target: { value: password },
    });
    fireEvent.click(screen.getByRole("button", { name: "Sign in" }));
  }

  it("cannot be sent empty", () => {
    render(<LoginPage onLogin={() => {}} />);
    expect(screen.getByRole("button", { name: "Sign in" })).toHaveProperty(
      "disabled",
      true,
    );
  });

  it("signs in with the password", async () => {
    const onLogin = vi.fn();
    const server = fakeServer({ "POST /api/login": undefined });
    render(<LoginPage onLogin={onLogin} />);
    signIn("hunter2");
    await waitFor(() => expect(onLogin).toHaveBeenCalledOnce());
    expect(server.sent("POST", "/api/login")[0].body).toEqual({
      password: "hunter2",
    });
  });

  it("says when the password is wrong, or why it could not sign in", async () => {
    const onLogin = vi.fn();
    const server = fakeServer({ "POST /api/login": reply(401) });
    render(<LoginPage onLogin={onLogin} />);
    signIn("bad");
    expect(
      await screen.findByText("That password does not match."),
    ).toBeTruthy();
    server.routes["POST /api/login"] = reply(503, '{"error":"starting up"}');
    signIn("bad");
    expect(await screen.findByText("starting up")).toBeTruthy();
    server.routes["POST /api/login"] = () => Promise.reject("x");
    signIn("bad");
    expect(await screen.findByText("Could not sign in.")).toBeTruthy();
    await act(async () => {});
    expect(onLogin).not.toHaveBeenCalled();
  });
});
