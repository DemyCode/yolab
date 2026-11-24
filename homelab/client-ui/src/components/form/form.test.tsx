import {
  act,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
import type {
  ArrayFieldTemplateProps,
  FieldTemplateProps,
  ObjectFieldTemplateProps,
  WidgetProps,
} from "@rjsf/utils";
import { describe, expect, it, vi } from "vitest";
import { templates, widgets } from "./registry";
import {
  ArrayFieldTemplate,
  FieldTemplate,
  ObjectFieldTemplate,
} from "./templates";
import {
  CheckboxWidget,
  PasswordWidget,
  TextareaWidget,
  TreeFolderWidget,
  TunnelWidget,
  YolabTokenWidget,
} from "./widgets";
import { fakeServer, reply } from "@/test/server";

function props(over: Partial<WidgetProps> = {}): WidgetProps {
  return {
    id: "f",
    name: "f",
    value: undefined,
    onChange: vi.fn(),
    onBlur: vi.fn(),
    onFocus: vi.fn(),
    disabled: false,
    readonly: false,
    autofocus: false,
    options: {},
    schema: {},
    label: "",
    placeholder: "",
    ...over,
  } as unknown as WidgetProps;
}

describe("registry", () => {
  it("names every template and widget the forms use", () => {
    expect(Object.keys(templates)).toEqual([
      "FieldTemplate",
      "ObjectFieldTemplate",
      "ArrayFieldTemplate",
    ]);
    expect(widgets.TreeFolderWidget).toBe(TreeFolderWidget);
    expect(Object.keys(widgets)).toHaveLength(6);
  });
});

describe("FieldTemplate", () => {
  const field = (over: Partial<FieldTemplateProps>) =>
    render(
      <FieldTemplate
        {...({
          id: "x",
          label: "Name",
          help: <i>help</i>,
          errors: <b>err</b>,
          children: <input id="x" />,
          schema: { type: "string", description: "Desc" },
          hidden: false,
          ...over,
        } as unknown as FieldTemplateProps)}
      />,
    );

  it("shows its label, description, input, errors and help", () => {
    field({});
    expect(screen.getByLabelText("Name")).toBeTruthy();
    expect(screen.getByText("Desc")).toBeTruthy();
    expect(screen.getByText("err")).toBeTruthy();
    expect(screen.getByText("help")).toBeTruthy();
  });

  it("is nothing when hidden, and only the input for a switch", () => {
    expect(field({ hidden: true }).container.innerHTML).toBe("");
    const { container } = field({
      schema: { type: "boolean", description: "D" },
    });
    expect(container.querySelector("label")).toBeNull();
    expect(screen.queryByText("D")).toBeNull();
  });

  it("leaves out a missing label and description", () => {
    const { container } = field({ label: "", schema: { type: "string" } });
    expect(container.querySelector("label")).toBeNull();
    expect(container.querySelector("p")).toBeNull();
  });
});

describe("ArrayFieldTemplate", () => {
  it("says when nothing was added, and offers to add", () => {
    const onAddClick = vi.fn();
    render(
      <ArrayFieldTemplate
        {...({
          items: [],
          canAdd: true,
          onAddClick,
        } as unknown as ArrayFieldTemplateProps)}
      />,
    );
    expect(screen.getByText("Nothing added yet.")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Add" }));
    expect(onAddClick).toHaveBeenCalledOnce();
  });

  it("lists items with a remove button where allowed", () => {
    const drop = vi.fn();
    const onDropIndexClick = vi.fn(() => drop);
    render(
      <ArrayFieldTemplate
        {...({
          canAdd: false,
          items: [
            {
              key: "a",
              index: 0,
              children: <span>one</span>,
              hasRemove: true,
              onDropIndexClick,
            },
            {
              key: "b",
              index: 1,
              children: <span>two</span>,
              hasRemove: false,
              onDropIndexClick,
            },
          ],
        } as unknown as ArrayFieldTemplateProps)}
      />,
    );
    expect(screen.getByText("one")).toBeTruthy();
    expect(screen.queryByText("Nothing added yet.")).toBeNull();
    expect(screen.queryByRole("button", { name: "Add" })).toBeNull();
    const removes = screen.getAllByRole("button", { name: "Remove" });
    expect(removes).toHaveLength(1);
    fireEvent.click(removes[0]);
    expect(onDropIndexClick).toHaveBeenCalledWith(0);
    expect(drop).toHaveBeenCalledOnce();
  });
});

describe("ObjectFieldTemplate", () => {
  const object = (id: string, uiSchema?: object) =>
    render(
      <ObjectFieldTemplate
        {...({
          idSchema: { $id: id },
          uiSchema,
          properties: [
            { name: "a", content: <span>A</span> },
            { name: "b", content: <span>B</span> },
          ],
        } as unknown as ObjectFieldTemplateProps)}
      />,
    );

  it("indents a field attached to the switch above it", () => {
    object("root", { b: { "ui:options": { attached: true } } });
    expect(screen.getByText("A").parentElement!.className).toBe("");
    expect(screen.getByText("B").parentElement!.className).toContain(
      "border-l-2",
    );
  });

  it("frames a nested object, and works without a ui schema", () => {
    const { container } = object("root_inner");
    expect(container.firstElementChild!.className).toContain("border");
  });
});

describe("TunnelWidget", () => {
  it("turns what is typed into an address and shows it", () => {
    const onChange = vi.fn();
    const { rerender } = render(
      <TunnelWidget
        {...props({ value: "", onChange, options: { domain: "x.yolab.io" } })}
      />,
    );
    fireEvent.change(screen.getByRole("textbox"), {
      target: { value: "--My Files!" },
    });
    expect(onChange).toHaveBeenCalledWith("my-files-");
    expect(screen.queryByText(/https:/)).toBeNull();
    rerender(
      <TunnelWidget
        {...props({ value: "files", options: { domain: "x.yolab.io" } })}
      />,
    );
    expect(screen.getByText("https://files.x.yolab.io")).toBeTruthy();
  });

  it("shows no address without a domain or a value, and locks when read-only", () => {
    render(<TunnelWidget {...props({ value: 3, readonly: true })} />);
    expect((screen.getByRole("textbox") as HTMLInputElement).value).toBe("");
    expect(screen.getByRole("textbox")).toHaveProperty("disabled", true);
    render(<TunnelWidget {...props({ value: "files" })} />);
    expect(screen.queryByText(/https:/)).toBeNull();
  });
});

describe("YolabTokenWidget", () => {
  it("uses the box's account until another token is chosen, and back", () => {
    const onChange = vi.fn();
    const { container } = render(<YolabTokenWidget {...props({ onChange })} />);
    expect(screen.getByText(/Uses this box's YoLab account/)).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Use another token" }));
    const input = container.querySelector("input")!;
    fireEvent.change(input, { target: { value: "tok" } });
    expect(onChange).toHaveBeenCalledWith("tok");
    fireEvent.click(screen.getByRole("button", { name: "Use this box's" }));
    expect(onChange).toHaveBeenLastCalledWith(undefined);
    expect(
      screen.getByRole("button", { name: "Use another token" }),
    ).toBeTruthy();
  });

  it("starts on its own token when one is set, and locks when disabled", () => {
    const { container } = render(
      <YolabTokenWidget {...props({ value: "tok", disabled: true })} />,
    );
    expect(container.querySelector("input")!.value).toBe("tok");
    expect(
      screen.getByRole("button", { name: "Use this box's" }),
    ).toHaveProperty("disabled", true);
    render(<YolabTokenWidget {...props({ value: 5, readonly: true })} />);
    expect(
      screen.getByRole("button", { name: "Use another token" }),
    ).toHaveProperty("disabled", true);
  });
});

describe("PasswordWidget", () => {
  it("hides and shows the password", () => {
    const onChange = vi.fn();
    const { container } = render(
      <PasswordWidget {...props({ value: "pw", onChange })} />,
    );
    const input = container.querySelector("input")!;
    expect(input.type).toBe("password");
    fireEvent.click(screen.getByRole("button", { name: "Show" }));
    expect(input.type).toBe("text");
    fireEvent.click(screen.getByRole("button", { name: "Hide" }));
    expect(input.type).toBe("password");
    fireEvent.change(input, { target: { value: "new" } });
    expect(onChange).toHaveBeenCalledWith("new");
    expect(
      screen.queryByRole("button", { name: "Generate a new one" }),
    ).toBeNull();
  });

  it("generates a new secret at least 24 long, or as long as the schema asks", () => {
    const onChange = vi.fn();
    const { rerender } = render(
      <PasswordWidget
        {...props({ value: 1, onChange, options: { generate: true } })}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: "Generate a new one" }));
    expect((onChange.mock.calls[0][0] as string).length).toBe(24);
    rerender(
      <PasswordWidget
        {...props({
          onChange,
          options: { generate: true },
          schema: { minLength: 40 },
          disabled: true,
        })}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: "Generate a new one" }));
    expect((onChange.mock.calls[1][0] as string).length).toBe(40);
  });
});

describe("CheckboxWidget", () => {
  it("is a toggle labelled by the field or its schema", () => {
    const onChange = vi.fn();
    const { rerender } = render(
      <CheckboxWidget
        {...props({
          label: "Tor",
          value: true,
          onChange,
          schema: { description: "Hidden" },
        })}
      />,
    );
    expect(screen.getByText("Hidden")).toBeTruthy();
    fireEvent.click(screen.getByRole("switch", { name: "Tor" }));
    expect(onChange).toHaveBeenCalledWith(false);
    rerender(
      <CheckboxWidget {...props({ schema: { title: "From schema" } })} />,
    );
    expect(screen.getByRole("switch", { name: "From schema" })).toBeTruthy();
    rerender(<CheckboxWidget {...props({})} />);
    expect(screen.getByRole("switch").getAttribute("aria-label")).toBe("");
  });

  it("cannot be flipped when disabled or read-only", () => {
    const onChange = vi.fn();
    const { rerender } = render(
      <CheckboxWidget {...props({ label: "x", onChange, disabled: true })} />,
    );
    fireEvent.click(screen.getByRole("switch"));
    rerender(
      <CheckboxWidget {...props({ label: "x", onChange, readonly: true })} />,
    );
    fireEvent.click(screen.getByRole("switch"));
    expect(onChange).not.toHaveBeenCalled();
  });
});

describe("TextareaWidget", () => {
  it("edits text and shows nothing for a non-text value", () => {
    const onChange = vi.fn();
    const { rerender } = render(
      <TextareaWidget {...props({ value: "a", onChange, placeholder: "p" })} />,
    );
    const t = screen.getByPlaceholderText("p") as HTMLTextAreaElement;
    expect(t.value).toBe("a");
    fireEvent.change(t, { target: { value: "b" } });
    expect(onChange).toHaveBeenCalledWith("b");
    rerender(
      <TextareaWidget
        {...props({ value: 1, readonly: true, placeholder: "p" })}
      />,
    );
    expect(t.value).toBe("");
    expect(t.disabled).toBe(true);
  });
});

const listing = (folders: string[], truncated = false) => ({
  path: "",
  folders,
  truncated,
});
const q = (path: string) =>
  `GET /api/tree/folders?path=${encodeURIComponent(path)}`;

describe("TreeFolderWidget", () => {
  it("lists the top of the tree and picks it", async () => {
    const onChange = vi.fn();
    fakeServer({ [q("")]: listing(["media", "photos"]) });
    render(<TreeFolderWidget {...props({ value: "", onChange })} />);
    expect(screen.getByText("Looking…")).toBeTruthy();
    expect(
      await screen.findByRole("listbox", { name: "Folders in /" }),
    ).toBeTruthy();
    expect(screen.getByText("No folder picked yet.")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Use /" }));
    expect(onChange).toHaveBeenCalledWith("/");
  });

  it("goes into a folder by clicking it, and back up through the trail", async () => {
    const onChange = vi.fn();
    fakeServer({
      [q("")]: listing(["media"]),
      [q("media")]: listing(["films"]),
      [q("media/films")]: listing([]),
    });
    render(<TreeFolderWidget {...props({ value: "", onChange })} />);
    fireEvent.mouseDown(await screen.findByRole("option", { name: "media" }));
    fireEvent.mouseDown(await screen.findByRole("option", { name: "films" }));
    expect(
      await screen.findByText("/media/films has no folders inside it."),
    ).toBeTruthy();
    const trail = screen.getByRole("navigation");
    expect(trail.querySelectorAll("svg")).toHaveLength(1);
    expect(screen.getByRole("button", { name: "films" })).toHaveProperty(
      "disabled",
      true,
    );
    fireEvent.click(screen.getByRole("button", { name: "Use /media/films" }));
    expect(onChange).toHaveBeenCalledWith("/media/films");
    fireEvent.click(screen.getByRole("button", { name: "media" }));
    expect(await screen.findByRole("option", { name: "films" })).toBeTruthy();
  });

  it("opens next to the folder already picked and says it is in use", async () => {
    fakeServer({
      [q("media")]: listing(["films"]),
      [q("media/films")]: listing([]),
    });
    render(<TreeFolderWidget {...props({ value: "/media/films/" })} />);
    expect(
      await screen.findByRole("listbox", { name: "Folders in /media" }),
    ).toBeTruthy();
    expect(screen.getAllByText("/media/films").length).toBeGreaterThan(0);
    fireEvent.mouseDown(screen.getByRole("option", { name: "films" }));
    const using = await screen.findByRole("button", {
      name: "Using /media/films",
    });
    expect(using).toHaveProperty("disabled", true);
  });

  it("a value that is not text is no folder picked", async () => {
    fakeServer({ [q("")]: listing([]) });
    render(<TreeFolderWidget {...props({ value: 7, readonly: true })} />);
    expect(await screen.findByText("No folder picked yet.")).toBeTruthy();
  });

  it("the whole tree picked shows as / in use", async () => {
    fakeServer({ [q("")]: listing([]) });
    render(<TreeFolderWidget {...props({ value: "/" })} />);
    expect(await screen.findByRole("button", { name: "Using /" })).toBeTruthy();
  });

  it("moves with the arrow keys, enters with Enter and goes up with Backspace", async () => {
    fakeServer({
      [q("")]: listing(["a", "b", "c"]),
      [q("b")]: listing([]),
    });
    render(<TreeFolderWidget {...props({ value: "" })} />);
    await screen.findByRole("listbox");
    const input = screen.getByRole("textbox");
    const selected = () =>
      screen
        .getAllByRole("option")
        .find((o) => o.getAttribute("aria-selected") === "true")!.textContent;
    fireEvent.keyDown(input, { key: "Backspace" });
    fireEvent.keyDown(input, { key: "x" });
    expect(selected()).toBe("a");
    fireEvent.keyDown(input, { key: "ArrowUp" });
    expect(selected()).toBe("c");
    fireEvent.keyDown(input, { key: "ArrowDown" });
    fireEvent.keyDown(input, { key: "ArrowDown" });
    expect(selected()).toBe("b");
    fireEvent.mouseEnter(screen.getByRole("option", { name: "c" }));
    expect(selected()).toBe("c");
    fireEvent.mouseEnter(screen.getByRole("option", { name: "b" }));
    fireEvent.keyDown(input, { key: "Enter" });
    expect(
      await screen.findByText("/b has no folders inside it."),
    ).toBeTruthy();
    fireEvent.keyDown(input, { key: "ArrowDown" });
    fireEvent.keyDown(input, { key: "ArrowUp" });
    fireEvent.keyDown(input, { key: "Enter" });
    expect(screen.getByText("/b has no folders inside it.")).toBeTruthy();
    fireEvent.change(input, { target: { value: "z" } });
    fireEvent.keyDown(input, { key: "Backspace" });
    expect(screen.getByText("No folder in /b matches “z”.")).toBeTruthy();
    fireEvent.change(input, { target: { value: "" } });
    fireEvent.keyDown(input, { key: "Backspace" });
    expect(
      await screen.findByRole("listbox", { name: "Folders in /" }),
    ).toBeTruthy();
  });

  it("filters without slashes and says when only some folders are shown", async () => {
    fakeServer({ [q("")]: listing(["music", "movies"], true) });
    render(<TreeFolderWidget {...props({ value: "" })} />);
    expect(await screen.findByText(/Only some folders are shown/)).toBeTruthy();
    fireEvent.change(screen.getByRole("textbox"), {
      target: { value: "/mu/" },
    });
    expect((screen.getByRole("textbox") as HTMLInputElement).value).toBe("mu");
    expect(screen.getAllByRole("option").map((o) => o.textContent)).toEqual([
      "music",
    ]);
    expect(screen.queryByText(/Only some folders are shown/)).toBeNull();
  });

  it("says when the folder is gone or the tree cannot be read", async () => {
    fakeServer({ [q("")]: reply(404, '{"error":"no such folder"}') });
    const { unmount } = render(<TreeFolderWidget {...props({ value: "" })} />);
    expect(
      await screen.findByText("There is no folder / in your file tree."),
    ).toBeTruthy();
    expect(screen.getByRole("button", { name: "Use /" })).toHaveProperty(
      "disabled",
      true,
    );
    unmount();
    fakeServer({ [q("x")]: reply(500, '{"error":"disk gone"}') });
    const second = render(<TreeFolderWidget {...props({ value: "x/y" })} />);
    expect(
      await screen.findByText("The file tree could not be read: disk gone"),
    ).toBeTruthy();
    second.unmount();
    fakeServer({ [q("z")]: () => Promise.reject("offline") });
    render(<TreeFolderWidget {...props({ value: "z/y" })} />);
    expect(
      await screen.findByText("The file tree could not be read: offline"),
    ).toBeTruthy();
  });

  it("refuses a folder the service's pattern does not allow", async () => {
    fakeServer({ [q("")]: listing([]) });
    render(
      <TreeFolderWidget
        {...props({ value: "", schema: { pattern: "^/media" } })}
      />,
    );
    const use = await screen.findByRole("button", { name: "Use /" });
    expect(use).toHaveProperty("disabled", true);
    expect(use.title).toBe("This version of the service cannot use /");
  });

  it("cannot be moved around when locked", async () => {
    fakeServer({ [q("a")]: listing(["b"]) });
    render(<TreeFolderWidget {...props({ value: "a/b", disabled: true })} />);
    fireEvent.mouseDown(await screen.findByRole("option", { name: "b" }));
    expect(screen.getByRole("listbox", { name: "Folders in /a" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "/" })).toHaveProperty(
      "disabled",
      true,
    );
    expect(screen.getByRole("textbox")).toHaveProperty("disabled", true);
  });

  it("ignores a listing that arrives after it is gone", async () => {
    let answer!: (r: Response) => void;
    let fail!: (e: Error) => void;
    vi.stubGlobal(
      "fetch",
      vi.fn(
        () =>
          new Promise<Response>((resolve, reject) => {
            answer = resolve;
            fail = reject;
          }),
      ),
    );
    const first = render(<TreeFolderWidget {...props({ value: "" })} />);
    first.unmount();
    await act(async () =>
      answer(new Response(JSON.stringify(listing(["late"])))),
    );
    const second = render(<TreeFolderWidget {...props({ value: "p/q" })} />);
    second.unmount();
    await act(async () => fail(new Error("late")));
    await waitFor(() => expect(screen.queryByText("late")).toBeNull());
  });
});
