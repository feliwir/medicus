import { Field } from "@/components/page";
import { SectionScreen } from "@/components/section-screen";
import { Badge } from "@/components/ui/badge";
import { Checkbox } from "@/components/ui/checkbox";
import { Input } from "@/components/ui/input";
import { type PrincipalItem, type RoleItem, validAeTitle } from "@/lib/items";
import { splitList, useSection } from "@/lib/settings";

interface PrincipalForm {
  callingAe: string;
  networks: string;
  roles: string[];
}

function PrincipalFields({ value, onChange }: { value: PrincipalForm; onChange: (f: PrincipalForm) => void }) {
  const roles = useSection<RoleItem>("roles");
  const names = ["admin", ...Object.keys(roles.data ?? {})];
  return (
    <>
      <div className="grid gap-4 sm:grid-cols-2">
        <Field id="p-ae" label="Calling AE title" hint="DIMSE peers calling with it are this principal.">
          <Input
            id="p-ae"
            className="font-mono"
            value={value.callingAe}
            onChange={(e) => onChange({ ...value, callingAe: e.target.value.toUpperCase() })}
          />
        </Field>
        <Field id="p-net" label="From networks" hint="e.g. 10.0.0.0/24; empty: anywhere.">
          <Input id="p-net" className="font-mono" value={value.networks} onChange={(e) => onChange({ ...value, networks: e.target.value })} />
        </Field>
      </div>
      <div className="space-y-1">
        <div className="text-sm font-medium">Roles</div>
        <div className="flex flex-wrap gap-x-6">
          {names.map((r) => (
            <Checkbox
              key={r}
              label={r}
              checked={value.roles.includes(r)}
              onChange={(e) =>
                onChange({ ...value, roles: e.target.checked ? [...value.roles, r] : value.roles.filter((x) => x !== r) })
              }
            />
          ))}
        </div>
      </div>
    </>
  );
}

export function Principals() {
  return (
    <SectionScreen<PrincipalItem, PrincipalForm>
      section="principals"
      title="Principals"
      singular="principal"
      description="Callers that are not user accounts, such as modalities, recognized by their calling AE title and network, with the roles they get."
      columns={[
        { accessorKey: "name", header: "Name", cell: ({ row }) => <span className="font-medium">{row.original.name}</span> },
        {
          id: "dimse",
          header: "Recognized by",
          accessorFn: (e) => e.item.dimse?.calling_ae ?? "",
          cell: ({ row }) =>
            row.original.item.dimse ? (
              <span className="font-mono">
                {row.original.item.dimse.calling_ae}
                {row.original.item.dimse.networks.length > 0 && (
                  <span className="text-muted-foreground"> from {row.original.item.dimse.networks.join(", ")}</span>
                )}
              </span>
            ) : (
              <span className="text-muted-foreground">name only (trusted proxy)</span>
            ),
        },
        {
          id: "roles",
          header: "Roles",
          enableSorting: false,
          cell: ({ row }) => (
            <div className="flex flex-wrap gap-1">
              {row.original.item.roles.map((r) => (
                <Badge key={r}>{r}</Badge>
              ))}
            </div>
          ),
        },
      ]}
      blank={() => ({ callingAe: "", networks: "", roles: [] })}
      toForm={(p) => ({ callingAe: p.dimse?.calling_ae ?? "", networks: (p.dimse?.networks ?? []).join(", "), roles: p.roles })}
      fromForm={(f) => ({
        dimse: f.callingAe.trim() ? { calling_ae: f.callingAe.trim(), networks: splitList(f.networks) } : null,
        roles: f.roles,
      })}
      validate={(f) =>
        f.callingAe && !validAeTitle(f.callingAe.trim())
          ? "AE titles have 1 to 16 characters, without backslash."
          : !f.callingAe.trim() && f.networks.trim()
            ? "Networks apply to a calling AE title."
            : undefined
      }
      Form={PrincipalFields}
    />
  );
}
