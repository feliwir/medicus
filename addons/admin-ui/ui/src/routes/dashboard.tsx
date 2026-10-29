import { useQuery } from "@tanstack/react-query";
import { Activity, Boxes, ListChecks, Server } from "lucide-react";
import { type ReactNode, useEffect, useState } from "react";
import { Badge } from "@/components/ui/badge";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { type AddonStatus, type Job, type MedicusEvent, api } from "@/lib/api";
import { EVENT_KINDS } from "@/lib/events";

function Panel({ title, icon, children }: { title: string; icon: ReactNode; children: ReactNode }) {
  return (
    <Card>
      <CardHeader className="flex-row items-center gap-3 space-y-0 [&_svg]:size-5 [&_svg]:text-muted-foreground">
        {icon}
        <CardTitle>{title}</CardTitle>
      </CardHeader>
      <CardContent>{children}</CardContent>
    </Card>
  );
}

function Empty({ children }: { children: ReactNode }) {
  return <p className="text-sm text-muted-foreground">{children}</p>;
}

function loadedBy(addon: AddonStatus): string {
  const l = addon.loaded_by;
  switch (l.by) {
    case "config":
      return "enabled";
    case "required":
      return "required";
    case "archive":
      return `${l.capability?.replace("_", " ")} of the archive`;
    case "dependency":
      return `needed by ${l.of}`;
  }
}

const jobStates: Record<Job["state"], "default" | "success" | "warning" | "destructive" | "outline"> = {
  queued: "outline",
  running: "default",
  succeeded: "success",
  failed: "destructive",
  cancelled: "warning",
};

/** The last events, live from the event stream. */
function useLiveEvents(enabled: boolean, keep = 15) {
  const [events, setEvents] = useState<MedicusEvent[]>([]);
  useEffect(() => {
    if (!enabled) return;
    const source = new EventSource(`/api/v1/events/stream?tail=${keep}`);
    const onEvent = (e: MessageEvent<string>) => {
      const event = JSON.parse(e.data) as MedicusEvent;
      setEvents((list) => [event, ...list.filter((x) => x.seq !== event.seq)].slice(0, keep));
    };
    // Every event is named after its kind, so each kind needs its own listener.
    for (const kind of EVENT_KINDS) source.addEventListener(kind, onEvent as EventListener);
    return () => source.close();
  }, [enabled, keep]);
  return events;
}


function time(iso: string) {
  return new Date(iso).toLocaleTimeString();
}

export function Dashboard() {
  const me = useQuery({ queryKey: ["me"], queryFn: api.me });
  const can = (p: string) => me.data?.permissions.includes(p) ?? false;
  const system = useQuery({ queryKey: ["system"], queryFn: api.system, enabled: can("admin.system") });
  const nodes = useQuery({ queryKey: ["nodes"], queryFn: api.nodes });
  const jobs = useQuery({ queryKey: ["jobs", 8], queryFn: () => api.jobs(8), enabled: can("admin.jobs"), refetchInterval: 5000 });
  const events = useLiveEvents(can("admin.events"));

  return (
    <div className="space-y-6 p-6">
      <h1 className="text-headline-sm">Dashboard</h1>
      <div className="grid gap-4 lg:grid-cols-2 xl:grid-cols-3">
        <Panel title="Server" icon={<Server />}>
          {system.data ? (
            <dl className="grid grid-cols-[10rem_1fr] gap-y-2 text-sm">
              <dt className="text-muted-foreground">Name</dt>
              <dd>{system.data.name}</dd>
              <dt className="text-muted-foreground">Version</dt>
              <dd className="font-mono">{system.data.version}</dd>
              <dt className="text-muted-foreground">Addon ABI</dt>
              <dd className="font-mono">{system.data.abi_version}</dd>
              <dt className="text-muted-foreground">Settings revision</dt>
              <dd className="font-mono">{system.data.settings_revision}</dd>
              <dt className="text-muted-foreground">Default storage</dt>
              <dd className="font-mono">{system.data.storage}</dd>
            </dl>
          ) : (
            <Empty>{can("admin.system") ? "Loading…" : "You may not see the server status."}</Empty>
          )}
          <div className="mt-4 border-t pt-4">
            <div className="mb-2 text-sm font-medium text-muted-foreground">Nodes</div>
            {nodes.data?.length ? (
              <ul className="space-y-2 text-sm">
                {nodes.data.map((n) => (
                  <li key={n.name} className="flex flex-wrap items-center gap-1.5">
                    <span className="font-medium">{n.name}</span>
                    {n.ae_title && <Badge variant="outline" className="font-mono">AE {n.ae_title}</Badge>}
                    {n.dicomweb_path && (
                      <Badge variant="outline" className="font-mono">/nodes/{n.dicomweb_path}/rs</Badge>
                    )}
                    {n.remote && <Badge>remote</Badge>}
                  </li>
                ))}
              </ul>
            ) : (
              <Empty>No nodes.</Empty>
            )}
          </div>
        </Panel>

        <Panel title="Addons" icon={<Boxes />}>
          {system.data ? (
            <ul className="divide-y text-sm">
              {system.data.addons.map((a) => (
                <li key={a.name} className="flex min-h-10 items-center gap-2 py-1 first:pt-0 last:pb-0">
                  <span className="font-mono">{a.name}</span>
                  <span className="text-xs text-muted-foreground">{a.version}</span>
                  <Badge variant={a.loaded_by.by === "config" ? "success" : "outline"} className="ml-auto">
                    {loadedBy(a)}
                  </Badge>
                </li>
              ))}
            </ul>
          ) : (
            <Empty>—</Empty>
          )}
        </Panel>

        <Panel title="Recent jobs" icon={<ListChecks />}>
          {!can("admin.jobs") ? (
            <Empty>You may not see jobs.</Empty>
          ) : jobs.data?.length ? (
            <table className="w-full text-sm">
              <tbody className="divide-y">
                {jobs.data.map((j) => (
                  <tr key={j.id}>
                    <td className="py-2">
                      {j.kind} {j.peer && <span className="text-muted-foreground">to {j.peer}</span>}
                    </td>
                    <td className="py-2 text-right font-mono text-xs text-muted-foreground">
                      {j.progress.completed}/{j.progress.total}
                    </td>
                    <td className="py-2 pl-2 text-right">
                      <Badge variant={jobStates[j.state]}>{j.state}</Badge>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          ) : (
            <Empty>No jobs yet. Jobs send studies to peers.</Empty>
          )}
        </Panel>

        <div className="lg:col-span-2 xl:col-span-3">
          <Panel title="Live events" icon={<Activity />}>
            {!can("admin.events") ? (
              <Empty>You may not see events.</Empty>
            ) : events.length ? (
              <table className="w-full text-sm">
                <tbody className="divide-y">
                  {events.map((e) => (
                    <tr key={e.seq}>
                      <td className="w-24 py-2 font-mono text-xs text-muted-foreground">{time(e.time)}</td>
                      <td className="w-40 py-2">
                        <Badge variant={e.kind.includes("fail") || e.kind.includes("denied") ? "destructive" : "default"}>
                          {e.kind}
                        </Badge>
                      </td>
                      <td className="py-2 text-muted-foreground">{e.node ?? "—"}</td>
                      <td className="truncate py-2 font-mono text-xs text-muted-foreground">
                        {e.resource?.study ?? e.origin.type}
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            ) : (
              <Empty>Waiting for events. Store an instance and it appears here.</Empty>
            )}
          </Panel>
        </div>
      </div>
    </div>
  );
}
