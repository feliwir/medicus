import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import type { ColumnDef } from "@tanstack/react-table";
import { Ban } from "lucide-react";
import { useState } from "react";
import { ErrorAlert, Page } from "@/components/page";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { DataTable } from "@/components/ui/data-table";
import { Dialog, DialogContent, DialogDescription, DialogTitle } from "@/components/ui/dialog";
import { Select } from "@/components/ui/select";
import { type Job, api } from "@/lib/api";

const PAGE = 50;

export const jobStates: Record<Job["state"], "default" | "success" | "warning" | "destructive" | "outline"> = {
  queued: "outline",
  running: "default",
  succeeded: "success",
  failed: "destructive",
  cancelled: "warning",
};

function Progress({ job }: { job: Job }) {
  const { total, completed, failed } = job.progress;
  const done = total ? (completed / total) * 100 : 0;
  return (
    <div className="flex items-center gap-2">
      <div className="h-1.5 w-24 overflow-hidden rounded-full bg-muted" role="progressbar" aria-valuenow={Math.round(done)}>
        <div className="h-full bg-primary" style={{ width: `${done}%` }} />
      </div>
      <span className="font-mono text-xs text-muted-foreground">
        {completed}/{total}
        {failed > 0 && <span className="text-destructive"> ({failed} failed)</span>}
      </span>
    </div>
  );
}

export function Jobs() {
  const queryClient = useQueryClient();
  const [state, setState] = useState("");
  const [offset, setOffset] = useState(0);
  const [shown, setShown] = useState<Job>();
  const jobs = useQuery({
    queryKey: ["jobs", state, offset],
    queryFn: () => api.jobsByState(state || undefined, PAGE + 1, offset),
    refetchInterval: 3000,
  });
  const cancel = useMutation({
    mutationFn: (id: string) => api.cancelJob(id),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ["jobs"] }),
  });
  const rows = (jobs.data ?? []).slice(0, PAGE);
  const more = (jobs.data?.length ?? 0) > PAGE;
  const columns: ColumnDef<Job, unknown>[] = [
    { accessorKey: "created_at", header: "Created", cell: ({ row }) => new Date(row.original.created_at).toLocaleString() },
    {
      id: "what",
      header: "Job",
      accessorFn: (j) => `${j.kind} ${j.peer ?? ""}`,
      cell: ({ row }) => (
        <span>
          {row.original.kind} <span className="text-muted-foreground">to</span> <span className="font-medium">{row.original.peer}</span>
        </span>
      ),
    },
    { accessorKey: "node", header: "From node" },
    { id: "progress", header: "Progress", enableSorting: false, cell: ({ row }) => <Progress job={row.original} /> },
    { accessorKey: "attempts", header: "Attempts", cell: ({ row }) => <span className="font-mono">{row.original.attempts}</span> },
    { accessorKey: "state", header: "State", cell: ({ row }) => <Badge variant={jobStates[row.original.state]}>{row.original.state}</Badge> },
    {
      id: "actions",
      header: "",
      enableSorting: false,
      cell: ({ row }) =>
        (row.original.state === "queued" || row.original.state === "running") && (
          <div className="flex justify-end" onClick={(e) => e.stopPropagation()}>
            <Button variant="ghost" size="sm" disabled={cancel.isPending} onClick={() => cancel.mutate(row.original.id)}>
              <Ban /> Cancel
            </Button>
          </div>
        ),
    },
  ];
  return (
    <Page
      title="Jobs"
      description="Transfers to peers, queued and retried by the server."
      actions={
        <Select
          aria-label="State"
          className="w-44"
          value={state}
          onChange={(e) => {
            setState(e.target.value);
            setOffset(0);
          }}
        >
          <option value="">All states</option>
          {Object.keys(jobStates).map((s) => (
            <option key={s}>{s}</option>
          ))}
        </Select>
      }
    >
      <ErrorAlert error={jobs.error ?? cancel.error} />
      <DataTable columns={columns} data={rows} rowKey={(j) => j.id} onRowClick={setShown} empty={jobs.isLoading ? "Loading…" : "No jobs."} />
      <div className="flex justify-end gap-2">
        <Button variant="outline" disabled={offset === 0} onClick={() => setOffset(Math.max(0, offset - PAGE))}>
          Newer
        </Button>
        <Button variant="outline" disabled={!more} onClick={() => setOffset(offset + PAGE)}>
          Older
        </Button>
      </div>
      <Dialog open={Boolean(shown)} onOpenChange={(o) => !o && setShown(undefined)}>
        {shown && (
          <DialogContent className="max-w-2xl">
            <DialogTitle>
              {shown.kind} to {shown.peer}
            </DialogTitle>
            <DialogDescription className="font-mono">{shown.id}</DialogDescription>
            {shown.error && <ErrorAlert error={shown.error} />}
            <pre className="max-h-[50vh] overflow-auto rounded-xs border bg-muted p-3 font-mono text-xs">{JSON.stringify(shown, null, 2)}</pre>
          </DialogContent>
        )}
      </Dialog>
    </Page>
  );
}
