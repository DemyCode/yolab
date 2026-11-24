import {
  act,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import {
  CopyButton,
  DisclosureRow,
  IconButton,
  Row,
  RowAction,
  Section,
  ValueRow,
} from "./ui/list";
import { ScheduleRow, SpaceRow } from "./BackupPolicyRows";
import { SshKeysCard } from "./SshKeysCard";
import { AppTile, AppTileSkeleton } from "./AppTile";
import { PackMembers } from "./PackMembers";
import { SettingsPage } from "@/pages/system/SettingsPage";
import { fakeServer, never, reply } from "@/test/server";
import { renderAt } from "@/test/render";
import { appInfo, catalogApp } from "@/test/fixtures";
import type { AppInfo } from "@/types/apps";

function clipboard(writeText: (t: string) => Promise<void>) {
  Object.defineProperty(navigator, "clipboard", {
    configurable: true,
    value: { writeText },
  });
}

describe("list parts", () => {
  it("a section has a title, an action and a card of rows", () => {
    render(
      <Section title="Things" action={<button>Add</button>}>
        <Row label="One" detail="first" />
      </Section>,
    );
    expect(screen.getByRole("heading", { name: "Things" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Add" })).toBeTruthy();
    expect(screen.getByText("first")).toBeTruthy();
  });

  it("a clickable row shows a chevron unless it has its own trailing part", () => {
    const onClick = vi.fn();
    const { rerender } = render(<Row label="Go" onClick={onClick} danger />);
    const b = screen.getByRole("button");
    expect(b.querySelector("svg")).not.toBeNull();
    expect(screen.getByText("Go").className).toContain("text-danger");
    fireEvent.click(b);
    expect(onClick).toHaveBeenCalledOnce();
    rerender(<Row label="Go" onClick={onClick} trailing={<b>t</b>} disabled />);
    expect(screen.getByRole("button").querySelector("svg")).toBeNull();
    expect(screen.getByRole("button")).toHaveProperty("disabled", true);
  });

  it("a plain row shows its children below and no chevron", () => {
    const { container } = render(
      <Row label="Plain" trailing={<i>t</i>}>
        <p>below</p>
      </Row>,
    );
    expect(screen.getByText("Plain").className).toContain("text-fg");
    expect(screen.getByText("below")).toBeTruthy();
    expect(container.querySelector("svg")).toBeNull();
  });

  it("a disclosure row opens and closes", () => {
    render(
      <DisclosureRow label="More" detail="details">
        <p>hidden</p>
      </DisclosureRow>,
    );
    const b = screen.getByRole("button");
    expect(b.getAttribute("aria-expanded")).toBe("false");
    expect(screen.queryByText("hidden")).toBeNull();
    fireEvent.click(b);
    expect(b.getAttribute("aria-expanded")).toBe("true");
    expect(screen.getByText("hidden")).toBeTruthy();
    expect(b.querySelector("svg")!.getAttribute("class")).toContain(
      "rotate-180",
    );
    fireEvent.click(b);
    expect(screen.queryByText("hidden")).toBeNull();
  });

  it("a disclosure row without detail shows only its label", () => {
    const { container } = render(
      <DisclosureRow label="More">
        <p>x</p>
      </DisclosureRow>,
    );
    expect(container.querySelectorAll(".mt-0\\.5")).toHaveLength(0);
  });

  it("a row action can be dangerous or disabled", () => {
    const onClick = vi.fn();
    const { rerender } = render(<RowAction onClick={onClick}>Do</RowAction>);
    expect(screen.getByRole("button").className).toContain("text-primary");
    fireEvent.click(screen.getByRole("button"));
    expect(onClick).toHaveBeenCalledOnce();
    rerender(
      <RowAction onClick={onClick} danger disabled>
        Do
      </RowAction>,
    );
    expect(screen.getByRole("button").className).toContain("text-danger");
    expect(screen.getByRole("button")).toHaveProperty("disabled", true);
  });

  it("an icon button is a link when it has an address", () => {
    const onClick = vi.fn();
    const { rerender } = render(
      <IconButton label="Open" href="https://x">
        i
      </IconButton>,
    );
    const a = screen.getByRole("link", { name: "Open" });
    expect(a.getAttribute("href")).toBe("https://x");
    expect(a.getAttribute("target")).toBe("_blank");
    rerender(
      <IconButton label="Open" onClick={onClick}>
        i
      </IconButton>,
    );
    fireEvent.click(screen.getByRole("button", { name: "Open" }));
    expect(onClick).toHaveBeenCalledOnce();
  });

  it("copies a value and says so for a moment", async () => {
    vi.useFakeTimers();
    const writeText = vi.fn(async () => {});
    clipboard(writeText);
    render(<CopyButton value="v" label="address" />);
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Copy address" }));
    });
    expect(writeText).toHaveBeenCalledWith("v");
    expect(screen.getByRole("button", { name: "address copied" })).toBeTruthy();
    act(() => vi.advanceTimersByTime(1600));
    expect(screen.getByRole("button", { name: "Copy address" })).toBeTruthy();
  });

  it("a refused copy changes nothing", async () => {
    clipboard(() => Promise.reject(new Error("no")));
    render(<CopyButton value="v" label="address" />);
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Copy address" }));
    });
    expect(screen.getByRole("button", { name: "Copy address" })).toBeTruthy();
  });

  it("a value row shows its value with an optional copy button", () => {
    const { rerender } = render(
      <ValueRow label="Key" value="abc" copy trailing={<i>t</i>} />,
    );
    expect(screen.getByTitle("abc").textContent).toBe("abc");
    expect(screen.getByRole("button", { name: "Copy Key" })).toBeTruthy();
    expect(screen.getByText("t")).toBeTruthy();
    rerender(<ValueRow label="Key" value="abc" />);
    expect(screen.queryByRole("button")).toBeNull();
  });
});

describe("ScheduleRow", () => {
  it("saves a preset straight away", () => {
    const onSave = vi.fn();
    render(<ScheduleRow schedule="0 3 * * *" busy={false} onSave={onSave} />);
    fireEvent.change(screen.getByLabelText("How often"), {
      target: { value: "0 */6 * * *" },
    });
    expect(onSave).toHaveBeenCalledWith("0 */6 * * *");
    expect(screen.queryByLabelText("Custom schedule")).toBeNull();
  });

  it("custom opens an editor seeded with the current schedule", () => {
    const onSave = vi.fn();
    render(<ScheduleRow schedule="0 3 * * *" busy={false} onSave={onSave} />);
    fireEvent.change(screen.getByLabelText("How often"), {
      target: { value: "__custom" },
    });
    const input = screen.getByLabelText("Custom schedule") as HTMLInputElement;
    expect(input.value).toBe("0 3 * * *");
    const save = screen.getByRole("button", { name: "Save" });
    expect(save).toHaveProperty("disabled", true);
    fireEvent.change(input, { target: { value: "  " } });
    expect(save).toHaveProperty("disabled", true);
    fireEvent.change(input, { target: { value: " 5 4 * * * " } });
    fireEvent.click(save);
    expect(onSave).toHaveBeenCalledWith("5 4 * * *");
    fireEvent.change(screen.getByLabelText("How often"), {
      target: { value: "0 3 * * 0" },
    });
    expect(screen.queryByLabelText("Custom schedule")).toBeNull();
  });

  it("a schedule that is no preset opens as custom, and waits while busy", () => {
    render(
      <ScheduleRow schedule="1 2 3 4 5" detail="why" busy onSave={() => {}} />,
    );
    expect(
      (screen.getByLabelText("How often") as HTMLSelectElement).value,
    ).toBe("__custom");
    expect(screen.getByLabelText("How often")).toHaveProperty("disabled", true);
    expect(screen.getByText("why")).toBeTruthy();
  });
});

describe("SpaceRow", () => {
  it("saves a new size in GB only when it is a real change", () => {
    const onSave = vi.fn();
    render(
      <SpaceRow
        label="Space"
        bytes={10_000_000_000}
        detail="d"
        busy={false}
        onSave={onSave}
        action={<i>extra</i>}
      />,
    );
    const input = screen.getByLabelText("Space in GB") as HTMLInputElement;
    const save = screen.getByRole("button", { name: "Save" });
    expect(input.value).toBe("10");
    expect(save).toHaveProperty("disabled", true);
    fireEvent.change(input, { target: { value: "abc" } });
    expect(save).toHaveProperty("disabled", true);
    fireEvent.click(save);
    expect(onSave).not.toHaveBeenCalled();
    fireEvent.change(input, { target: { value: "2,5" } });
    fireEvent.click(save);
    expect(onSave).toHaveBeenCalledWith(2_500_000_000);
    expect(screen.getByText("extra")).toBeTruthy();
  });
});

const keys = {
  user: "root",
  port: 2222,
  keys: [
    {
      id: "k1",
      kind: "ssh-ed25519",
      fingerprint: "SHA256:aa",
      comment: "me@laptop",
      label: "",
    },
  ],
};

describe("SshKeysCard", () => {
  it("lists keys with the command to connect", async () => {
    fakeServer({ "GET /api/ssh/keys": keys });
    render(<SshKeysCard />);
    await screen.findByText("me@laptop");
    expect(screen.getByText("SHA256:aa")).toBeTruthy();
    expect(screen.getByText(/^ssh -p 2222 root@/)).toBeTruthy();
  });

  it("shows nothing listed when there are no keys", async () => {
    const server = fakeServer({ "GET /api/ssh/keys": { ...keys, keys: [] } });
    render(<SshKeysCard />);
    await waitFor(() => expect(server.calls).toHaveLength(1));
    await act(async () => {});
    expect(screen.queryByRole("list")).toBeNull();
  });

  it("says why the keys could not be read", async () => {
    fakeServer({ "GET /api/ssh/keys": reply(500, '{"error":"no store"}') });
    render(<SshKeysCard />);
    expect(await screen.findByText("no store")).toBeTruthy();
  });

  it("adds a key with its name and clears the form", async () => {
    const server = fakeServer({
      "GET /api/ssh/keys": { ...keys, keys: [] },
      "POST /api/ssh/keys": keys,
    });
    render(<SshKeysCard />);
    await waitFor(() => expect(server.calls).toHaveLength(1));
    await act(async () => {});
    const add = screen.getByRole("button", { name: "Add key" });
    expect(add).toHaveProperty("disabled", true);
    fireEvent.change(screen.getByLabelText("Public key"), {
      target: { value: "ssh-ed25519 AAAA me@laptop" },
    });
    fireEvent.change(screen.getByLabelText("Key name"), {
      target: { value: "Work" },
    });
    fireEvent.click(add);
    await screen.findByText("me@laptop");
    expect(server.sent("POST", "/api/ssh/keys")[0].body).toEqual({
      key: "ssh-ed25519 AAAA me@laptop",
      label: "Work",
    });
    expect(
      (screen.getByLabelText("Public key") as HTMLTextAreaElement).value,
    ).toBe("");
    expect((screen.getByLabelText("Key name") as HTMLInputElement).value).toBe(
      "",
    );
  });

  it("refuses a private key", () => {
    fakeServer({ "GET /api/ssh/keys": never() });
    render(<SshKeysCard />);
    fireEvent.change(screen.getByLabelText("Public key"), {
      target: { value: "-----BEGIN OPENSSH PRIVATE KEY-----" },
    });
    expect(screen.getByText(/This is a private key/)).toBeTruthy();
    expect(screen.getByRole("button", { name: "Add key" })).toHaveProperty(
      "disabled",
      true,
    );
  });

  it("says why a key was not added", async () => {
    fakeServer({
      "GET /api/ssh/keys": never(),
      "POST /api/ssh/keys": reply(400, '{"error":"not a key"}'),
    });
    render(<SshKeysCard />);
    fireEvent.change(screen.getByLabelText("Public key"), {
      target: { value: "x" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Add key" }));
    expect(await screen.findByText("not a key")).toBeTruthy();
  });

  it("a failure that is not an error still says the key was not added", async () => {
    fakeServer({
      "GET /api/ssh/keys": never(),
      "POST /api/ssh/keys": () => Promise.reject("x"),
    });
    render(<SshKeysCard />);
    fireEvent.change(screen.getByLabelText("Public key"), {
      target: { value: "x" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Add key" }));
    expect(await screen.findByText("Could not add the key")).toBeTruthy();
  });

  it("removes a key", async () => {
    const server = fakeServer({
      "GET /api/ssh/keys": keys,
      "POST /api/ssh/keys/remove": { ...keys, keys: [] },
    });
    render(<SshKeysCard />);
    fireEvent.click(
      await screen.findByRole("button", { name: "Remove me@laptop" }),
    );
    await waitFor(() => expect(screen.queryByText("me@laptop")).toBeNull());
    expect(server.sent("POST", "/api/ssh/keys/remove")[0].body).toEqual({
      id: "k1",
    });
  });

  it("says why a key was not removed", async () => {
    fakeServer({
      "GET /api/ssh/keys": keys,
      "POST /api/ssh/keys/remove": reply(500, '{"error":"locked"}'),
    });
    render(<SshKeysCard />);
    fireEvent.click(
      await screen.findByRole("button", { name: "Remove me@laptop" }),
    );
    expect(await screen.findByText("locked")).toBeTruthy();
  });

  it("a failure that is not an error still says the key was not removed", async () => {
    fakeServer({
      "GET /api/ssh/keys": keys,
      "POST /api/ssh/keys/remove": () => Promise.reject("x"),
    });
    render(<SshKeysCard />);
    fireEvent.click(
      await screen.findByRole("button", { name: "Remove me@laptop" }),
    );
    expect(await screen.findByText("Could not remove the key")).toBeTruthy();
  });
});

describe("SettingsPage", () => {
  const settings = {
    backup_schedule: "0 3 * * *",
    backup_budget_bytes: 10_000_000_000,
  };

  it("waits, then shows the defaults for new services", async () => {
    fakeServer({ "GET /api/settings": settings, "GET /api/ssh/keys": never() });
    render(<SettingsPage />);
    expect(screen.getByLabelText("Loading")).toBeTruthy();
    expect(await screen.findByText("Backups of new services")).toBeTruthy();
    expect(
      (screen.getByLabelText("Space in GB") as HTMLInputElement).value,
    ).toBe("10");
  });

  it("says why the settings could not be read", async () => {
    fakeServer({
      "GET /api/settings": reply(500, '{"error":"no quorum"}'),
      "GET /api/ssh/keys": never(),
    });
    render(<SettingsPage />);
    expect(
      await screen.findByText("The backup settings could not be read"),
    ).toBeTruthy();
    expect(screen.getByText("no quorum")).toBeTruthy();
  });

  it("saves a change, says so, and rereads", async () => {
    const server = fakeServer({
      "GET /api/settings": settings,
      "PUT /api/settings": settings,
      "GET /api/ssh/keys": never(),
    });
    render(<SettingsPage />);
    fireEvent.change(await screen.findByLabelText("How often"), {
      target: { value: "0 */6 * * *" },
    });
    expect(await screen.findByText("Saved")).toBeTruthy();
    expect(screen.getByText("Saved").className).toContain("text-success");
    expect(server.sent("PUT", "/api/settings")[0].body).toEqual({
      ...settings,
      backup_schedule: "0 */6 * * *",
    });
    await waitFor(() =>
      expect(server.sent("GET", "/api/settings")).toHaveLength(2),
    );
  });

  it("says why a change was not saved", async () => {
    fakeServer({
      "GET /api/settings": settings,
      "PUT /api/settings": reply(409, '{"error":"busy"}'),
      "GET /api/ssh/keys": never(),
    });
    render(<SettingsPage />);
    const input = await screen.findByLabelText("Space in GB");
    fireEvent.change(input, { target: { value: "20" } });
    fireEvent.click(screen.getAllByRole("button", { name: "Save" })[0]);
    expect(await screen.findByText("busy")).toBeTruthy();
    expect(screen.getByText("busy").className).toContain("text-danger");
  });
});

describe("AppTile", () => {
  const tone = (status: AppInfo["status"], over = {}) => {
    const { container, unmount } = renderAt(
      <AppTile app={appInfo({ status, ...over })} name="Gitea" icon="" />,
    );
    const dot = container.querySelector("a span.rounded-full")!;
    const link = container.querySelector("a")!;
    const out = {
      dot: dot.className,
      link: link.className,
      text: link.textContent,
    };
    unmount();
    return out;
  };

  it("links to the app and colours its dot by state", () => {
    expect(tone("failed").dot).toContain("bg-danger");
    expect(tone("stopped", { reason: "Crashed" }).dot).toContain("bg-danger");
    expect(tone("uninstalling").dot).toContain("bg-warning");
    expect(tone("uninstalling").link).toContain("opacity-60");
    expect(tone("starting").dot).toContain("animate-pulse");
    expect(tone("copying").dot).toContain("bg-primary");
    expect(tone("running").dot).toContain("bg-success");
    expect(tone("running").text).toBe("Gitea");
    expect(tone("starting").text).toBe("GiteaStarting up…");
  });

  it("opens the app's page", () => {
    renderAt(
      <AppTile
        app={appInfo({ instance_name: "gitea-2" })}
        name="Gitea"
        icon=""
        appId="x"
      />,
    );
    fireEvent.click(screen.getByRole("link"));
    expect(screen.getByTestId("where").textContent).toBe("/app/gitea-2");
  });

  it("a skeleton has a plate and a name bar", () => {
    const { container } = render(<AppTileSkeleton />);
    expect(container.querySelectorAll(".shimmer")).toHaveLength(2);
  });
});

describe("PackMembers", () => {
  it("shows nothing for a pack without members", () => {
    const { container } = renderAt(<PackMembers members={[]} />);
    expect(container.querySelector("section")).toBeNull();
  });

  it("lists each member with its source and website", () => {
    renderAt(
      <PackMembers
        members={[
          catalogApp({
            id: "sonarr",
            name: "Sonarr",
            tagline: "TV",
            github: "Sonarr/Sonarr",
            stars: 12000,
            home: "https://sonarr.tv",
          }),
          catalogApp({
            id: "radarr",
            name: "Radarr",
            github: "Radarr/Radarr",
            stars: 0,
            home: "https://github.com/Radarr/Radarr",
          }),
          catalogApp({ id: "x", name: "X", github: "", stars: null, home: "" }),
        ]}
      />,
    );
    expect(screen.getByText("TV")).toBeTruthy();
    expect(screen.getByText("12k")).toBeTruthy();
    expect(
      screen.getByTitle("Sonarr/Sonarr on GitHub").getAttribute("href"),
    ).toBe("https://github.com/Sonarr/Sonarr");
    expect(screen.getByLabelText("Sonarr website").getAttribute("href")).toBe(
      "https://sonarr.tv",
    );
    expect(screen.getByTitle("Radarr/Radarr on GitHub").textContent).toBe(
      "Source",
    );
    expect(screen.queryByLabelText("Radarr website")).toBeNull();
    expect(screen.queryByTitle(/X on GitHub/)).toBeNull();
    fireEvent.click(screen.getByText("Sonarr"));
    expect(screen.getByTestId("where").textContent).toBe("/add/sonarr");
  });

  it("a member without stars counted shows Source", () => {
    renderAt(
      <PackMembers members={[catalogApp({ github: "a/b", stars: null })]} />,
    );
    expect(screen.getByTitle("a/b on GitHub").textContent).toBe("Source");
  });
});
