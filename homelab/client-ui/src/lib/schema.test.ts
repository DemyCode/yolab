import { describe, expect, it } from "vitest";
import {
  addressField,
  chosenBranchDefaults,
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
  it("renders the address field with the server's domain", () => {
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

const switched: ConfigSchema = {
  type: "object",
  properties: {
    yolab_enabled: { type: "boolean", default: true },
    storage_size: { type: "string" },
  },
  dependencies: {
    yolab_enabled: {
      oneOf: [
        { properties: { yolab_enabled: { const: false } } },
        {
          properties: {
            yolab_enabled: { const: true },
            subdomain: {
              type: "string",
              format: "tunnel",
              default: "jellyfin",
            },
            yolab_token: {
              type: "string",
              format: "yolab-token",
              writeOnly: true,
            },
          },
        },
      ],
    },
  },
};

describe("the YoLab address switch", () => {
  it("finds the subdomain behind the switch", () => {
    expect(addressField(switched)).toBe("subdomain");
  });

  it("shows the subdomain and token right under the switch", () => {
    const order = uiSchemaFor(switched, "6.yolab.io")["ui:order"] as string[];
    expect(order.slice(0, 3)).toEqual([
      "yolab_enabled",
      "subdomain",
      "yolab_token",
    ]);
  });

  it("gives the token the box-fills-it-in widget, not a password box", () => {
    const ui = uiSchemaFor(switched, "6.yolab.io") as Record<
      string,
      Record<string, unknown>
    >;
    expect(ui.yolab_token["ui:widget"]).toBe("YolabTokenWidget");
    expect(ui.subdomain["ui:widget"]).toBe("TunnelWidget");
  });

  it("prefills the branch the switch selects, without touching what is set", () => {
    expect(chosenBranchDefaults(switched, { yolab_enabled: true })).toEqual({
      subdomain: "jellyfin",
    });
    expect(
      chosenBranchDefaults(switched, {
        yolab_enabled: true,
        subdomain: "films",
      }),
    ).toEqual({});
    expect(chosenBranchDefaults(switched, { yolab_enabled: false })).toEqual(
      {},
    );
  });
});

const explorer: ConfigSchema = {
  type: "object",
  properties: {
    file_explorer_enabled: { type: "boolean", default: true },
    storage_size: { type: "string" },
  },
  dependencies: {
    file_explorer_enabled: {
      oneOf: [
        { properties: { file_explorer_enabled: { const: false } } },
        {
          properties: {
            file_explorer_enabled: { const: true },
            file_explorer_yolab_enabled: { type: "boolean", default: true },
            file_explorer_tailscale_enabled: {
              type: "boolean",
              default: false,
            },
            file_explorer_password: {
              type: "string",
              writeOnly: true,
              generate: true,
            },
          },
          dependencies: {
            file_explorer_yolab_enabled: {
              oneOf: [
                {
                  properties: { file_explorer_yolab_enabled: { const: false } },
                },
                {
                  properties: {
                    file_explorer_yolab_enabled: { const: true },
                    file_explorer_subdomain: {
                      type: "string",
                      format: "tunnel",
                      default: "app-files",
                    },
                  },
                },
              ],
            },
            file_explorer_tailscale_enabled: {
              oneOf: [
                {
                  properties: {
                    file_explorer_tailscale_enabled: { const: false },
                  },
                },
                {
                  properties: {
                    file_explorer_tailscale_enabled: { const: true },
                    file_explorer_tailscale_auth_key: {
                      type: "string",
                      writeOnly: true,
                    },
                  },
                },
              ],
            },
          },
        },
      ],
    },
  },
};

describe("the file explorer's own ways in", () => {
  it("places each nested switch's fields right under it, inside the explorer", () => {
    const order = uiSchemaFor(explorer, "6.yolab.io")["ui:order"] as string[];
    expect(order).toEqual([
      "file_explorer_enabled",
      "file_explorer_yolab_enabled",
      "file_explorer_subdomain",
      "file_explorer_tailscale_enabled",
      "file_explorer_tailscale_auth_key",
      "file_explorer_password",
      "storage_size",
      "*",
    ]);
  });

  it("gives nested fields their widgets and attaches them", () => {
    const ui = uiSchemaFor(explorer, "6.yolab.io") as Record<
      string,
      Record<string, unknown>
    >;
    expect(ui.file_explorer_subdomain["ui:widget"]).toBe("TunnelWidget");
    expect(ui.file_explorer_tailscale_auth_key["ui:widget"]).toBe(
      "PasswordWidget",
    );
    expect(ui.file_explorer_tailscale_auth_key["ui:options"]).toMatchObject({
      attached: true,
    });
  });

  it("prefills the explorer's switches and then the branches they select", () => {
    expect(
      chosenBranchDefaults(explorer, { file_explorer_enabled: true }),
    ).toEqual({
      file_explorer_yolab_enabled: true,
      file_explorer_tailscale_enabled: false,
      file_explorer_subdomain: "app-files",
    });
  });

  it("prefills nothing of the explorer while it is off", () => {
    expect(
      chosenBranchDefaults(explorer, { file_explorer_enabled: false }),
    ).toEqual({});
  });

  it("does not take the explorer's address for the app's own", () => {
    expect(addressField(explorer)).toBeUndefined();
  });
});

describe("a link to another app's service", () => {
  it("gets the installed-or-URL widget, told which service to look for", () => {
    const ui = uiSchemaFor(
      {
        properties: {
          ollama_url: {
            type: "string",
            format: "service-url",
            "x-yolab-service": "ollama",
          },
        },
      },
      "6.yolab.io",
    ) as Record<string, Record<string, unknown>>;
    expect(ui.ollama_url["ui:widget"]).toBe("ServiceUrlWidget");
    expect(ui.ollama_url["ui:options"]).toEqual({ service: "ollama" });
  });
});
