import { useQuery } from "@tanstack/react-query";
import type { ColumnDef } from "@tanstack/react-table";
import { useEffect, useState } from "react";
import { ErrorAlert, Page } from "@/components/page";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { DataTable } from "@/components/ui/data-table";
import { Dialog, DialogContent, DialogTitle } from "@/components/ui/dialog";
import { Select } from "@/components/ui/select";
import { Switch } from "@/components/ui/switch";
import { type MedicusEvent, api } from "@/lib/api";
import { EVENT_KINDS } from "@/lib/events";

export function Events() {
  const [tail, setTail] = useState(100);
  const [kind, setKind] = useState("");
  const [live, setLive] = useState(true);
  const [arrived, setArrived] = useState<MedicusEvent[]>([]);
  const [shown, setShown] = useState<MedicusEvent>();
  const page = useQuery({ queryKey: ["events", tail, kind], queryFn: () => api.events({ tail, limit: tail, kind }) });

  useEffect(() => {
    setArrived([]);
    if (!live) return;
    const source = new EventSource(`/api/v1/events/stream?tail=0${kind ? `&kind=${kind}` : ""}`);
    const add = (e: MessageEvent<string>) => setArrived((list) => [JSON.parse(e.data) as MedicusEvent, ...list].slice(0, 500));
    for (const k of EVENT_KINDS) source.addEventListener(k, add as EventListener);
    return () => source.close();
  }, [live, kind, page.dataUpdatedAt]);

  const known = new Set(arrived.map((e) => e.seq));
  const events = [...arrived, ...[...(page.data?.events ?? [])].reverse().filter((e) => !known.has(e.seq))];

  const columns: ColumnDef<MedicusEvent, unknown>[] = [
    { accessorKey: "seq", header: "#", cell: ({ row }) => <span className="font-mono text-muted-foreground">{row.original.seq}</span> },
    {
      accessorKey: "time",
      header: "Time",
      cell: ({ row }) => <span className="whitespace-nowrap">{new Date(row.original.time).toLocaleString()}</span>,
    },
    {
      accessorKey: "kind",
      header: "Kind",
      cell: ({ row }) => (
        <Badge variant={/fail|denied|full|unhealthy|unreachable|rejected/.test(row.original.kind) ? "destructive" : "default"}>
          {row.original.kind}
        </Badge>
      ),
    },
    { id: "node", header: "Node", accessorFn: (e) => e.node ?? "", cell: ({ getValue }) => (getValue() as string) || "—" },
    { id: "partition", header: "Partition", accessorFn: (e) => e.partition ?? "", cell: ({ getValue }) => <span className="font-mono">{(getValue() as string) || "—"}</span> },
    {
      id: "origin",
      header: "Caller",
      accessorFn: (e) => e.origin.type,
      cell: ({ row }) => {
        const o = row.original.origin as Record<string, unknown>;
        const who = (o.principal ?? o.calling_ae ?? o.component ?? o.path ?? "") as string;
        // Imports name a file: its name says enough, the path is in the tooltip and the details.
        const short = o.type === "import" ? (who.split(/[\\/]/).pop() ?? who) : who;
        return (
          <span className="whitespace-nowrap" title={who}>
            {o.type as string} {short && <span className="text-muted-foreground">{short}</span>}
          </span>
        );
      },
    },
    {
      id: "resource",
      header: "Study",
      accessorFn: (e) => e.resource?.study ?? "",
      cell: ({ getValue }) => <span className="font-mono text-xs">{(getValue() as string) || "—"}</span>,
    },
  ];
  return (
    <Page
      title="Events"
      description="What happened, newest first: stored and deleted data, jobs, storage, access denials and settings changes."
      actions={
        <>
          <label className="flex items-center gap-3 text-sm">
            <Switch checked={live} onCheckedChange={setLive} aria-label="Live" />
            Live
          </label>
          <Select aria-label="Kind" className="w-52" value={kind} onChange={(e) => setKind(e.target.value)}>
            <option value="">All kinds</option>
            {EVENT_KINDS.map((k) => (
              <option key={k}>{k}</option>
            ))}
          </Select>
        </>
      }
    >
      <ErrorAlert error={page.error} />
      <DataTable columns={columns} data={events} rowKey={(e) => String(e.seq)} onRowClick={setShown} empty={page.isLoading ? "Loading…" : "No events."} />
      <div className="flex justify-end">
        <Button variant="outline" disabled={(page.data?.events.length ?? 0) < tail} onClick={() => setTail(tail + 200)}>
          Load older
        </Button>
      </div>
      <Dialog open={Boolean(shown)} onOpenChange={(o) => !o && setShown(undefined)}>
        {shown && (
          <DialogContent className="max-w-2xl">
            <DialogTitle>
              {shown.kind} #{shown.seq}
            </DialogTitle>
            <pre className="max-h-[60vh] overflow-auto rounded-xs border bg-muted p-3 font-mono text-xs">{JSON.stringify(shown, null, 2)}</pre>
          </DialogContent>
        )}
      </Dialog>
    </Page>
  );
}
