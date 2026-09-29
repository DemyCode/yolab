export interface SchemaProp {
  type?: string;
  title?: string;
  default?: unknown;
  const?: unknown;
  description?: string;
  format?: string;
  enum?: string[];
  minLength?: number;
  maxLength?: number;
  writeOnly?: boolean;
  generate?: boolean;
  properties?: Record<string, SchemaProp>;
}

interface Branch {
  properties?: Record<string, SchemaProp>;
}

export interface ConfigSchema {
  type?: string;
  title?: string;
  properties?: Record<string, SchemaProp>;
  required?: string[];
  dependencies?: Record<string, { oneOf?: Branch[]; properties?: unknown }>;
}

export type UiSchema = Record<string, unknown>;

export function configSchemaOf(schema: object | undefined): ConfigSchema {
  if (!schema) return {};
  const s = schema as ConfigSchema & { properties?: { config?: ConfigSchema } };
  const nested = s.properties?.config;
  if (nested && typeof nested === "object" && "properties" in nested) {
    return nested;
  }
  return s.properties ? s : {};
}

export function revealedBy(schema: ConfigSchema): Map<string, string[]> {
  const revealed = new Map<string, string[]>();
  for (const [toggle, dep] of Object.entries(schema.dependencies ?? {})) {
    const fields = new Set<string>();
    for (const branch of dep.oneOf ?? []) {
      for (const name of Object.keys(branch.properties ?? {})) {
        if (name !== toggle && !(name in (schema.properties ?? {}))) {
          fields.add(name);
        }
      }
    }
    if (fields.size > 0) revealed.set(toggle, [...fields]);
  }
  return revealed;
}

function fieldUi(prop: SchemaProp, domain: string): UiSchema | undefined {
  if (prop.format === "tunnel") {
    return { "ui:widget": "TunnelWidget", "ui:options": { domain } };
  }
  if (prop.writeOnly) {
    return {
      "ui:widget": "PasswordWidget",
      "ui:options": { generate: prop.generate === true },
    };
  }
  return undefined;
}

function nestedUi(prop: SchemaProp, domain: string): UiSchema {
  const ui: UiSchema = {};
  const items = (prop as { items?: SchemaProp }).items;
  for (const [name, child] of Object.entries(items?.properties ?? {})) {
    const childUi = fieldUi(child, domain);
    if (childUi) ui[name] = childUi;
  }
  return Object.keys(ui).length > 0 ? { items: ui } : {};
}

export function uiSchemaFor(schema: ConfigSchema, domain: string): UiSchema {
  const ui: UiSchema = {};
  const order: string[] = [];
  const revealed = revealedBy(schema);

  const describe = (name: string, prop: SchemaProp, attached: boolean) => {
    const entry: UiSchema = {
      ...(fieldUi(prop, domain) ?? {}),
      ...nestedUi(prop, domain),
    };
    if (attached) {
      entry["ui:options"] = {
        ...((entry["ui:options"] as object) ?? {}),
        attached: true,
      };
    }
    if (Object.keys(entry).length > 0) ui[name] = entry;
  };

  for (const [name, prop] of Object.entries(schema.properties ?? {})) {
    order.push(name);
    describe(name, prop, false);
    for (const dependent of revealed.get(name) ?? []) {
      order.push(dependent);
      const prop = branchProp(schema, name, dependent);
      if (prop) describe(dependent, prop, true);
    }
  }
  ui["ui:order"] = [...order, "*"];
  return ui;
}

function branchProp(
  schema: ConfigSchema,
  toggle: string,
  name: string,
): SchemaProp | undefined {
  for (const branch of schema.dependencies?.[toggle]?.oneOf ?? []) {
    const prop = branch.properties?.[name];
    if (prop) return prop;
  }
  return undefined;
}

export function revealedFields(schema: ConfigSchema): [string, SchemaProp][] {
  return [...revealedBy(schema)].flatMap(([toggle, names]) =>
    names.flatMap((name): [string, SchemaProp][] => {
      const prop = branchProp(schema, toggle, name);
      return prop ? [[name, prop]] : [];
    }),
  );
}

export function generatedFields(schema: ConfigSchema): [string, string][] {
  return Object.entries(schema.properties ?? {})
    .filter(([, p]) => p.writeOnly === true && p.generate === true)
    .map(([name, p]) => [name, p.title ?? name]);
}

export function addressField(schema: ConfigSchema): string | undefined {
  return Object.entries(schema.properties ?? {}).find(
    ([, p]) => p.format === "tunnel",
  )?.[0];
}
