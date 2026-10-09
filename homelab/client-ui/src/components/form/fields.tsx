import { useEffect, useState } from "react";
import type { FieldProps } from "@rjsf/utils";
import { Input, Select } from "@/components/ui/input";
import {
  adoptsDefault,
  connectionChoice,
  type Connection,
} from "@/lib/connections";
import { serviceLabel, type ServiceInstance } from "@/lib/services";
import { useApi } from "@/lib/useResource";

const OWN = "__own__";
const TYPED = "__typed__";

interface KeyProp {
  title?: string;
  format?: string;
}

export function ConnectionField(props: FieldProps) {
  const { formData, onChange, schema, idSchema, disabled, readonly } = props;
  const off = disabled || readonly;
  const wants = String(
    (schema as Record<string, unknown>)["x-yolab-requires"] ?? "",
  );
  const installed =
    useApi<ServiceInstance[]>(
      wants ? `services:${wants}` : null,
      `/api/services/${encodeURIComponent(wants)}`,
    ).data ?? [];
  const value: Connection =
    formData && typeof formData === "object" && !Array.isArray(formData)
      ? (formData as Connection)
      : {};
  const canRunOwn = schema.default !== undefined;
  const keys = Object.entries(
    (schema.properties ?? {}) as Record<string, KeyProp>,
  ).filter(([k]) => k !== "from");
  const [typing, setTyping] = useState(false);
  const choice = connectionChoice(value, installed, canRunOwn, typing);
  const adopt = adoptsDefault(value, choice);

  useEffect(() => {
    if (adopt && !off) onChange({ from: adopt });
  }, [adopt, off, onChange]);

  const selected =
    choice.kind === "own"
      ? OWN
      : choice.kind === "typed"
        ? TYPED
        : choice.namespace;

  return (
    <div className="space-y-2">
      <Select
        id={idSchema.$id}
        value={selected}
        disabled={off}
        onChange={(e) => {
          const next = e.target.value;
          setTyping(next === TYPED);
          if (next === OWN) onChange(canRunOwn ? {} : undefined);
          else if (next === TYPED) onChange({ from: "" });
          else onChange({ from: next });
        }}
      >
        {canRunOwn && <option value={OWN}>Run its own</option>}
        {installed.map((s) => (
          <option key={s.namespace} value={s.namespace}>
            Use {serviceLabel(s)}
          </option>
        ))}
        {choice.kind === "missing" && (
          <option value={choice.namespace}>
            {choice.namespace.replace(/^yolab-/, "")} (no longer installed)
          </option>
        )}
        <option value={TYPED}>Another address…</option>
      </Select>
      {choice.kind === "typed" &&
        keys.map(([key, prop]) => (
          <Input
            key={key}
            aria-label={prop.title ?? key}
            placeholder={
              prop.format === "uri" ? "http://192.168.1.20:8080" : prop.title
            }
            type={key.includes("password") ? "password" : "text"}
            value={typeof value[key] === "string" ? (value[key] as string) : ""}
            disabled={off}
            onChange={(e) =>
              onChange({ ...value, from: "", [key]: e.target.value })
            }
            className="font-mono"
          />
        ))}
      {choice.kind === "missing" && (
        <p className="text-sm text-danger">
          The app this one used was removed. Pick another one.
        </p>
      )}
    </div>
  );
}
