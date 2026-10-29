import { Field } from "@/components/page";
import { SectionScreen } from "@/components/section-screen";
import { Input } from "@/components/ui/input";
import type { NodeItem, PartitionItem } from "@/lib/items";
import { useSection } from "@/lib/settings";

function NodesUsing({ partition }: { partition: string }) {
  const nodes = useSection<NodeItem>("nodes");
  const using = Object.entries(nodes.data ?? {})
    .filter(([, n]) => n.partition === partition)
    .map(([name]) => name);
  return <span className="text-muted-foreground">{using.length ? using.join(", ") : "—"}</span>;
}

export function Partitions() {
  return (
    <SectionScreen<PartitionItem, { storage: string }>
      section="partitions"
      title="Partitions"
      singular="partition"
      description="Separate archives in one server: the same UIDs can exist in each, and nothing is visible across them. Nodes give access to a partition."
      notice={
        <p className="text-sm text-muted-foreground">
          The partition <span className="font-mono">default</span> always exists.
        </p>
      }
      columns={[
        { accessorKey: "name", header: "Name", cell: ({ row }) => <span className="font-mono font-medium">{row.original.name}</span> },
        {
          id: "storage",
          header: "Blob store",
          accessorFn: (e) => e.item.storage ?? "",
          cell: ({ getValue }) =>
            getValue() ? <span className="font-mono">{getValue() as string}</span> : <span className="text-muted-foreground">the archive's</span>,
        },
        { id: "nodes", header: "Nodes", enableSorting: false, cell: ({ row }) => <NodesUsing partition={row.original.name} /> },
      ]}
      blank={() => ({ storage: "" })}
      toForm={(p) => ({ storage: p.storage ?? "" })}
      fromForm={(f) => (f.storage.trim() ? { storage: f.storage.trim() } : {})}
      Form={({ value, onChange }) => (
        <Field
          id="partition-storage"
          label="Blob store"
          hint="The registered name of the blob store new instances of this partition go to. Empty: the archive's storage."
        >
          <Input
            id="partition-storage"
            className="font-mono"
            value={value.storage}
            onChange={(e) => onChange({ storage: e.target.value })}
          />
        </Field>
      )}
      consequence={(name) =>
        `Partition ${name} is removed from the settings; nodes on it must be changed first. Its data stays in the index.`
      }
    />
  );
}
