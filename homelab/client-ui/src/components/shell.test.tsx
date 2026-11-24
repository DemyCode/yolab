import {
  act,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
import {
  MemoryRouter,
  Route,
  Routes,
  useOutletContext,
} from "react-router-dom";
import { describe, expect, it, vi } from "vitest";
import { AppShell, Page } from "./AppShell";
import {
  DataMovementCard,
  DrainProgress,
  MovementBanner,
  MovementSummary,
} from "./DataMovement";
import { fakeServer, never } from "@/test/server";
import { Where } from "@/test/Where";
import type { Movement, MovementJob } from "@/types/storage";

function Child() {
  const { signOut } = useOutletContext<{ signOut: () => void }>();
  return <button onClick={signOut}>child sign out</button>;
}

function shell(onLogout = vi.fn(), path = "/") {
  render(
    <MemoryRouter initialEntries={[path]}>
      <Routes>
        <Route element={<AppShell onLogout={onLogout} />}>
          <Route path="*" element={<Child />} />
        </Route>
      </Routes>
      <Where />
    </MemoryRouter>,
  );
  return onLogout;
}

function scrollTo(y: number) {
  Object.defineProperty(window, "scrollY", { configurable: true, value: y });
  act(() => {
    window.dispatchEvent(new Event("scroll"));
  });
}

describe("AppShell", () => {
  it("marks the page you are on in both navigations and moves between pages", () => {
    shell(vi.fn(), "/add");
    const services = screen.getAllByRole("link", { name: "Services" });
    expect(services).toHaveLength(2);
    expect(services[0].className).toContain("text-primary");
    expect(services[1].className).toContain("text-primary");
    expect(
      screen.getAllByRole("link", { name: "Home" })[1].className,
    ).toContain("text-fg-subtle");
    fireEvent.click(screen.getAllByRole("link", { name: "System" })[0]);
    expect(screen.getByTestId("where").textContent).toBe("/system");
    expect(screen.queryByText("Change server")).toBeNull();
  });

  it("signs out on the server, then here, even when the server does not answer", async () => {
    const server = fakeServer({ "POST /api/logout": undefined });
    const onLogout = shell();
    fireEvent.click(screen.getByRole("button", { name: "Sign out" }));
    await waitFor(() => expect(onLogout).toHaveBeenCalledOnce());
    expect(server.sent("POST", "/api/logout")).toHaveLength(1);
    server.routes["POST /api/logout"] = () => Promise.reject(new Error("down"));
    fireEvent.click(screen.getByRole("button", { name: "child sign out" }));
    await waitFor(() => expect(onLogout).toHaveBeenCalledTimes(2));
  });

  it("offers to change server inside the YoLab app", () => {
    const real = window.location;
    const fake = { href: "https://x" };
    Object.defineProperty(window, "location", {
      configurable: true,
      value: fake,
    });
    (window as unknown as { __YOLAB_SHELL__: boolean }).__YOLAB_SHELL__ = true;
    try {
      shell();
      fireEvent.click(screen.getByRole("button", { name: "Change server" }));
      expect(fake.href).not.toBe("https://x");
    } finally {
      delete (window as unknown as { __YOLAB_SHELL__?: boolean })
        .__YOLAB_SHELL__;
      Object.defineProperty(window, "location", {
        configurable: true,
        value: real,
      });
    }
  });

  it("hides the bottom bar when scrolling down past the top, and shows it going up", () => {
    const frames: FrameRequestCallback[] = [];
    vi.stubGlobal("requestAnimationFrame", (cb: FrameRequestCallback) =>
      frames.push(cb),
    );
    Object.defineProperty(window, "scrollY", { configurable: true, value: 0 });
    shell();
    const bar = () => screen.getAllByRole("navigation")[1].className;
    const flush = () => act(() => frames.splice(0).forEach((f) => f(0)));
    scrollTo(5);
    flush();
    expect(bar()).not.toContain("translate-y-full");
    scrollTo(200);
    scrollTo(210);
    expect(frames).toHaveLength(1);
    flush();
    expect(bar()).toContain("translate-y-full");
    scrollTo(100);
    flush();
    expect(bar()).not.toContain("translate-y-full");
    scrollTo(50);
    flush();
    scrollTo(70);
    flush();
    expect(bar()).not.toContain("translate-y-full");
    Object.defineProperty(window, "scrollY", { configurable: true, value: 0 });
  });
});

describe("Page", () => {
  it("has a header with title, subtitle and action, and a wide layout", () => {
    const { container } = render(
      <Page title="T" subtitle="S" action={<button>A</button>} wide>
        body
      </Page>,
    );
    expect(screen.getByRole("heading", { name: "T" })).toBeTruthy();
    expect(screen.getByText("S")).toBeTruthy();
    expect(container.firstElementChild!.className).toContain("max-w-6xl");
  });

  it("has a header for an action alone, and none without title or action", () => {
    const { container, rerender } = render(
      <Page action={<button>A</button>}>b</Page>,
    );
    expect(container.querySelector("header")).not.toBeNull();
    expect(container.querySelector("h1")).toBeNull();
    expect(container.firstElementChild!.className).toContain("max-w-3xl");
    rerender(<Page subtitle="lost">b</Page>);
    expect(container.querySelector("header")).toBeNull();
  });
});

function job(over: Partial<MovementJob> = {}): MovementJob {
  return {
    kind: "move",
    unit: "bytes",
    remaining_bytes: 0,
    to_move_bytes: 2_000_000_000,
    moved_bytes: 1_000_000_000,
    progress: 0.5,
    eta_secs: 30,
    ...over,
  };
}

function movement(over: Partial<Movement> = {}): Movement {
  return {
    state: "working",
    jobs: [job()],
    move_reason: null,
    draining: [],
    filling: [],
    waiting_for: [],
    copies: null,
    clones: 0,
    repairing: false,
    eta_secs: 30,
    inactive_pgs: 0,
    total_pgs: 81,
    ...over,
  };
}

function moving(m: Movement | (() => Promise<Response>)) {
  return fakeServer({ "GET /api/storage/movement": m });
}

describe("DataMovementCard", () => {
  it("shows nothing until there is movement", async () => {
    moving(movement({ state: "settled" }));
    const { container } = render(<DataMovementCard />);
    await waitFor(() => expect(container.innerHTML).toBe(""));
  });

  it("shows one job's progress in bytes, how long is left, and that it is safe", async () => {
    moving(movement());
    render(<DataMovementCard className="c" />);
    expect(
      await screen.findByText("Evening out space across your disks"),
    ).toBeTruthy();
    expect(screen.getByText("less than a minute left")).toBeTruthy();
    expect(screen.getByText(/1(\.0)? GB/)).toBeTruthy();
    expect(
      screen.getByText(
        "Your files are safe and your services keep working meanwhile.",
      ),
    ).toBeTruthy();
    expect(screen.queryByText("Evening out space across disks")).toBeNull();
  });

  it("labels each job when there are several, in percent where bytes are unknown", async () => {
    moving(
      movement({
        jobs: [
          job({ kind: "rebuild" }),
          job({ kind: "free_space", unit: "percent", progress: 0.25 }),
          job({ kind: "repair", progress: null }),
        ],
      }),
    );
    const { container } = render(<DataMovementCard />);
    expect(await screen.findByText("Reorganising your files")).toBeTruthy();
    expect(screen.getByText("Rebuilding missing copies")).toBeTruthy();
    expect(
      screen.getByText("Freeing space from deleted snapshots"),
    ).toBeTruthy();
    expect(container.textContent).toContain("25%");
    expect(container.querySelector("[style*='width: 1.5%']")).not.toBeNull();
    expect(container.querySelector(".border-warning\\/25")).not.toBeNull();
  });

  it("says nothing about time when there are no jobs, nor about safety when there is none", async () => {
    moving(movement({ jobs: [job({ kind: "add_copies" })] }));
    const first = render(<DataMovementCard />);
    expect(await screen.findByText(/Adding another copy/)).toBeTruthy();
    expect(first.container.querySelector("p.mt-4")).toBeNull();
    first.unmount();
    moving(movement({ state: "restarting", jobs: [] }));
    render(<DataMovementCard />);
    expect(await screen.findByText("A machine is restarting")).toBeTruthy();
    expect(screen.queryByText(/left/)).toBeNull();
  });

  it("an unreachable store is an error", async () => {
    moving(movement({ state: "unavailable", jobs: [] }));
    const { container } = render(<DataMovementCard />);
    expect(
      await screen.findByText("Some of your files can't be reached"),
    ).toBeTruthy();
    expect(container.querySelector(".border-danger\\/25")).not.toBeNull();
  });
});

describe("DrainProgress", () => {
  it("shows how far a move got, and nothing without a measured move", async () => {
    moving(movement());
    const first = render(<DrainProgress />);
    expect(await screen.findByText(/% moved,/)).toBeTruthy();
    expect(first.container.textContent).toContain("50");
    first.unmount();
    moving(movement({ jobs: [job({ progress: null })] }));
    const second = render(<DrainProgress />);
    await waitFor(() => expect(second.container.innerHTML).toBe(""));
    second.unmount();
    moving(movement({ jobs: [job({ kind: "rebuild" })] }));
    const third = render(<DrainProgress />);
    await waitFor(() => expect(third.container.innerHTML).toBe(""));
  });
});

describe("MovementBanner", () => {
  it("is shown everywhere only when files cannot be reached or there is no room", async () => {
    moving(movement({ state: "no_room" }));
    const first = render(<MovementBanner className="b" />);
    expect(await screen.findByRole("alert")).toBeTruthy();
    expect(
      screen.getByText("Not enough room to finish reorganising your files"),
    ).toBeTruthy();
    first.unmount();
    moving(movement());
    const second = render(<MovementBanner />);
    await waitFor(() => expect(second.container.innerHTML).toBe(""));
  });
});

describe("MovementSummary", () => {
  it("summarises the movement in a few words", async () => {
    const cases: [Movement, string][] = [
      [movement(), "Moving files, 50%"],
      [
        movement({ jobs: [job(), job({ kind: "rebuild" })] }),
        "Reorganising files, 50%",
      ],
      [
        movement({ state: "unavailable" }),
        "Some of your files can't be reached",
      ],
      [movement({ jobs: [] }), "Rearranging your files across your disks"],
      [
        movement({ jobs: [job({ progress: null })] }),
        "Evening out space across your disks",
      ],
    ];
    for (const [m, text] of cases) {
      moving(m);
      const { container, unmount } = render(<MovementSummary />);
      await waitFor(() => expect(container.textContent).toBe(text));
      unmount();
    }
  });

  it("is nothing while loading or settled", async () => {
    moving(never());
    const first = render(<MovementSummary />);
    expect(first.container.innerHTML).toBe("");
  });

  it("is nothing once settled", async () => {
    moving(movement({ state: "settled" }));
    const second = render(<MovementSummary />);
    await waitFor(() => expect(second.container.innerHTML).toBe(""));
  });
});
