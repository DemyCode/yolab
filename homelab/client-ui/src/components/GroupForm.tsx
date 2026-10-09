import { useMemo } from "react";
import Form from "@rjsf/core";
import type { RJSFSchema } from "@rjsf/utils";
import validator from "@rjsf/validator-ajv8";
import { fields, templates, widgets } from "@/components/form/registry";
import { Card } from "@/components/ui/card";
import { configSchemaOf, uiSchemaFor } from "@/lib/schema";
import { useApi } from "@/lib/useResource";

export function GroupForm({
  schema,
  formData,
  onChange,
}: {
  schema: object | null | undefined;
  formData: Record<string, unknown>;
  onChange: (next: Record<string, unknown>) => void;
}) {
  const domain = useApi<{ domain: string }>("domain", "/api/tunnel/domain");
  const config = useMemo(
    () => configSchemaOf(schema ?? undefined),
    [schema],
  );
  const rjsfSchema = useMemo(
    () => ({ type: "object", ...config }) as RJSFSchema,
    [config],
  );
  const uiSchema = useMemo(
    () => uiSchemaFor(config, domain.data?.domain ?? ""),
    [config, domain.data?.domain],
  );

  return (
    <Card className="p-5">
      <Form
        schema={rjsfSchema}
        uiSchema={uiSchema}
        formData={formData}
        formContext={{ formData }}
        validator={validator}
        widgets={widgets}
        fields={fields}
        templates={templates}
        liveValidate={false}
        showErrorList={false}
        onChange={(e) => onChange(e.formData ?? {})}
      >
        <></>
      </Form>
    </Card>
  );
}
