import type {
  FieldTemplateProps,
  ObjectFieldTemplateProps,
} from "@rjsf/utils";

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
  const { id, label, help, description, errors, children, schema, hidden } =
    props;

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
      {description}
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
  return (
    <div className="space-y-5">
      {props.properties.map((p) => (
        <div key={p.name}>{p.content}</div>
      ))}
    </div>
  );
}

export const templates = { FieldTemplate, ObjectFieldTemplate };
