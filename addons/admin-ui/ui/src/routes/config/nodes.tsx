import { useQuery } from "@tanstack/react-query";
import { Badge } from "@/components/ui/badge";
import { Field } from "@/components/page";
import { SectionScreen } from "@/components/section-screen";
import { Input } from "@/components/ui/input";
import { Select } from "@/components/ui/select";
import { api, nodeRoot } from "@/lib/api";
import { type NodeItem, type PartitionItem, type PeerItem, validAeTitle } from "@/lib/items";
import { useSection } from "@/lib/settings";

interface NodeForm {
  backend: "partition" | "peer";
  partition: string;
  peer: string;
  aeTitle: string;
  /** Empty: the DIMSE addon's port. */
  dimsePort: string;
  dicomwebPath: string;
}

/** The DICOMweb addon's `node_root`, once known. */
function useNodeRoot(): string | undefined {
  const system = useQuery({ queryKey: ["system"], queryFn: api.system });
  return system.data?.dicomweb?.node_root;
}

const validPort = (p: string) => /^\d+$/.test(p) && Number(p) >= 1 && Number(p) <= 65535;

function NodeFormFields({ value, onChange }: { value: NodeForm; onChange: (f: NodeForm) => void }) {
  const partitions = useSection<PartitionItem>("partitions");
  const peers = useSection<PeerItem>("peers");
  const partitionNames = ["default", ...Object.keys(partitions.data ?? {}).filter((p) => p !== "default")];
  const peerNames = Object.keys(peers.data ?? {});
  const root = useNodeRoot();
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
      <div className="grid gap-4 sm:grid-cols-[1fr_8rem]">
        <Field id="node-ae" label="DIMSE AE title" hint="Empty: not served over DIMSE.">
          <Input
            id="node-ae"
            className="font-mono"
            value={value.aeTitle}
            onChange={(e) => onChange({ ...value, aeTitle: e.target.value.toUpperCase() })}
          />
        </Field>
        <Field id="node-port" label="Port" hint="Empty: the DIMSE addon's.">
          <Input
            id="node-port"
            type="number"
            className="font-mono"
            disabled={!value.aeTitle.trim()}
            value={value.dimsePort}
            onChange={(e) => onChange({ ...value, dimsePort: e.target.value })}
          />
        </Field>
      </div>
      <div className="grid gap-4 sm:grid-cols-2">
        <Field id="node-path" label="DICOMweb path" hint={value.dicomwebPath ? nodeRoot(value.dicomwebPath, root) : "Empty: not served over DICOMweb."}>
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
  const root = useNodeRoot();
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
          accessorFn: (e) =>
            e.item.dimse ? `${e.item.dimse.ae_title}${e.item.dimse.port ? `:${e.item.dimse.port}` : ""}` : "",
          cell: ({ getValue }) => <span className="font-mono">{(getValue() as string) || "—"}</span>,
        },
        {
          id: "path",
          header: "DICOMweb",
          accessorFn: (e) => e.item.dicomweb?.path ?? "",
          cell: ({ getValue }) =>
            getValue() ? <span className="font-mono">{nodeRoot(getValue() as string, root)}</span> : "—",
        },
      ]}
      blank={() => ({ backend: "partition", partition: "default", peer: "", aeTitle: "", dimsePort: "", dicomwebPath: "" })}
      toForm={(n) => ({
        backend: n.peer ? "peer" : "partition",
        partition: n.partition ?? "default",
        peer: n.peer ?? "",
        aeTitle: n.dimse?.ae_title ?? "",
        dimsePort: n.dimse?.port ? String(n.dimse.port) : "",
        dicomwebPath: n.dicomweb?.path ?? "",
      })}
      fromForm={(f) => {
        const item: NodeItem = f.backend === "peer" ? { peer: f.peer } : { partition: f.partition };
        if (f.aeTitle.trim()) {
          item.dimse = { ae_title: f.aeTitle.trim() };
          if (f.dimsePort.trim()) item.dimse.port = Number(f.dimsePort);
        }
        if (f.dicomwebPath.trim()) item.dicomweb = { path: f.dicomwebPath.trim() };
        return item;
      }}
      validate={(f) =>
        f.backend === "peer" && !f.peer
          ? "Choose the peer."
          : f.aeTitle && !validAeTitle(f.aeTitle.trim())
            ? "AE titles have 1 to 16 characters, without backslash."
            : f.aeTitle && f.dimsePort.trim() && !validPort(f.dimsePort.trim())
              ? "Use a port between 1 and 65535."
              : f.dicomwebPath && !/^[A-Za-z0-9_-]+$/.test(f.dicomwebPath.trim())
                ? "DICOMweb paths use letters, digits, - and _."
                : undefined
      }
      consequence={(name) => `Node ${name} stops answering at once; roles that grant on it must be changed first.`}
      Form={NodeFormFields}
    />
  );
}
