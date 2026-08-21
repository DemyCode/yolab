import type {
  ArrayFieldTemplateProps,
  FieldTemplateProps,
  ObjectFieldTemplateProps,
} from "@rjsf/utils";
import { Plus, X } from "lucide-react";

/**
 * Templates that keep an RJSF form looking like the rest of the product.
 *
 * The objection to RJSF was that a generated form is "definitionally
 * schema-shaped" — field names, types, validation messages. That is only true
 * of the default templates. Templates are the supported way to say how a form
 * looks, so the argument was really against RJSF's defaults, not RJSF.
 */

/**
 * One field: label, help text, control, error.
 *
 * Booleans render bare, because CheckboxWidget is a switch that carries its own
 * label and help — wrapping it would print both twice.
 */
export function FieldTemplate(props: FieldTemplateProps) {
  // `description` is RJSF's own rendered DescriptionField. It is deliberately
  // NOT used: schema.description is rendered directly below, and taking both
  // printed every help line twice — "How much CephFS storage to allocate"
  // appearing under itself.
  const { id, label, help, errors, children, schema, hidden } = props;

  if (hidden) return null;
  if (schema.type === "boolean") return <div className="py-1">{children}</div>;

  return (
    <div className="space-y-1.5">
      {label && (
        <label htmlFor={id} className="block text-sm font-medium text-fg">
          {label}
        </label>
      )}
      {schema.description && (
        <p className="text-sm text-fg-muted">{schema.description}</p>
      )}
      {children}
      {errors}
      {help}
    </div>
  );
}

/**
 * A repeating field, as an actual list.
 *
 * RJSF handles arrays natively — add, remove, reorder — and this only supplies
 * the presentation. It matters for anything genuinely plural: a set of logins
 * is a list of people, not a textarea someone has to format correctly, and a
 * typo in "user:password" should not be a silent failure at container start.
 */
export function ArrayFieldTemplate(props: ArrayFieldTemplateProps) {
  // No title and no description here. An array field is still wrapped by
  // FieldTemplate, which already prints both — rendering them again showed
  // "Logins / Who can open this app…" twice, one block under the other.
  const { items, canAdd, onAddClick } = props;

  return (
    <div className="space-y-3">
      {items.length === 0 && (
        <p className="text-sm text-fg-subtle">Nothing added yet.</p>
      )}

      {items.map((el) => (
        <div
          key={el.key}
          className="flex items-start gap-2 rounded-lg border border-border p-3"
        >
          <div className="min-w-0 flex-1">{el.children}</div>
          {el.hasRemove && (
            <button
              type="button"
              aria-label="Remove"
              onClick={el.onDropIndexClick(el.index)}
              className="mt-1 rounded-md p-2 text-fg-muted hover:bg-surface-2 hover:text-danger"
            >
              <X className="h-4 w-4" />
            </button>
          )}
        </div>
      ))}

      {canAdd && (
        <button
          type="button"
          onClick={onAddClick}
          className="flex items-center gap-2 rounded-lg border border-dashed border-border px-3 py-2 text-sm text-fg-muted hover:border-primary hover:text-fg"
        >
          <Plus className="h-4 w-4" />
          Add
        </button>
      )}
    </div>
  );
}

/**
 * The object wrapper: just the fields, spaced.
 *
 * No title, no description, no `<fieldset>` — the page already says which app
 * is being installed, and RJSF's default would repeat it above every group.
 *
 * Crucially: no collapsing. Every option the chart exposes stays on the page.
 * An app like Minecraft is nothing but these choices — creative or survival, a
 * seed, who is whitelisted — so folding them behind a second click hides the
 * entire reason someone opened the form.
 */
export function ObjectFieldTemplate(props: ObjectFieldTemplateProps) {
  const ui = (props.uiSchema ?? {}) as Record<
    string,
    { "ui:options"?: { attached?: boolean } }
  >;

  return (
    <div className="space-y-5">
      {props.properties.map((p) => {
        // A field marked `attached` belongs to the one above it — Logins only
        // exists because "Add login" is on. Rendered as a plain sibling it read
        // as an unrelated question two rows down; the rule and the indent say
        // "this is part of that" without needing a heading to explain it.
        const attached = ui[p.name]?.["ui:options"]?.attached;
        return attached ? (
          <div
            key={p.name}
            className="-mt-2 ml-1 border-l-2 border-border pl-4"
          >
            {p.content}
          </div>
        ) : (
          <div key={p.name}>{p.content}</div>
        );
      })}
    </div>
  );
}

export const templates = {
  FieldTemplate,
  ObjectFieldTemplate,
  ArrayFieldTemplate,
};
