import { Plus, X } from "lucide-react";
import { SectionScreen } from "@/components/section-screen";
import { Alert } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { ADMIN_PERMISSIONS, DATA_PERMISSIONS, type Grant, type NodeItem, type RoleItem } from "@/lib/items";
import { useSection } from "@/lib/settings";

function GrantEditor({
  grant,
  nodes,
  onChange,
  onRemove,
}: {
  grant: Grant;
  nodes: string[];
  onChange: (g: Grant) => void;
  onRemove: () => void;
}) {
  const all = grant.nodes.includes("*");
  const toggle = (list: string[], value: string, on: boolean) =>
    on ? [...list.filter((v) => v !== value), value] : list.filter((v) => v !== value);
  return (
    <fieldset className="relative space-y-3 rounded-md border p-4">
      <Button type="button" variant="ghost" size="icon" className="absolute top-2 right-2" aria-label="Remove grant" onClick={onRemove}>
        <X />
      </Button>
      <legend className="px-1 text-sm font-medium">On</legend>
      <div className="flex flex-wrap gap-x-6">
        <Checkbox
          label="All nodes"
          checked={all}
          onChange={(e) => onChange({ ...grant, nodes: e.target.checked ? ["*"] : [] })}
        />
        {!all &&
          nodes.map((n) => (
            <Checkbox
              key={n}
              label={n}
              checked={grant.nodes.includes(n)}
              onChange={(e) => onChange({ ...grant, nodes: toggle(grant.nodes, n, e.target.checked) })}
            />
          ))}
      </div>
      <div className="text-sm font-medium">May</div>
      <div className="flex flex-wrap gap-x-6">
        {DATA_PERMISSIONS.map((p) => (
          <Checkbox
            key={p}
            label={p}
            checked={grant.permissions.includes(p)}
            onChange={(e) => onChange({ ...grant, permissions: toggle(grant.permissions, p, e.target.checked) })}
          />
        ))}
      </div>
      <div className="flex flex-wrap gap-x-6">
        {ADMIN_PERMISSIONS.map((p) => (
          <Checkbox
            key={p}
            label={p}
            disabled={!all}
            title={all ? undefined : "Admin permissions are granted on all nodes only."}
            checked={grant.permissions.includes(p)}
            onChange={(e) => onChange({ ...grant, permissions: toggle(grant.permissions, p, e.target.checked) })}
          />
        ))}
      </div>
    </fieldset>
  );
}

function RoleForm({ value, onChange }: { value: RoleItem; onChange: (r: RoleItem) => void }) {
  const nodes = useSection<NodeItem>("nodes");
  const names = Object.keys(nodes.data ?? {});
  const set = (i: number, g: Grant) => onChange({ grants: value.grants.map((x, j) => (j === i ? g : x)) });
  return (
    <div className="space-y-3">
      {value.grants.map((g, i) => (
        <GrantEditor
          key={i}
          grant={g}
          nodes={names}
          onChange={(g) => set(i, g)}
          onRemove={() => onChange({ grants: value.grants.filter((_, j) => j !== i) })}
        />
      ))}
      <Button type="button" variant="outline" onClick={() => onChange({ grants: [...value.grants, { nodes: [], permissions: [] }] })}>
        <Plus /> Add grant
      </Button>
    </div>
  );
}

export function Roles() {
  return (
    <SectionScreen<RoleItem, RoleItem>
      section="roles"
      title="Roles"
      singular="role"
      description="Named sets of permissions on nodes. Users and principals get permissions through their roles."
      notice={
        <Alert variant="info">
          <div>
            The role <span className="font-mono">admin</span> is built in: it grants every permission on every node and on
            the server. Without any role or principal configured, data access is open to everyone; administration never is.
          </div>
        </Alert>
      }
      columns={[
        { accessorKey: "name", header: "Name", cell: ({ row }) => <span className="font-medium">{row.original.name}</span> },
        {
          id: "grants",
          header: "Grants",
          enableSorting: false,
          cell: ({ row }) => (
            <div className="space-y-1">
              {row.original.item.grants.map((g, i) => (
                <div key={i} className="flex flex-wrap items-center gap-1">
                  <span className="text-muted-foreground">{g.nodes.includes("*") ? "all nodes" : g.nodes.join(", ")}:</span>
                  {g.permissions.map((p) => (
                    <Badge key={p} variant={p.startsWith("admin.") ? "warning" : "default"}>
                      {p}
                    </Badge>
                  ))}
                </div>
              ))}
            </div>
          ),
        },
      ]}
      blank={() => ({ grants: [{ nodes: ["*"], permissions: ["query", "retrieve"] }] })}
      toForm={(r) => structuredClone(r)}
      fromForm={(r) => r}
      validate={(r) =>
        r.grants.some((g) => g.nodes.length === 0)
          ? "Every grant needs at least one node."
          : r.grants.some((g) => g.permissions.length === 0)
            ? "Every grant needs at least one permission."
            : undefined
      }
      consequence={(name) => `Role ${name} is removed; principals and anonymous callers that have it must be changed first. Users keep the name but get nothing from it.`}
      Form={RoleForm}
    />
  );
}
