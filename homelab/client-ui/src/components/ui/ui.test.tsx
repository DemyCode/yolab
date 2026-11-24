import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { Field, Input, Select, Switch, Toggle } from "./input";
import {
  Banner,
  EmptyState,
  ServiceTrouble,
  Skeleton,
  Spinner,
} from "./feedback";
import { ConfirmDialog, Sheet } from "./sheet";

describe("inputs", () => {
  it("an input and a select keep their extra classes and props", () => {
    render(
      <>
        <Input className="i" placeholder="type" />
        <Select className="s" aria-label="pick">
          <option>a</option>
        </Select>
      </>,
    );
    expect(screen.getByPlaceholderText("type").className).toMatch(/h-11 .* i$/);
    expect(screen.getByLabelText("pick").className).toMatch(/pr-9 s$/);
  });

  it("a field shows its error over its help, and nothing without either", () => {
    const { rerender, container } = render(
      <Field label="Name" help="Help" error="Bad" htmlFor="n">
        <input id="n" />
      </Field>,
    );
    expect(screen.getByLabelText("Name")).toBeTruthy();
    expect(screen.getByText("Bad").className).toContain("text-danger");
    expect(screen.queryByText("Help")).toBeNull();
    rerender(
      <Field label="Name" help="Help">
        <input />
      </Field>,
    );
    expect(screen.getByText("Help")).toBeTruthy();
    rerender(
      <Field label="Name">
        <input />
      </Field>,
    );
    expect(container.querySelectorAll("p")).toHaveLength(0);
  });

  it("a switch flips without telling its parent row", () => {
    const onChange = vi.fn();
    const onRow = vi.fn();
    const { rerender } = render(
      <div onClick={onRow}>
        <Switch checked={false} onChange={onChange} label="On" />
      </div>,
    );
    const s = screen.getByRole("switch", { name: "On" });
    expect(s.getAttribute("aria-checked")).toBe("false");
    expect(s.className).toContain("bg-surface-3");
    fireEvent.click(s);
    expect(onChange).toHaveBeenCalledWith(true);
    expect(onRow).not.toHaveBeenCalled();
    rerender(
      <div onClick={onRow}>
        <Switch checked onChange={onChange} label="On" disabled />
      </div>,
    );
    expect(s.className).toContain("bg-primary");
    expect(s.firstElementChild!.className).toContain("translate-x-5");
    expect(s).toHaveProperty("disabled", true);
  });

  it("a toggle row flips when the row or the switch is clicked", () => {
    const onChange = vi.fn();
    render(<Toggle checked onChange={onChange} label="Backups" help="Daily" />);
    fireEvent.click(screen.getByText("Backups"));
    fireEvent.click(screen.getByRole("switch"));
    expect(onChange.mock.calls).toEqual([[false], [false]]);
    expect(screen.getByText("Daily")).toBeTruthy();
  });

  it("a toggle without help shows only its label", () => {
    const { container } = render(
      <Toggle checked={false} onChange={() => {}} label="L" />,
    );
    expect(container.querySelectorAll("span.block")).toHaveLength(1);
  });
});

describe("feedback", () => {
  it("skeleton and spinner take extra classes", () => {
    const { container } = render(
      <>
        <Skeleton className="sk" />
        <Spinner className="sp" />
      </>,
    );
    expect(container.firstElementChild!.className).toContain("shimmer");
    expect(container.firstElementChild!.className).toContain("sk");
    expect(screen.getByLabelText("Loading").getAttribute("class")).toContain(
      "sp",
    );
  });

  it("a banner is an alert only for errors, and shows its body and action", () => {
    const tones = ["info", "success", "warning", "error"] as const;
    const wraps = [
      "bg-primary-soft",
      "bg-success-soft",
      "bg-warning-soft",
      "bg-danger-soft",
    ];
    tones.forEach((tone, i) => {
      const { unmount } = render(
        <Banner
          tone={tone}
          title="T"
          action={<button>Do</button>}
          className="b"
        >
          Body
        </Banner>,
      );
      const role = tone === "error" ? "alert" : "status";
      const banner = screen.getByRole(role);
      expect(banner.className).toContain(wraps[i]);
      expect(banner.className).toContain("b");
      expect(screen.getByText("Body")).toBeTruthy();
      expect(screen.getByRole("button", { name: "Do" })).toBeTruthy();
      unmount();
    });
    const { container } = render(<Banner tone="info" title="Only" />);
    expect(container.querySelectorAll(".mt-1, .mt-3")).toHaveLength(0);
  });

  it("an empty state shows only what it was given", () => {
    const { container, rerender } = render(
      <EmptyState
        icon={<i>ic</i>}
        title="Nothing"
        body="Yet"
        action={<a>Add</a>}
      />,
    );
    expect(screen.getByText("ic")).toBeTruthy();
    expect(screen.getByText("Yet")).toBeTruthy();
    expect(screen.getByText("Add")).toBeTruthy();
    rerender(<EmptyState title="Nothing" />);
    expect(container.querySelectorAll("p, i, a")).toHaveLength(0);
  });

  it("service trouble offers to try again", () => {
    const onRetry = vi.fn();
    render(<ServiceTrouble onRetry={onRetry} />);
    fireEvent.click(screen.getByRole("button", { name: "Try again" }));
    expect(onRetry).toHaveBeenCalledOnce();
  });
});

describe("Sheet", () => {
  it("shows nothing while closed", () => {
    const { container } = render(
      <Sheet open={false} onClose={() => {}}>
        x
      </Sheet>,
    );
    expect(container.innerHTML).toBe("");
    expect(document.body.style.overflow).toBe("");
  });

  it("locks scrolling, takes focus, and closes on Escape, backdrop or the cross", () => {
    const onClose = vi.fn();
    document.body.style.overflow = "auto";
    const { unmount } = render(
      <Sheet
        open
        onClose={onClose}
        title="Title"
        subtitle="Sub"
        footer={<p>Foot</p>}
        wide
      >
        Body
      </Sheet>,
    );
    expect(document.body.style.overflow).toBe("hidden");
    const dialog = screen.getByRole("dialog");
    expect(document.activeElement).toBe(dialog.children[1]);
    expect(dialog.children[1].className).toContain("sm:max-w-2xl");
    expect(screen.getByText("Sub")).toBeTruthy();
    expect(screen.getByText("Foot")).toBeTruthy();
    fireEvent.keyDown(document, { key: "Enter" });
    expect(onClose).not.toHaveBeenCalled();
    fireEvent.keyDown(document, { key: "Escape" });
    fireEvent.click(dialog.children[0]);
    fireEvent.click(screen.getByRole("button", { name: "Close" }));
    expect(onClose).toHaveBeenCalledTimes(3);
    unmount();
    expect(document.body.style.overflow).toBe("auto");
    fireEvent.keyDown(document, { key: "Escape" });
    expect(onClose).toHaveBeenCalledTimes(3);
    document.body.style.overflow = "";
  });

  it("has no header without a title or subtitle, and a header with either", () => {
    const { rerender } = render(
      <Sheet open onClose={() => {}}>
        Body
      </Sheet>,
    );
    expect(screen.queryByRole("button", { name: "Close" })).toBeNull();
    expect(screen.getByRole("dialog").children[1].className).toContain(
      "sm:max-w-md",
    );
    rerender(
      <Sheet open onClose={() => {}} subtitle="Only sub">
        Body
      </Sheet>,
    );
    expect(screen.queryByRole("heading")).toBeNull();
    expect(screen.getByRole("button", { name: "Close" })).toBeTruthy();
    rerender(
      <Sheet open onClose={() => {}} title="Only title">
        Body
      </Sheet>,
    );
    expect(screen.getByRole("heading", { name: "Only title" })).toBeTruthy();
    expect(screen.queryByText("Only sub")).toBeNull();
  });
});

describe("ConfirmDialog", () => {
  it("confirms or cancels", () => {
    const onClose = vi.fn();
    const onConfirm = vi.fn();
    render(
      <ConfirmDialog
        open
        onClose={onClose}
        onConfirm={onConfirm}
        title="Remove?"
        body="Gone forever"
      />,
    );
    const go = screen.getByRole("button", { name: "Continue" });
    expect(go.className).toContain("bg-primary");
    fireEvent.click(go);
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(onConfirm).toHaveBeenCalledOnce();
    expect(onClose).toHaveBeenCalledOnce();
  });

  it("a destructive confirm is red, and both buttons wait while busy", () => {
    render(
      <ConfirmDialog
        open
        onClose={() => {}}
        onConfirm={() => {}}
        title="Remove?"
        body="x"
        confirmLabel="Remove"
        destructive
        busy
      />,
    );
    expect(screen.getByRole("button", { name: "Remove" }).className).toContain(
      "bg-danger",
    );
    expect(screen.getByRole("button", { name: "Cancel" })).toHaveProperty(
      "disabled",
      true,
    );
    expect(screen.getByRole("button", { name: "Remove" })).toHaveProperty(
      "disabled",
      true,
    );
  });
});
