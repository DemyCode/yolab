import { useEffect, useState } from "react";
import type { FieldProps } from "@rjsf/utils";
import { Input, Select } from "@/components/ui/input";
import {
  adoptsDefault,
  boxesOf,
  FROM,
  linkChoice,
  linkLabel,
  linksQuery,
  missingLabel,
  type LinkCandidate,
  type LinkValue,
} from "@/lib/links";
import type { SchemaProp } from "@/lib/schema";
import { useApi } from "@/lib/useResource";

const OWN = "__own__";
const TYPED = "__typed__";

export function LinkField(props: FieldProps) {
  const { formData, onChange, schema, idSchema, disabled, readonly } = props;
  const off = disabled || readonly;
  const prop = schema as unknown as SchemaProp;
  const query = linksQuery(prop);
  const found = useApi<LinkCandidate[]>(
    query ? `links:${query}` : null,
    query ?? "",
  );
  const candidates = found.data ?? [];
  const value: LinkValue =
    formData && typeof formData === "object" && !Array.isArray(formData)
      ? (formData as LinkValue)
      : {};
  const canRunOwn = schema.default !== undefined;
  const boxes = boxesOf(prop);
  const [typing, setTyping] = useState(false);
  const choice = linkChoice(value, candidates, canRunOwn, typing);
  const adopt = adoptsDefault(value, choice);

  useEffect(() => {
    if (adopt && !off) onChange({ [FROM]: adopt });
  }, [adopt, off, onChange]);

  const selected =
    choice.kind === "own"
      ? OWN
      : choice.kind === "typed"
        ? TYPED
        : choice.reference;

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
          else if (next === TYPED) onChange({});
          else onChange({ [FROM]: next });
        }}
      >
        {canRunOwn && <option value={OWN}>Run its own</option>}
        {candidates.map((c) => (
          <option key={c.reference} value={c.reference}>
            Use {linkLabel(c)}
          </option>
        ))}
        {choice.kind === "missing" && (
          <option value={choice.reference}>
            {missingLabel(choice.reference)} (no longer installed)
          </option>
        )}
        <option value={TYPED}>Type it in…</option>
      </Select>
      {choice.kind === "typed" &&
        boxes.map(([key, box]) => (
          <Input
            key={key}
            aria-label={box.title ?? key}
            placeholder={
              box.format === "uri" ? "http://192.168.1.20:8080" : box.title
            }
            type={
              key.includes("password") || key.includes("key")
                ? "password"
                : "text"
            }
            value={typeof value[key] === "string" ? (value[key] as string) : ""}
            disabled={off}
            onChange={(e) => {
              const next: LinkValue = { ...value, [key]: e.target.value };
              delete next[FROM];
              delete next.from;
              onChange(next);
            }}
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
