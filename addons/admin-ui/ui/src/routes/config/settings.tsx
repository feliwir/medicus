import { useQuery } from "@tanstack/react-query";
import { useBlocker } from "@tanstack/react-router";
import type { ColumnDef } from "@tanstack/react-table";
import { Plus, RotateCcw, X } from "lucide-react";
import { type ReactNode, useEffect, useState } from "react";
import { ConfirmDialog, ErrorAlert, Field, Page } from "@/components/page";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Checkbox } from "@/components/ui/checkbox";
import { DataTable } from "@/components/ui/data-table";
import { Dialog, DialogContent, DialogDescription, DialogTitle } from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Select } from "@/components/ui/select";
import { type RevisionSummary, type Settings as SettingsDoc, api } from "@/lib/api";
import { useSettingsMutation } from "@/lib/settings";

interface Archive {
  storage: string;
  duplicate_policy: "reject" | "keep_existing" | "overwrite";
  max_header_bytes: number;
  extra_tags: { tag: string; level: string }[];
  capacity: { critical_percent: number; resume_percent: number; quota_bytes: number | null; check_interval_secs: number };
}

interface Jobs {
  concurrency: number;
  max_attempts: number;
  retry_base_secs: number;
}

type Doc = SettingsDoc & { archive: Archive; jobs: Jobs; access: { anonymous_roles: string[] }; roles: Record<string, unknown> };

function Section({ title, description, children }: { title: string; description: string; children: ReactNode }) {
  return (
    <Card>
      <CardHeader>
        <CardTitle>{title}</CardTitle>
        <p className="text-sm text-muted-foreground">{description}</p>
      </CardHeader>
      <CardContent className="space-y-4">{children}</CardContent>
    </Card>
  );
}

function NumberInput({ id, value, onChange, className = "w-40" }: { id: string; value: number | null; onChange: (v: number | null) => void; className?: string }) {
  return (
    <Input
      id={id}
      type="number"
      className={`font-mono ${className}`}
      value={value ?? ""}
      onChange={(e) => onChange(e.target.value === "" ? null : Number(e.target.value))}
    />
  );
}

function RevisionHistory({ revision }: { revision: number }) {
  const history = useQuery({ queryKey: ["history"], queryFn: () => api.history(100) });
  const [viewing, setViewing] = useState<number>();
  const [rollingBack, setRollingBack] = useState<number>();
  const shown = useQuery({ queryKey: ["revision", viewing], queryFn: () => api.revision(viewing ?? 0), enabled: viewing !== undefined });
  const rollback = useSettingsMutation((n: number) => api.rollback(n));
  const columns: ColumnDef<RevisionSummary, unknown>[] = [
    { accessorKey: "revision", header: "Revision", cell: ({ row }) => <span className="font-mono">{row.original.revision}{row.original.revision === revision && " (current)"}</span> },
    { accessorKey: "created_at", header: "When", cell: ({ row }) => new Date(row.original.created_at).toLocaleString() },
    { id: "author", header: "By", accessorFn: (r) => r.author ?? "", cell: ({ getValue }) => (getValue() as string) || <span className="text-muted-foreground">the server</span> },
    { id: "comment", header: "Comment", accessorFn: (r) => r.comment ?? "", cell: ({ getValue }) => getValue() as string },
    {
      id: "actions",
      header: "",
      enableSorting: false,
      cell: ({ row }) =>
        row.original.revision !== revision && (
          <div className="flex justify-end" onClick={(e) => e.stopPropagation()}>
            <Button variant="ghost" size="sm" onClick={() => setRollingBack(row.original.revision)}>
              <RotateCcw /> Roll back
            </Button>
          </div>
        ),
    },
  ];
  return (
    <Section title="History" description="Every change is a revision. Rolling back stores an old revision's settings as a new one.">
      <ErrorAlert error={history.error} />
      <DataTable columns={columns} data={history.data ?? []} rowKey={(r) => String(r.revision)} onRowClick={(r) => setViewing(r.revision)} />
      <Dialog open={viewing !== undefined} onOpenChange={(o) => !o && setViewing(undefined)}>
        <DialogContent className="max-w-3xl">
          <DialogTitle>Revision {viewing}</DialogTitle>
          <DialogDescription>{shown.data?.comment ?? ""}</DialogDescription>
          <pre className="max-h-[60vh] overflow-auto rounded-xs border bg-muted p-3 font-mono text-xs">
            {shown.data ? JSON.stringify(shown.data.settings, null, 2) : "Loading…"}
          </pre>
        </DialogContent>
      </Dialog>
      <ConfirmDialog
        open={rollingBack !== undefined}
        onOpenChange={(o) => !o && setRollingBack(undefined)}
        title={`Roll back to revision ${rollingBack}?`}
        consequence="Its settings become the current ones, as a new revision. Nodes, peers and access change at once; the rest after a restart."
        action="Roll back"
        pending={rollback.isPending}
        error={rollback.error}
        onConfirm={() => rollingBack !== undefined && rollback.mutate(rollingBack, { onSuccess: () => setRollingBack(undefined) })}
      />
    </Section>
  );
}

export function Settings() {
  const state = useQuery({ queryKey: ["settings"], queryFn: api.settings });
  const [draft, setDraft] = useState<Doc>();
  const [comment, setComment] = useState("");
  const save = useSettingsMutation(() => api.putSettings(state.data?.revision ?? 0, draft as Doc, comment || "settings"));

  useEffect(() => {
    if (state.data) setDraft(structuredClone(state.data.settings) as Doc);
  }, [state.data]);

  const dirty = Boolean(draft && state.data && JSON.stringify(draft) !== JSON.stringify(state.data.settings));
  useBlocker({ shouldBlockFn: () => dirty && !window.confirm("Discard the unsaved changes?"), enableBeforeUnload: () => dirty });

  if (!draft || !state.data) {
    return (
      <Page title="Settings">
        <ErrorAlert error={state.error} />
      </Page>
    );
  }
  const set = (change: (d: Doc) => void) => {
    const next = structuredClone(draft);
    change(next);
    setDraft(next);
  };
  const { archive, jobs } = draft;
  const roles = ["admin", ...Object.keys(draft.roles ?? {})];

  return (
    <Page
      title="Settings"
      description={`Server, archive, job and access settings; revision ${state.data.revision}. Archive and job settings take effect after a restart.`}
      actions={
        dirty && (
          <>
            <Input className="w-64" placeholder="Why (optional)" value={comment} onChange={(e) => setComment(e.target.value)} />
            <Button variant="ghost" onClick={() => setDraft(structuredClone(state.data.settings) as Doc)}>
              Discard
            </Button>
            <Button disabled={save.isPending} onClick={() => save.mutate(undefined, { onSuccess: () => setComment("") })}>
              {save.isPending ? "Saving…" : "Save changes"}
            </Button>
          </>
        )
      }
    >
      <ErrorAlert error={save.error} />
      <div className="grid gap-6 xl:grid-cols-2">
        <Section title="Server" description="Shown in this interface and the API.">
          <Field id="s-name" label="Name">
            <Input id="s-name" value={draft.server.name} onChange={(e) => set((d) => void (d.server.name = e.target.value))} />
          </Field>
        </Section>

        <Section title="Access" description="What callers who are no user and match no principal may do.">
          <div className="text-sm font-medium">Roles of anonymous callers</div>
          <div className="flex flex-wrap gap-x-6">
            {roles.map((r) => (
              <Checkbox
                key={r}
                label={r}
                checked={draft.access.anonymous_roles.includes(r)}
                onChange={(e) =>
                  set((d) => {
                    d.access.anonymous_roles = e.target.checked
                      ? [...d.access.anonymous_roles, r]
                      : d.access.anonymous_roles.filter((x) => x !== r);
                  })
                }
              />
            ))}
          </div>
        </Section>

        <Section title="Archive" description="How instances are stored.">
          <div className="grid gap-4 sm:grid-cols-2">
            <Field id="a-storage" label="Blob store" hint="Registered name of the store new instances go to.">
              <Input id="a-storage" className="font-mono" value={archive.storage} onChange={(e) => set((d) => void (d.archive.storage = e.target.value))} />
            </Field>
            <Field id="a-dup" label="Duplicates" hint="When an instance with a stored SOP Instance UID arrives.">
              <Select
                id="a-dup"
                value={archive.duplicate_policy}
                onChange={(e) => set((d) => void (d.archive.duplicate_policy = e.target.value as Archive["duplicate_policy"]))}
              >
                <option value="overwrite">Replace the stored instance</option>
                <option value="keep_existing">Keep the stored instance</option>
                <option value="reject">Refuse the new instance</option>
              </Select>
            </Field>
          </div>
          <Field id="a-header" label="Largest header (bytes)" hint="Everything before the pixel data; larger instances are refused.">
            <NumberInput id="a-header" value={archive.max_header_bytes} onChange={(v) => set((d) => void (d.archive.max_header_bytes = v ?? 0))} />
          </Field>
          <div className="text-sm font-medium">Extra indexed attributes</div>
          <p className="text-xs text-muted-foreground">Searchable in addition to the standard keys: a keyword or tag, and its level.</p>
          {archive.extra_tags.map((t, i) => (
            <div key={i} className="flex gap-2">
              <Input
                aria-label="Attribute"
                className="font-mono"
                value={t.tag}
                onChange={(e) => set((d) => void ((d.archive.extra_tags[i] as { tag: string }).tag = e.target.value))}
              />
              <Select
                aria-label="Level"
                className="w-40 shrink-0"
                value={t.level}
                onChange={(e) => set((d) => void ((d.archive.extra_tags[i] as { level: string }).level = e.target.value))}
              >
                {["patient", "study", "series", "instance"].map((l) => (
                  <option key={l}>{l}</option>
                ))}
              </Select>
              <Button variant="ghost" size="icon" aria-label="Remove" onClick={() => set((d) => void d.archive.extra_tags.splice(i, 1))}>
                <X />
              </Button>
            </div>
          ))}
          <Button variant="outline" size="sm" onClick={() => set((d) => void d.archive.extra_tags.push({ tag: "", level: "instance" }))}>
            <Plus /> Add attribute
          </Button>
        </Section>

        <Section title="Capacity" description="New instances are refused when the storage is almost full, until usage drops again.">
          <div className="grid gap-4 sm:grid-cols-2">
            <Field id="c-critical" label="Refuse at (%)">
              <NumberInput id="c-critical" value={archive.capacity.critical_percent} onChange={(v) => set((d) => void (d.archive.capacity.critical_percent = v ?? 0))} />
            </Field>
            <Field id="c-resume" label="Resume below (%)">
              <NumberInput id="c-resume" value={archive.capacity.resume_percent} onChange={(v) => set((d) => void (d.archive.capacity.resume_percent = v ?? 0))} />
            </Field>
            <Field id="c-quota" label="Quota (bytes)" hint="Empty: the disk's capacity. Needed for stores without a limit.">
              <NumberInput id="c-quota" className="w-48" value={archive.capacity.quota_bytes} onChange={(v) => set((d) => void (d.archive.capacity.quota_bytes = v))} />
            </Field>
            <Field id="c-interval" label="Check every (seconds)">
              <NumberInput id="c-interval" value={archive.capacity.check_interval_secs} onChange={(v) => set((d) => void (d.archive.capacity.check_interval_secs = v ?? 1))} />
            </Field>
          </div>
        </Section>

        <Section title="Jobs" description="Transfers to peers.">
          <div className="grid gap-4 sm:grid-cols-3">
            <Field id="j-conc" label="At the same time">
              <NumberInput id="j-conc" className="w-full" value={jobs.concurrency} onChange={(v) => set((d) => void (d.jobs.concurrency = v ?? 1))} />
            </Field>
            <Field id="j-att" label="Attempts">
              <NumberInput id="j-att" className="w-full" value={jobs.max_attempts} onChange={(v) => set((d) => void (d.jobs.max_attempts = v ?? 1))} />
            </Field>
            <Field id="j-retry" label="First retry after (s)" hint="Doubles each time.">
              <NumberInput id="j-retry" className="w-full" value={jobs.retry_base_secs} onChange={(v) => set((d) => void (d.jobs.retry_base_secs = v ?? 1))} />
            </Field>
          </div>
        </Section>
      </div>
      <RevisionHistory revision={state.data.revision} />
    </Page>
  );
}
