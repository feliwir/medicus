import { Badge } from "@/components/ui/badge";
import { Field } from "@/components/page";
import { SectionScreen } from "@/components/section-screen";
import { Input } from "@/components/ui/input";
import { Select } from "@/components/ui/select";
import { type NodeItem, type PartitionItem, type PeerItem, validAeTitle } from "@/lib/items";
import { useSection } from "@/lib/settings";

interface NodeForm {
  backend: "partition" | "peer";
  partition: string;
  peer: string;
  aeTitle: string;
  dicomwebPath: string;
}

function NodeFormFields({ value, onChange }: { value: NodeForm; onChange: (f: NodeForm) => void }) {
  const partitions = useSection<PartitionItem>("partitions");
  const peers = useSection<PeerItem>("peers");
  const partitionNames = ["default", ...Object.keys(partitions.data ?? {}).filter((p) => p !== "default")];
  const peerNames = Object.keys(peers.data ?? {});
  return (
    <>
      <Field id="node-backend" label="Backed by" hint="A local partition, or a remote peer the node stands for.">
        <Select
          id="node-backend"
          value={value.backend}
          onChange={(e) => onChange({ ...value, backend: e.target.value as NodeForm["backend"] })}
        >
          <option value="partition">A local partition</option>
          <option value="peer" disabled={peerNames.length === 0}>
            A remote peer{peerNames.length === 0 ? " (no peers configured)" : ""}
          </option>
        </Select>
      </Field>
      {value.backend === "partition" ? (
        <Field id="node-partition" label="Partition">
          <Select id="node-partition" value={value.partition} onChange={(e) => onChange({ ...value, partition: e.target.value })}>
            {partitionNames.map((p) => (
              <option key={p}>{p}</option>
            ))}
          </Select>
        </Field>
      ) : (
        <Field id="node-peer" label="Peer">
          <Select id="node-peer" value={value.peer} onChange={(e) => onChange({ ...value, peer: e.target.value })}>
            <option value="">Choose a peer</option>
            {peerNames.map((p) => (
              <option key={p}>{p}</option>
            ))}
          </Select>
        </Field>
      )}
      <div className="grid gap-4 sm:grid-cols-2">
        <Field id="node-ae" label="DIMSE AE title" hint="Empty: not served over DIMSE.">
          <Input
            id="node-ae"
            className="font-mono"
            value={value.aeTitle}
            onChange={(e) => onChange({ ...value, aeTitle: e.target.value.toUpperCase() })}
          />
        </Field>
        <Field id="node-path" label="DICOMweb path" hint={value.dicomwebPath ? `/nodes/${value.dicomwebPath}/rs` : "Empty: not served over DICOMweb."}>
          <Input
            id="node-path"
            className="font-mono"
            value={value.dicomwebPath}
            onChange={(e) => onChange({ ...value, dicomwebPath: e.target.value })}
          />
        </Field>
      </div>
    </>
  );
}

export function Nodes() {
  return (
    <SectionScreen<NodeItem, NodeForm>
      section="nodes"
      title="Nodes"
      singular="node"
      description="How the archive appears to its peers: each node has an AE title and/or a DICOMweb path and is backed by a partition or a remote peer. Changes take effect at once; the listeners restart."
      notice={
        <p className="text-sm text-muted-foreground">
          Without any node, the archive serves the node <span className="font-mono">MEDICUS</span> on the default
          partition.
        </p>
      }
      columns={[
        { accessorKey: "name", header: "Name", cell: ({ row }) => <span className="font-medium">{row.original.name}</span> },
        {
          id: "backend",
          header: "Backed by",
          accessorFn: (e) => e.item.partition ?? e.item.peer,
          cell: ({ row }) =>
            row.original.item.peer ? (
              <span>
                peer <span className="font-mono">{row.original.item.peer}</span> <Badge>remote</Badge>
              </span>
            ) : (
              <span>
                partition <span className="font-mono">{row.original.item.partition}</span>
              </span>
            ),
        },
        {
          id: "ae",
          header: "AE title",
          accessorFn: (e) => e.item.dimse?.ae_title ?? "",
          cell: ({ getValue }) => <span className="font-mono">{(getValue() as string) || "—"}</span>,
        },
        {
          id: "path",
          header: "DICOMweb",
          accessorFn: (e) => e.item.dicomweb?.path ?? "",
          cell: ({ getValue }) =>
            getValue() ? <span className="font-mono">/nodes/{getValue() as string}/rs</span> : "—",
        },
      ]}
      blank={() => ({ backend: "partition", partition: "default", peer: "", aeTitle: "", dicomwebPath: "" })}
      toForm={(n) => ({
        backend: n.peer ? "peer" : "partition",
        partition: n.partition ?? "default",
        peer: n.peer ?? "",
        aeTitle: n.dimse?.ae_title ?? "",
        dicomwebPath: n.dicomweb?.path ?? "",
      })}
      fromForm={(f) => {
        const item: NodeItem = f.backend === "peer" ? { peer: f.peer } : { partition: f.partition };
        if (f.aeTitle.trim()) item.dimse = { ae_title: f.aeTitle.trim() };
        if (f.dicomwebPath.trim()) item.dicomweb = { path: f.dicomwebPath.trim() };
        return item;
      }}
      validate={(f) =>
        f.backend === "peer" && !f.peer
          ? "Choose the peer."
          : f.aeTitle && !validAeTitle(f.aeTitle.trim())
            ? "AE titles have 1 to 16 characters, without backslash."
            : f.dicomwebPath && !/^[A-Za-z0-9_-]+$/.test(f.dicomwebPath.trim())
              ? "DICOMweb paths use letters, digits, - and _."
              : undefined
      }
      consequence={(name) => `Node ${name} stops answering at once; roles that grant on it must be changed first.`}
      Form={NodeFormFields}
    />
  );
}
