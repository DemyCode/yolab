import type {
  ArrayFieldTemplateProps,
  FieldTemplateProps,
  ObjectFieldTemplateProps,
} from "@rjsf/utils";
import { Plus, X } from "lucide-react";
import { showIfMet, type ShowIf } from "@/lib/form";

export function FieldTemplate(props: FieldTemplateProps) {
  const {
    id,
    label,
    help,
    errors,
    children,
    schema,
    hidden,
    uiSchema,
    formContext,
  } = props;

  if (hidden) return null;

  const showIf = (uiSchema?.["ui:options"] as { showIf?: ShowIf } | undefined)
    ?.showIf;
  if (showIf) {
    const data =
      (formContext as { formData?: Record<string, unknown> } | undefined)
        ?.formData ?? {};
    if (!showIfMet(showIf, data)) return null;
  }

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

export function ArrayFieldTemplate(props: ArrayFieldTemplateProps) {
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

export function ObjectFieldTemplate(props: ObjectFieldTemplateProps) {
  const ui = (props.uiSchema ?? {}) as Record<
    string,
    { "ui:options"?: { attached?: boolean } }
  >;

  return (
    <div className="space-y-5">
      {props.properties.map((p) => {
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
