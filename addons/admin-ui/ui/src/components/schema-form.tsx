// A form generated from an addon's configuration schema (the subset schemars produces for plain
// configuration structs). Fields it cannot render are edited as JSON.

import { useState } from "react";
import { Field } from "@/components/page";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Input } from "@/components/ui/input";
import { Select } from "@/components/ui/select";
import { Textarea } from "@/components/ui/textarea";
import { type JsonSchema, SECRET, isSecret } from "@/lib/api";
import { splitList } from "@/lib/settings";

type Config = Record<string, unknown>;

/** The schema of a field without its `null` alternative, and whether it may be null. */
export function nonNull(schema: JsonSchema): { schema: JsonSchema; nullable: boolean } {
  if (Array.isArray(schema.type)) {
    const types = schema.type.filter((t) => t !== "null");
    return { schema: { ...schema, type: types.length === 1 ? types[0] : types }, nullable: types.length < schema.type.length };
  }
  const alternatives = schema.anyOf ?? schema.oneOf;
  if (alternatives) {
    const rest = alternatives.filter((s) => s.type !== "null");
    if (rest.length === 1 && rest[0]) return { schema: { ...rest[0], description: schema.description, default: schema.default }, nullable: true };
  }
  return { schema, nullable: false };
}

/** How a field is edited. */
export function fieldKind(schema: JsonSchema): "string" | "number" | "boolean" | "enum" | "strings" | "json" {
  const { schema: s } = nonNull(schema);
  if (s.enum && s.enum.every((v) => typeof v === "string")) return "enum";
  if (s.type === "string") return "string";
  if (s.type === "integer" || s.type === "number") return "number";
  if (s.type === "boolean") return "boolean";
  if (s.type === "array" && s.items && nonNull(s.items).schema.type === "string") return "strings";
  return "json";
}

const label = (name: string) => name.replace(/_/g, " ").replace(/^./, (c) => c.toUpperCase());

function defaultHint(schema: JsonSchema): string | undefined {
  if (schema.default === undefined || schema.default === null) return undefined;
  const d = schema.default;
  if (Array.isArray(d)) return d.length ? `Default: ${d.join(", ")}` : undefined;
  return `Default: ${typeof d === "object" ? JSON.stringify(d) : String(d)}`;
}

function SchemaField({
  name,
  schema,
  required,
  value,
  onChange,
}: {
  name: string;
  schema: JsonSchema;
  required: boolean;
  value: unknown;
  onChange: (v: unknown) => void;
}) {
  const id = `cfg-${name}`;
  const kind = fieldKind(schema);
  const { schema: s } = nonNull(schema);
  const hint = [schema.description, defaultHint(schema)].filter(Boolean).join(" ");
  const title = `${label(name)}${required ? "" : " (optional)"}`;
  if (schema.writeOnly) {
    return <SecretField id={id} title={title} hint={hint} kind={kind} value={value} onChange={onChange} />;
  }
  switch (kind) {
    case "boolean":
      return (
        <div>
          <Checkbox
            label={label(name)}
            checked={typeof value === "boolean" ? value : Boolean(schema.default)}
            onChange={(e) => onChange(e.target.checked)}
          />
          {hint && <p className="ml-[30px] text-xs text-muted-foreground">{hint}</p>}
        </div>
      );
    case "enum":
      return (
        <Field id={id} label={title} hint={hint}>
          <Select id={id} value={typeof value === "string" ? value : ""} onChange={(e) => onChange(e.target.value || undefined)}>
            <option value="">{schema.default ? `Default (${String(schema.default)})` : "Not set"}</option>
            {(s.enum as string[]).map((v) => (
              <option key={v}>{v}</option>
            ))}
          </Select>
        </Field>
      );
    case "number":
      return (
        <Field id={id} label={title} hint={hint}>
          <Input
            id={id}
            type="number"
            className="w-48 font-mono"
            placeholder={schema.default !== undefined ? String(schema.default) : undefined}
            value={typeof value === "number" ? value : ""}
            onChange={(e) => onChange(e.target.value === "" ? undefined : Number(e.target.value))}
          />
        </Field>
      );
    case "strings":
      return (
        <Field id={id} label={title} hint={`${hint} Separate entries with commas.`.trim()}>
          <Input
            id={id}
            className="font-mono"
            value={Array.isArray(value) ? value.join(", ") : ""}
            onChange={(e) => onChange(e.target.value.trim() ? splitList(e.target.value) : undefined)}
          />
        </Field>
      );
    case "string":
      return (
        <Field id={id} label={title} hint={hint}>
          <Input
            id={id}
            className="font-mono"
            placeholder={typeof schema.default === "string" ? schema.default : undefined}
            value={typeof value === "string" ? value : ""}
            onChange={(e) => onChange(e.target.value === "" ? undefined : e.target.value)}
          />
        </Field>
      );
    default:
      return <JsonField id={id} label={title} hint={hint} value={value} onChange={onChange} />;
  }
}

/** A secret: the server never returns it, so a stored one can only be kept, replaced or removed. */
function SecretField({
  id,
  title,
  hint,
  kind,
  value,
  onChange,
}: {
  id: string;
  title: string;
  hint?: string;
  kind: ReturnType<typeof fieldKind>;
  value: unknown;
  onChange: (v: unknown) => void;
}) {
  // Whether a value was stored when the form opened; replacing it can be undone.
  const [stored] = useState(isSecret(value));
  const [replacing, setReplacing] = useState(false);
  if (isSecret(value) && !replacing) {
    return (
      <Field id={id} label={title} hint={hint}>
        <div className="flex items-center gap-2">
          <Badge variant="outline">Set · hidden</Badge>
          <Button id={id} type="button" variant="outline" onClick={() => setReplacing(true)}>
            Replace
          </Button>
          <Button type="button" variant="ghost" onClick={() => onChange(undefined)}>
            Remove
          </Button>
        </div>
      </Field>
    );
  }
  const keep = stored && (
    <Button
      type="button"
      variant="ghost"
      onClick={() => {
        onChange(SECRET);
        setReplacing(false);
      }}
    >
      Keep the stored one
    </Button>
  );
  if (kind === "string") {
    return (
      <Field id={id} label={title} hint={hint}>
        <div className="flex items-center gap-2">
          <Input
            id={id}
            type="password"
            autoComplete="new-password"
            className="font-mono"
            value={typeof value === "string" ? value : ""}
            onChange={(e) => onChange(e.target.value === "" ? (stored ? SECRET : undefined) : e.target.value)}
          />
          {keep}
        </div>
      </Field>
    );
  }
  return (
    <div className="space-y-2">
      <JsonField id={id} label={title} hint={hint} value={isSecret(value) ? undefined : value} onChange={(v) => onChange(v ?? (stored ? SECRET : undefined))} />
      {keep}
    </div>
  );
}

function JsonField({
  id,
  label,
  hint,
  value,
  onChange,
}: {
  id: string;
  label: string;
  hint?: string;
  value: unknown;
  onChange: (v: unknown) => void;
}) {
  return (
    <Field id={id} label={`${label} (JSON)`} hint={hint}>
      <Textarea
        id={id}
        className="min-h-32 font-mono text-xs"
        defaultValue={value === undefined ? "" : JSON.stringify(value, null, 2)}
        onChange={(e) => {
          if (!e.target.value.trim()) return onChange(undefined);
          try {
            onChange(JSON.parse(e.target.value));
            e.target.setCustomValidity("");
          } catch {
            e.target.setCustomValidity("Not valid JSON.");
          }
        }}
      />
    </Field>
  );
}

/** Edits `value` (an addon's configuration object) by `schema`. Unset fields are left out, so the
 * addon's defaults apply. */
export function SchemaForm({ schema, value, onChange }: { schema: JsonSchema; value: Config; onChange: (v: Config) => void }) {
  const properties = Object.entries(schema.properties ?? {});
  if (!properties.length) {
    return <JsonField id="cfg-json" label="Configuration" value={value} onChange={(v) => onChange((v ?? {}) as Config)} />;
  }
  return (
    <div className="space-y-4">
      {properties.map(([name, field]) => (
        <SchemaField
          key={name}
          name={name}
          schema={field}
          required={schema.required?.includes(name) ?? false}
          value={value[name]}
          onChange={(v) => {
            const next = { ...value };
            if (v === undefined) delete next[name];
            else next[name] = v;
            onChange(next);
          }}
        />
      ))}
    </div>
  );
}
