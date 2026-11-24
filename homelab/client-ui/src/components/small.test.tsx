import {
  act,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { GitHubMark } from "./GitHubMark";
import { Logo, Wordmark } from "./Logo";
import { ThemeControl } from "./ThemeControl";
import { AppIcon, AppIconTile } from "./AppIcon";
import { RecoveryKeyGate } from "./RecoveryKeyGate";
import { RecoveryKeyOverlay } from "./RecoveryKeyOverlay";
import { Badge, StatusDot } from "./ui/badge";
import { Button } from "./ui/button";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "./ui/card";
import { SystemSubPage } from "@/pages/system/SystemSubPage";
import { fakeServer, reply } from "@/test/server";
import { renderAt } from "@/test/render";

describe("GitHubMark", () => {
  it("is a decorative mark that takes a class", () => {
    const { container } = render(<GitHubMark className="h-4" />);
    const svg = container.querySelector("svg")!;
    expect(svg.getAttribute("class")).toBe("h-4");
    expect(svg.getAttribute("aria-hidden")).toBe("true");
  });
});

describe("Logo", () => {
  it("shows the light and dark symbol without a name", () => {
    const { container } = render(<Logo className="h-16" />);
    const imgs = [...container.querySelectorAll("img")];
    expect(imgs.map((i) => i.getAttribute("src"))).toEqual([
      "/brand/yolab-symbol-color.svg",
      "/brand/yolab-symbol-reversed.svg",
    ]);
    expect(
      imgs.every((i) => i.alt === "" && i.className.includes("h-16")),
    ).toBe(true);
    expect(imgs[0].className).toContain("dark:hidden");
    expect(imgs[1].className).toContain("dark:block");
  });

  it("the wordmark is named YoLab", () => {
    render(<Wordmark />);
    expect(screen.getAllByAltText("YoLab")).toHaveLength(2);
    expect(screen.getAllByAltText("YoLab")[0].className).toContain("h-9");
  });
});

describe("ThemeControl", () => {
  it("marks the current choice and switches on click", () => {
    render(<ThemeControl />);
    const auto = screen.getByRole("radio", { name: "Automatic" });
    expect(auto.getAttribute("aria-checked")).toBe("true");
    expect(screen.getByText("Light")).toBeTruthy();
    fireEvent.click(screen.getByRole("radio", { name: "Dark" }));
    expect(
      screen.getByRole("radio", { name: "Dark" }).getAttribute("aria-checked"),
    ).toBe("true");
    expect(auto.getAttribute("aria-checked")).toBe("false");
    expect(localStorage.getItem("yolab.theme")).toBe("dark");
    expect(screen.getByRole("radio", { name: "Dark" }).title).toBe("");
  });

  it("compact shows icons with their names as titles", () => {
    render(<ThemeControl compact className="x" />);
    expect(screen.queryByText("Light")).toBeNull();
    expect(screen.getByRole("radio", { name: "Light" }).title).toBe("Light");
    expect(screen.getByRole("radiogroup").className).toContain("x");
  });
});

describe("AppIcon", () => {
  it("prefers the bundled icon, and falls back to the initial when it fails", () => {
    const { container } = render(
      <AppIcon appId="gitea" name="Gitea" icon="🍵" />,
    );
    const img = container.querySelector("img")!;
    expect(img.getAttribute("src")).toBe("/icons/gitea.svg");
    fireEvent.error(img);
    expect(container.textContent).toBe("🍵");
  });

  it("uses a remote or local icon link", () => {
    for (const icon of ["https://x/i.png", "//x/i.png", "/i.png"]) {
      const { container, unmount } = render(<AppIcon name="X" icon={icon} />);
      expect(container.querySelector("img")?.getAttribute("src")).toBe(icon);
      unmount();
    }
  });

  it("shows an emoji icon as text", () => {
    const { container } = render(<AppIcon name="X" icon="📦" />);
    expect(container.textContent).toBe("📦");
  });

  it("shows the first letter when there is no icon, or a question mark", () => {
    const { container, rerender } = render(<AppIcon name="gitea" />);
    expect(container.textContent).toBe("G");
    rerender(<AppIcon name="" />);
    expect(container.textContent).toBe("?");
  });

  it("a failed remote icon falls back to the letter", () => {
    const { container } = render(<AppIcon name="mine" icon="/i.png" />);
    fireEvent.error(container.querySelector("img")!);
    expect(container.textContent).toBe("M");
  });

  it("the tile sizes its plate and glyph", () => {
    for (const [size, box] of [
      ["sm", "h-12"],
      ["md", "h-16"],
      ["lg", "h-16"],
    ] as const) {
      const { container, unmount } = render(
        <AppIconTile name="A" size={size} className="extra" />,
      );
      const plate = container.firstElementChild!;
      expect(plate.className).toContain(box);
      expect(plate.className).toContain("extra");
      unmount();
    }
    const { container } = render(<AppIconTile name="A" />);
    expect(container.firstElementChild!.className).toContain("rounded-tile");
  });
});

describe("Badge and StatusDot", () => {
  it("a badge takes its variant", () => {
    render(<Badge variant="danger">x</Badge>);
    expect(screen.getByText("x").className).toContain("bg-danger-soft");
    render(<Badge>y</Badge>);
    expect(screen.getByText("y").className).toContain("bg-surface-2");
  });

  it("a dot is coloured by tone, hidden when fine, and pulses when asked", () => {
    const cls = (tone: "ok" | "busy" | "warn" | "error", pulse?: boolean) => {
      const { container, unmount } = render(
        <StatusDot tone={tone} pulse={pulse} className="z" />,
      );
      const span = container.firstElementChild!;
      const out = {
        c: span.className,
        hidden: span.getAttribute("aria-hidden"),
      };
      unmount();
      return out;
    };
    expect(cls("error").c).toContain("bg-danger");
    expect(cls("warn").c).toContain("bg-warning");
    expect(cls("busy", true).c).toContain("animate-pulse");
    const ok = cls("ok", true);
    expect(ok.c).toContain("bg-success");
    expect(ok.c).toContain("opacity-0");
    expect(ok.c).not.toContain("animate-pulse");
    expect(ok.hidden).toBe("true");
    expect(cls("busy").hidden).toBeNull();
  });
});

describe("Button", () => {
  it("is disabled and busy while loading", () => {
    render(<Button loading>Save</Button>);
    const b = screen.getByRole("button");
    expect(b).toHaveProperty("disabled", true);
    expect(b.getAttribute("aria-busy")).toBe("true");
    expect(b.querySelector("svg")).not.toBeNull();
  });

  it("takes its variant and size and stays clickable", () => {
    const onClick = vi.fn();
    render(
      <Button variant="danger" size="sm" full onClick={onClick}>
        Go
      </Button>,
    );
    const b = screen.getByRole("button", { name: "Go" });
    expect(b.className).toContain("bg-danger");
    expect(b.className).toContain("h-9");
    expect(b.className).toContain("w-full");
    expect(b.getAttribute("aria-busy")).toBeNull();
    fireEvent.click(b);
    expect(onClick).toHaveBeenCalledOnce();
  });
});

describe("Card", () => {
  it("lays out its parts with extra classes", () => {
    render(
      <Card className="c" data-testid="card">
        <CardHeader className="h">
          <CardTitle className="t">Title</CardTitle>
          <CardDescription className="d">Desc</CardDescription>
        </CardHeader>
        <CardContent className="b">Body</CardContent>
      </Card>,
    );
    expect(screen.getByTestId("card").className).toMatch(/^rounded-card .* c$/);
    expect(screen.getByText("Title").tagName).toBe("H3");
    expect(screen.getByText("Title").className).toContain("t");
    expect(screen.getByText("Desc").className).toContain("d");
    expect(screen.getByText("Body").className).toBe("p-5 b");
    expect(screen.getByText("Title").parentElement!.className).toContain("h");
  });
});

describe("SystemSubPage", () => {
  it("links back to System and shows its title and subtitle", () => {
    renderAt(
      <SystemSubPage title="Storage" subtitle="Where things live">
        <p>inside</p>
      </SystemSubPage>,
    );
    expect(screen.getByRole("heading", { name: "Storage" })).toBeTruthy();
    expect(screen.getByText("Where things live")).toBeTruthy();
    expect(screen.getByText("inside")).toBeTruthy();
    fireEvent.click(screen.getByRole("link", { name: "System" }));
    expect(screen.getByTestId("where").textContent).toBe("/system");
  });

  it("has no subtitle line without one", () => {
    const { container } = renderAt(<SystemSubPage title="T">x</SystemSubPage>);
    expect(container.querySelector("header p")).toBeNull();
  });
});

function clipboard(writeText: (t: string) => Promise<void>) {
  Object.defineProperty(navigator, "clipboard", {
    configurable: true,
    value: { writeText },
  });
}

describe("RecoveryKeyOverlay", () => {
  it("must be acknowledged before continuing, and saves the acknowledgement", async () => {
    const onClose = vi.fn();
    const onAcknowledge = vi.fn(async () => {});
    render(
      <RecoveryKeyOverlay
        recoveryKey="KEY-1"
        mandatory
        onClose={onClose}
        onAcknowledge={onAcknowledge}
      />,
    );
    expect(screen.getByText("KEY-1")).toBeTruthy();
    const go = screen.getByRole("button", {
      name: "I have saved it — continue",
    });
    expect(go).toHaveProperty("disabled", true);
    fireEvent.click(screen.getByRole("checkbox"));
    expect(go).toHaveProperty("disabled", false);
    fireEvent.click(go);
    await waitFor(() => expect(onClose).toHaveBeenCalledOnce());
    expect(onAcknowledge).toHaveBeenCalledOnce();
  });

  it("stays open when the acknowledgement fails", async () => {
    const onClose = vi.fn();
    render(
      <RecoveryKeyOverlay
        recoveryKey="K"
        mandatory
        onClose={onClose}
        onAcknowledge={() => Promise.reject(new Error("down"))}
      />,
    );
    fireEvent.click(screen.getByRole("checkbox"));
    fireEvent.click(screen.getByRole("button", { name: /continue/ }));
    await waitFor(() =>
      expect(
        screen
          .getByRole("button", { name: /continue/ })
          .getAttribute("aria-busy"),
      ).toBeNull(),
    );
    expect(onClose).not.toHaveBeenCalled();
    expect(screen.getByRole("alert").textContent).toBe("down");
  });

  it("names a failed acknowledgement that was not an error", async () => {
    render(
      <RecoveryKeyOverlay
        recoveryKey="K"
        mandatory
        onClose={() => {}}
        onAcknowledge={() => Promise.reject("x")}
      />,
    );
    fireEvent.click(screen.getByRole("checkbox"));
    fireEvent.click(screen.getByRole("button", { name: /continue/ }));
    expect((await screen.findByRole("alert")).textContent).toBe(
      "Saving that you have the key failed.",
    );
  });

  it("when optional it just closes, without a checkbox", () => {
    const onClose = vi.fn();
    render(
      <RecoveryKeyOverlay
        recoveryKey="K"
        mandatory={false}
        onClose={onClose}
      />,
    );
    expect(screen.queryByRole("checkbox")).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Close" }));
    expect(onClose).toHaveBeenCalledOnce();
  });

  it("mandatory without a way to acknowledge still closes", async () => {
    const onClose = vi.fn();
    render(<RecoveryKeyOverlay recoveryKey="K" mandatory onClose={onClose} />);
    fireEvent.click(screen.getByRole("checkbox"));
    fireEvent.click(screen.getByRole("button", { name: /continue/ }));
    await waitFor(() => expect(onClose).toHaveBeenCalledOnce());
  });

  it("copies the key and shows a tick for two seconds", async () => {
    vi.useFakeTimers();
    const writeText = vi.fn(async () => {});
    clipboard(writeText);
    const { container } = render(
      <RecoveryKeyOverlay
        recoveryKey="K-9"
        mandatory={false}
        onClose={() => {}}
      />,
    );
    const copy = screen.getAllByRole("button")[0];
    const icon = () => copy.querySelector("svg")!.getAttribute("class");
    expect(icon()).not.toContain("text-success");
    await act(async () => {
      fireEvent.click(copy);
    });
    expect(writeText).toHaveBeenCalledWith("K-9");
    expect(icon()).toContain("text-success");
    act(() => vi.advanceTimersByTime(2000));
    expect(icon()).not.toContain("text-success");
    expect(container).toBeTruthy();
  });

  it("a refused copy says to copy it by hand", async () => {
    clipboard(() => Promise.reject(new Error("denied")));
    render(
      <RecoveryKeyOverlay
        recoveryKey="K"
        mandatory={false}
        onClose={() => {}}
      />,
    );
    const copy = screen.getAllByRole("button")[0];
    await act(async () => {
      fireEvent.click(copy);
    });
    expect(copy.querySelector("svg")!.getAttribute("class")).not.toContain(
      "text-success",
    );
    expect(screen.getByRole("alert").textContent).toBe(
      "Could not copy it. Select the key and copy it yourself.",
    );
  });
});

describe("RecoveryKeyGate", () => {
  it("shows a key that was never acknowledged, and acknowledges it", async () => {
    const server = fakeServer({
      "GET /api/backups/recovery-key": {
        configured: true,
        recovery_key: "RK",
        acknowledged: false,
      },
      "POST /api/backups/recovery-key/ack": undefined,
    });
    render(<RecoveryKeyGate />);
    await screen.findByText("RK");
    fireEvent.click(screen.getByRole("checkbox"));
    fireEvent.click(screen.getByRole("button", { name: /continue/ }));
    await waitFor(() => expect(screen.queryByText("RK")).toBeNull());
    expect(server.sent("POST", "/api/backups/recovery-key/ack")).toHaveLength(
      1,
    );
  });

  it("shows nothing when there is no key to see", async () => {
    for (const answer of [
      { configured: false },
      { configured: true },
      { configured: true, recovery_key: "RK", acknowledged: true },
    ]) {
      const server = fakeServer({ "GET /api/backups/recovery-key": answer });
      const { container, unmount } = render(<RecoveryKeyGate />);
      await waitFor(() => expect(server.calls).toHaveLength(1));
      await act(async () => {});
      expect(container.innerHTML).toBe("");
      unmount();
    }
  });

  it("shows nothing when the key cannot be read", async () => {
    const server = fakeServer({
      "GET /api/backups/recovery-key": reply(500, "down"),
    });
    const { container } = render(<RecoveryKeyGate />);
    await waitFor(() => expect(server.calls).toHaveLength(1));
    await act(async () => {});
    expect(container.innerHTML).toBe("");
  });

  it("ignores an answer that arrives after it is gone", async () => {
    let answer!: (r: Response) => void;
    vi.stubGlobal(
      "fetch",
      vi.fn(() => new Promise<Response>((r) => (answer = r))),
    );
    const { unmount } = render(<RecoveryKeyGate />);
    unmount();
    await act(async () =>
      answer(
        new Response(JSON.stringify({ configured: true, recovery_key: "RK" })),
      ),
    );
    expect(screen.queryByText("RK")).toBeNull();
  });
});
