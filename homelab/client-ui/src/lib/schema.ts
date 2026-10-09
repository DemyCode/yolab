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
  "x-yolab-service"?: string;
  properties?: Record<string, SchemaProp>;
}

interface Branch {
  properties?: Record<string, SchemaProp>;
  dependencies?: Dependencies;
}

type Dependencies = Record<string, { oneOf?: Branch[]; properties?: unknown }>;

export interface ConfigSchema {
  type?: string;
  title?: string;
  properties?: Record<string, SchemaProp>;
  required?: string[];
  dependencies?: Dependencies;
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

function switches(
  dependencies: Dependencies | undefined,
): [string, { oneOf?: Branch[] }][] {
  return Object.entries(dependencies ?? {}).flatMap(([toggle, dep]) => [
    [toggle, dep] as [string, { oneOf?: Branch[] }],
    ...(dep.oneOf ?? []).flatMap((branch) => switches(branch.dependencies)),
  ]);
}

export function revealedBy(schema: ConfigSchema): Map<string, string[]> {
  const revealed = new Map<string, string[]>();
  for (const [toggle, dep] of switches(schema.dependencies)) {
    const fields = new Set<string>();
    for (const branch of dep.oneOf ?? []) {
      for (const name of Object.keys(branch.properties ?? {})) {
        if (name !== toggle && !(name in (schema.properties ?? {}))) {
          fields.add(name);
        }
      }
    }
    if (fields.size > 0) {
      const before = revealed.get(toggle) ?? [];
      revealed.set(toggle, [...new Set([...before, ...fields])]);
    }
  }
  return revealed;
}

function fieldUi(prop: SchemaProp, domain: string): UiSchema | undefined {
  if (prop.format === "tunnel") {
    return { "ui:widget": "TunnelWidget", "ui:options": { domain } };
  }
  if (prop.format === "service-url") {
    return {
      "ui:widget": "ServiceUrlWidget",
      "ui:options": { service: prop["x-yolab-service"] ?? "" },
    };
  }
  if (prop.format === "folder") {
    return { "ui:widget": "FolderWidget" };
  }
  if (prop.format === "yolab-token") {
    return { "ui:widget": "YolabTokenWidget" };
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

  const reveal = (toggle: string) => {
    for (const dependent of revealed.get(toggle) ?? []) {
      if (order.includes(dependent)) continue;
      order.push(dependent);
      const prop = branchProp(schema, toggle, dependent);
      if (prop) describe(dependent, prop, true);
      reveal(dependent);
    }
  };

  for (const [name, prop] of Object.entries(schema.properties ?? {})) {
    order.push(name);
    describe(name, prop, false);
    reveal(name);
  }
  ui["ui:order"] = [...order, "*"];
  return ui;
}

function branchProp(
  schema: ConfigSchema,
  toggle: string,
  name: string,
): SchemaProp | undefined {
  for (const [switched, dep] of switches(schema.dependencies)) {
    if (switched !== toggle) continue;
    for (const branch of dep.oneOf ?? []) {
      const prop = branch.properties?.[name];
      if (prop) return prop;
    }
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
  const top = Object.entries(schema.properties ?? {}).find(
    ([, p]) => p.format === "tunnel",
  )?.[0];
  if (top) return top;
  const switched = schema.dependencies?.yolab_enabled?.oneOf ?? [];
  for (const branch of switched) {
    const found = Object.entries(branch.properties ?? {}).find(
      ([, p]) => p.format === "tunnel",
    )?.[0];
    if (found) return found;
  }
  return undefined;
}

export function chosenBranchDefaults(
  schema: ConfigSchema,
  values: Record<string, unknown>,
): Record<string, unknown> {
  const defaults: Record<string, unknown> = {};
  const seen = { ...values };
  const fill = (dependencies: Dependencies | undefined) => {
    for (const [toggle, dep] of Object.entries(dependencies ?? {})) {
      const branch = (dep.oneOf ?? []).find(
        (b) => b.properties?.[toggle]?.const === seen[toggle],
      );
      for (const [name, prop] of Object.entries(branch?.properties ?? {})) {
        if (name === toggle || prop.writeOnly || prop.default === undefined)
          continue;
        if (seen[name] === undefined) {
          defaults[name] = prop.default;
          seen[name] = prop.default;
        }
      }
      fill(branch?.dependencies);
    }
  };
  fill(schema.dependencies);
  return defaults;
}
