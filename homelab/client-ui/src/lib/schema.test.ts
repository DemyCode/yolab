import { describe, expect, it } from "vitest";
import {
  addressField,
  configSchemaOf,
  generatedFields,
  revealedBy,
  uiSchemaFor,
  type ConfigSchema,
} from "./schema";

const codeServer: ConfigSchema = {
  type: "object",
  properties: {
    subdomain: { type: "string", format: "tunnel", default: "code-server" },
    password: {
      type: "string",
      title: "Access password",
      writeOnly: true,
      generate: true,
    },
    pin: { type: "string", title: "PIN", writeOnly: true },
    storage_size: { type: "string" },
  },
};

const withLogins: ConfigSchema = {
  type: "object",
  properties: {
    subdomain: { type: "string", format: "tunnel" },
    auth_enabled: { type: "boolean", default: false },
    file_explorer_enabled: { type: "boolean", default: true },
  },
  dependencies: {
    auth_enabled: {
      oneOf: [
        { properties: { auth_enabled: { type: "boolean" } } },
        {
          properties: {
            auth_enabled: { type: "boolean" },
            auth_users: {
              type: "array",
              items: {
                type: "object",
                properties: {
                  username: { type: "string" },
                  password: { type: "string", writeOnly: true, generate: true },
                },
              },
            } as never,
          },
        },
      ],
    },
  },
};

describe("configSchemaOf", () => {
  it("takes the config part of a whole values schema", () => {
    expect(
      configSchemaOf({ properties: { config: codeServer, yolab: {} } }),
    ).toBe(codeServer);
  });

  it("takes a config schema as it is", () => {
    expect(configSchemaOf(codeServer)).toBe(codeServer);
  });

  it("gives an empty form for a chart with no schema", () => {
    expect(configSchemaOf(undefined)).toEqual({});
  });
});

describe("uiSchemaFor", () => {
  it("renders the address field with the box's domain", () => {
    expect(uiSchemaFor(codeServer, "box.yolab.io").subdomain).toEqual({
      "ui:widget": "TunnelWidget",
      "ui:options": { domain: "box.yolab.io" },
    });
  });

  it("offers to generate a credential only when the schema asks for it", () => {
    const ui = uiSchemaFor(codeServer, "");
    expect(ui.password).toEqual({
      "ui:widget": "PasswordWidget",
      "ui:options": { generate: true },
    });
    expect(ui.pin).toEqual({
      "ui:widget": "PasswordWidget",
      "ui:options": { generate: false },
    });
  });

  it("keeps the fields in the order the developer wrote them", () => {
    expect(uiSchemaFor(codeServer, "")["ui:order"]).toEqual([
      "subdomain",
      "password",
      "pin",
      "storage_size",
      "*",
    ]);
  });

  it("puts the fields a toggle reveals right under that toggle", () => {
    const ui = uiSchemaFor(withLogins, "");
    expect(ui["ui:order"]).toEqual([
      "subdomain",
      "auth_enabled",
      "auth_users",
      "file_explorer_enabled",
      "*",
    ]);
    expect(ui.auth_users).toMatchObject({ "ui:options": { attached: true } });
  });

  it("masks credentials inside a list, such as each login's password", () => {
    expect(uiSchemaFor(withLogins, "").auth_users).toMatchObject({
      items: {
        password: {
          "ui:widget": "PasswordWidget",
          "ui:options": { generate: true },
        },
      },
    });
  });
});

describe("revealedBy", () => {
  it("finds the fields each toggle brings in", () => {
    expect(revealedBy(withLogins)).toEqual(
      new Map([["auth_enabled", ["auth_users"]]]),
    );
  });

  it("finds nothing in a schema without conditions", () => {
    expect(revealedBy(codeServer).size).toBe(0);
  });
});

describe("generatedFields", () => {
  it("lists the credentials YoLab generates, not the ones you type", () => {
    expect(generatedFields(codeServer)).toEqual([
      ["password", "Access password"],
    ]);
  });
});

describe("addressField", () => {
  it("is the field whose format is tunnel", () => {
    expect(addressField(codeServer)).toBe("subdomain");
    expect(addressField({ properties: {} })).toBeUndefined();
  });
});
