import { useMutation } from "@tanstack/react-query";
import { Radio } from "lucide-react";
import { Field } from "@/components/page";
import { type Entry, SectionScreen } from "@/components/section-screen";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { api } from "@/lib/api";
import { type PeerItem, validAeTitle } from "@/lib/items";

interface PeerForm {
  aeTitle: string;
  host: string;
  port: string;
}

/** C-ECHO to the peer, with the outcome next to the button. */
function Echo({ entry }: { entry: Entry<PeerItem> }) {
  const echo = useMutation({ mutationFn: () => api.echoPeer(entry.name) });
  return (
    <span className="flex items-center gap-2">
      {echo.isSuccess && <Badge variant="success">reachable</Badge>}
      {echo.isError && (
        <Badge variant="destructive" title={echo.error.message}>
          unreachable
        </Badge>
      )}
      <Button variant="outline" size="sm" disabled={echo.isPending} onClick={() => echo.mutate()}>
        <Radio /> {echo.isPending ? "Echoing…" : "C-ECHO"}
      </Button>
    </span>
  );
}

export function Peers() {
  return (
    <SectionScreen<PeerItem, PeerForm>
      section="peers"
      title="Peers"
      singular="peer"
      description="Other DICOM systems the archive sends to and can stand in for (remote nodes). Changes take effect at once."
      columns={[
        { accessorKey: "name", header: "Name", cell: ({ row }) => <span className="font-medium">{row.original.name}</span> },
        { id: "ae", header: "AE title", accessorFn: (e) => e.item.ae_title, cell: ({ getValue }) => <span className="font-mono">{getValue() as string}</span> },
        {
          id: "address",
          header: "Address",
          accessorFn: (e) => `${e.item.host}:${e.item.port}`,
          cell: ({ getValue }) => <span className="font-mono">{getValue() as string}</span>,
        },
        { id: "protocol", header: "Protocol", accessorFn: (e) => e.item.protocol, cell: () => <Badge variant="outline">DIMSE</Badge> },
      ]}
      rowActions={(entry) => <Echo entry={entry} />}
      blank={() => ({ aeTitle: "", host: "", port: "104" })}
      toForm={(p) => ({ aeTitle: p.ae_title, host: p.host, port: String(p.port) })}
      fromForm={(f) => ({ protocol: "dimse", ae_title: f.aeTitle.trim(), host: f.host.trim(), port: Number(f.port) })}
      validate={(f) =>
        !validAeTitle(f.aeTitle.trim())
          ? "AE titles have 1 to 16 characters, without backslash."
          : !f.host.trim()
            ? "Give the host name or address."
            : !(Number.isInteger(Number(f.port)) && Number(f.port) > 0 && Number(f.port) < 65536)
              ? "Use a port between 1 and 65535."
              : undefined
      }
      consequence={(name) => `Peer ${name} is removed; queued jobs to it fail, and remote nodes for it must be changed first.`}
      Form={({ value, onChange }) => (
        <>
          <Field id="peer-ae" label="AE title">
            <Input
              id="peer-ae"
              className="font-mono"
              value={value.aeTitle}
              onChange={(e) => onChange({ ...value, aeTitle: e.target.value.toUpperCase() })}
            />
          </Field>
          <div className="grid grid-cols-[1fr_8rem] gap-4">
            <Field id="peer-host" label="Host">
              <Input id="peer-host" className="font-mono" value={value.host} onChange={(e) => onChange({ ...value, host: e.target.value })} />
            </Field>
            <Field id="peer-port" label="Port">
              <Input
                id="peer-port"
                type="number"
                className="font-mono"
                value={value.port}
                onChange={(e) => onChange({ ...value, port: e.target.value })}
              />
            </Field>
          </div>
        </>
      )}
    />
  );
}
