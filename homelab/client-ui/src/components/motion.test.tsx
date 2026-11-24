import { act, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { AnimatedList, Collapse, RollingNumber, Swap } from "./motion";

interface Anim {
  keyframes: Keyframe[];
  options: KeyframeAnimationOptions;
  el: Element;
  onfinish: (() => void) | null;
  cancel: ReturnType<typeof vi.fn>;
}

const proto = Element.prototype as unknown as Record<string, unknown>;

function motion({ getAnimations = true } = {}) {
  const anims: Anim[] = [];
  proto.animate = function (
    this: Element,
    keyframes: Keyframe[],
    options: KeyframeAnimationOptions,
  ) {
    const a: Anim = {
      keyframes,
      options,
      el: this,
      onfinish: null,
      cancel: vi.fn(),
    };
    anims.push(a);
    return a;
  };
  if (getAnimations) {
    proto.getAnimations = function (this: Element) {
      return anims.filter((a) => a.el === this);
    };
  }
  return anims;
}

function placed() {
  vi.spyOn(HTMLElement.prototype, "getBoundingClientRect").mockImplementation(
    function (this: HTMLElement) {
      const top = [...(this.parentElement?.children ?? [])].indexOf(this) * 10;
      return { left: 0, top } as DOMRect;
    },
  );
}

function reducedMotion() {
  vi.stubGlobal("matchMedia", (q: string) => ({
    matches: q.includes("reduce"),
  }));
}

afterEach(() => {
  delete proto.animate;
  delete proto.getAnimations;
});

const list = (items: string[], as?: "div" | "li" | "tr") => (
  <AnimatedList items={items} keyOf={(s) => s} itemClassName="row" as={as}>
    {(s) => s}
  </AnimatedList>
);

const shown = (container: HTMLElement) =>
  [...container.querySelectorAll(".row")].map((r) => r.textContent);

describe("AnimatedList without animation", () => {
  it("adds and drops rows at once", () => {
    const { container, rerender } = render(list(["a", "b"]));
    expect(shown(container)).toEqual(["a", "b"]);
    rerender(list(["b", "c"]));
    expect(shown(container)).toEqual(["b", "c"]);
  });

  it("renders as list items or table rows", () => {
    const { container } = render(<ul>{list(["a"], "li")}</ul>);
    expect(container.querySelector("li.row")).not.toBeNull();
    const table = render(
      <table>
        <tbody>{list(["a"], "tr")}</tbody>
      </table>,
    );
    expect(table.container.querySelector("tr.row")).not.toBeNull();
  });

  it("does not animate when the person asked for less motion", () => {
    const anims = motion();
    reducedMotion();
    const { container, rerender } = render(list(["a"]));
    rerender(list(["b"]));
    expect(anims).toHaveLength(0);
    expect(shown(container)).toEqual(["b"]);
  });
});

describe("AnimatedList with animation", () => {
  it("fades a new row in, but not the rows it started with", () => {
    const anims = motion();
    const { rerender } = render(list(["a"]));
    expect(anims).toHaveLength(0);
    rerender(list(["a", "b"]));
    expect(anims).toHaveLength(1);
    expect(anims[0].el.textContent).toBe("b");
    expect(anims[0].keyframes[0]).toMatchObject({ opacity: 0 });
  });

  it("fades a removed row out, keeps it unclickable, and drops it when done", () => {
    const anims = motion();
    const { container, rerender } = render(list(["a", "b"]));
    rerender(list(["a"]));
    const leaving = container.querySelector("[data-leaving]") as HTMLElement;
    expect(leaving.textContent).toBe("b");
    expect(leaving.getAttribute("aria-hidden")).toBe("true");
    expect(leaving.style.pointerEvents).toBe("none");
    expect(anims[0].options.fill).toBe("forwards");
    rerender(list(["a"]));
    expect(anims).toHaveLength(1);
    act(() => anims[0].onfinish!());
    expect(shown(container)).toEqual(["a"]);
  });

  it("a row that comes back while fading out is kept and its fade stopped", () => {
    const anims = motion();
    const { container, rerender } = render(list(["a", "b"]));
    rerender(list(["a"]));
    rerender(list(["a", "b"]));
    expect(container.querySelector("[data-leaving]")).toBeNull();
    const b = [...container.querySelectorAll(".row")][1] as HTMLElement;
    expect(b.style.pointerEvents).toBe("");
    expect(anims[0].cancel).toHaveBeenCalledOnce();
  });

  it("a returning row in a browser without getAnimations is just kept", () => {
    motion({ getAnimations: false });
    const { container, rerender } = render(list(["a", "b"]));
    rerender(list(["a"]));
    rerender(list(["a", "b"]));
    expect(shown(container)).toEqual(["a", "b"]);
  });

  it("slides rows that moved, and leaves still rows alone", () => {
    const anims = motion();
    placed();
    const { rerender } = render(<div>{list(["a", "b", "c"])}</div>);
    rerender(<div>{list(["b", "a", "c"])}</div>);
    expect(anims.map((a) => [a.el.textContent, a.keyframes[0]])).toEqual([
      ["b", { transform: "translate(0px, 10px)" }],
      ["a", { transform: "translate(0px, -10px)" }],
    ]);
  });
});

describe("RollingNumber", () => {
  function frames() {
    const queue: FrameRequestCallback[] = [];
    vi.stubGlobal("requestAnimationFrame", (cb: FrameRequestCallback) =>
      queue.push(cb),
    );
    const cancel = vi.fn();
    vi.stubGlobal("cancelAnimationFrame", cancel);
    vi.spyOn(performance, "now").mockReturnValue(0);
    return {
      cancel,
      run(at: number) {
        const cb = queue.shift()!;
        act(() => cb(at));
      },
    };
  }

  it("rolls to a new value over its time and stops there", () => {
    const f = frames();
    const { container, rerender } = render(
      <RollingNumber value={0} className="n" />,
    );
    expect(container.textContent).toBe("0");
    expect(container.firstElementChild!.className).toBe("tabular-nums n");
    rerender(<RollingNumber value={100} />);
    f.run(350);
    const mid = Number(container.textContent);
    expect(mid).toBeGreaterThan(50);
    expect(mid).toBeLessThan(100);
    f.run(700);
    expect(container.textContent).toBe("100");
  });

  it("does nothing when the value did not change", () => {
    const f = frames();
    const { rerender } = render(<RollingNumber value={5} />);
    rerender(<RollingNumber value={5} />);
    expect(f.cancel).not.toHaveBeenCalled();
  });

  it("stops rolling when it goes away", () => {
    const f = frames();
    const { rerender, unmount } = render(<RollingNumber value={0} />);
    rerender(<RollingNumber value={10} />);
    unmount();
    expect(f.cancel).toHaveBeenCalled();
  });

  it("jumps straight to the value with less motion or without frames", () => {
    reducedMotion();
    const a = render(<RollingNumber value={1} format={(n) => `${n}%`} />);
    a.rerender(<RollingNumber value={2.5} format={(n) => `${n}%`} />);
    expect(a.container.textContent).toBe("2.5%");
    vi.unstubAllGlobals();
    vi.stubGlobal("requestAnimationFrame", undefined);
    const b = render(<RollingNumber value={1} />);
    b.rerender(<RollingNumber value={2.25} />);
    expect(b.container.textContent).toBe("2.25");
  });
});

describe("Collapse", () => {
  it("shows its content while open and hides it at once without animation", () => {
    const { container, rerender } = render(
      <Collapse open className="c">
        <p>inside</p>
      </Collapse>,
    );
    expect(container.firstElementChild!.className).toBe("flow-root c");
    rerender(
      <Collapse open={false}>
        <p>inside</p>
      </Collapse>,
    );
    expect(container.innerHTML).toBe("");
    rerender(
      <Collapse open>
        <p>again</p>
      </Collapse>,
    );
    expect(screen.getByText("again")).toBeTruthy();
  });

  it("starts closed with nothing in it", () => {
    const { container } = render(
      <Collapse open={false}>
        <p>inside</p>
      </Collapse>,
    );
    expect(container.innerHTML).toBe("");
  });

  it("animates closed keeping the last content, then removes it", () => {
    const anims = motion();
    const { container, rerender } = render(
      <Collapse open>
        <p>first</p>
      </Collapse>,
    );
    rerender(
      <Collapse open>
        <p>latest</p>
      </Collapse>,
    );
    rerender(
      <Collapse open={false}>
        <p>ignored</p>
      </Collapse>,
    );
    expect(screen.getByText("latest")).toBeTruthy();
    expect(anims).toHaveLength(1);
    expect(anims[0].keyframes[1]).toEqual({ height: "0px", opacity: 0 });
    const el = container.firstElementChild as HTMLElement;
    expect(el.style.overflow).toBe("hidden");
    act(() => anims[0].onfinish!());
    expect(container.innerHTML).toBe("");
  });

  it("animates open and lets its content overflow again when done", () => {
    const anims = motion();
    const { container, rerender } = render(
      <Collapse open={false}>
        <p>inside</p>
      </Collapse>,
    );
    rerender(
      <Collapse open>
        <p>inside</p>
      </Collapse>,
    );
    expect(anims[0].keyframes[0]).toEqual({ height: "0px", opacity: 0 });
    const el = container.firstElementChild as HTMLElement;
    act(() => anims[0].onfinish!());
    expect(el.style.overflow).toBe("");
    expect(screen.getByText("inside")).toBeTruthy();
  });

  it("stops a running animation before starting the next", () => {
    const anims = motion();
    const { rerender } = render(
      <Collapse open>
        <p>x</p>
      </Collapse>,
    );
    rerender(
      <Collapse open={false}>
        <p>x</p>
      </Collapse>,
    );
    rerender(
      <Collapse open>
        <p>x</p>
      </Collapse>,
    );
    expect(anims[0].cancel).toHaveBeenCalledOnce();
    expect(anims).toHaveLength(2);
  });
});

describe("Swap", () => {
  it("fades in new content when its id changes", () => {
    const anims = motion();
    const { container, rerender } = render(<Swap id="a">A</Swap>);
    expect(container.firstElementChild!.className).toBe("inline-block");
    rerender(<Swap id="a">A</Swap>);
    expect(anims).toHaveLength(0);
    rerender(
      <Swap id="b" className="s">
        B
      </Swap>,
    );
    expect(anims).toHaveLength(1);
    expect(container.firstElementChild!.className).toBe("s");
  });

  it("just swaps without animation", () => {
    const { container, rerender } = render(<Swap id={1}>A</Swap>);
    rerender(<Swap id={2}>B</Swap>);
    expect(container.textContent).toBe("B");
  });
});
