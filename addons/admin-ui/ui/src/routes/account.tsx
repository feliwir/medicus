import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import type { ColumnDef } from "@tanstack/react-table";
import { Copy, Plus, Trash2 } from "lucide-react";
import { type FormEvent, useState } from "react";
import { ConfirmDialog, ErrorAlert, Field, Page } from "@/components/page";
import { Alert } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { DataTable } from "@/components/ui/data-table";
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogTitle } from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { type TokenInfo, api } from "@/lib/api";

const date = (iso?: string) => (iso ? new Date(iso).toLocaleString() : "—");

function Password() {
  const [form, setForm] = useState({ current: "", next: "", repeat: "" });
  const change = useMutation({
    mutationFn: () => api.changePassword(form.current, form.next),
    onSuccess: () => setForm({ current: "", next: "", repeat: "" }),
  });
  const problem =
    form.next && form.next.length < 8
      ? "At least 8 characters."
      : form.repeat && form.next !== form.repeat
        ? "The passwords differ."
        : undefined;
  const submit = (e: FormEvent) => {
    e.preventDefault();
    if (!problem) change.mutate();
  };
  return (
    <Card>
      <CardHeader>
        <CardTitle>Password</CardTitle>
      </CardHeader>
      <CardContent>
        <form className="max-w-sm space-y-4" onSubmit={submit}>
          <Field id="pw-current" label="Current password">
            <Input id="pw-current" type="password" autoComplete="current-password" value={form.current} onChange={(e) => setForm({ ...form, current: e.target.value })} />
          </Field>
          <Field id="pw-new" label="New password" error={problem}>
            <Input id="pw-new" type="password" autoComplete="new-password" value={form.next} onChange={(e) => setForm({ ...form, next: e.target.value })} />
          </Field>
          <Field id="pw-repeat" label="Repeat the new password">
            <Input id="pw-repeat" type="password" autoComplete="new-password" value={form.repeat} onChange={(e) => setForm({ ...form, repeat: e.target.value })} />
          </Field>
          <ErrorAlert error={change.error} />
          {change.isSuccess && <p className="text-sm text-success">The password is changed.</p>}
          <Button type="submit" disabled={Boolean(problem) || !form.current || !form.next || !form.repeat || change.isPending}>
            Change password
          </Button>
        </form>
      </CardContent>
    </Card>
  );
}

function Tokens() {
  const queryClient = useQueryClient();
  const tokens = useQuery({ queryKey: ["tokens"], queryFn: api.tokens });
  const [creating, setCreating] = useState(false);
  const [form, setForm] = useState({ name: "", days: "" });
  const [created, setCreated] = useState<TokenInfo>();
  const [revoking, setRevoking] = useState<TokenInfo>();
  const refresh = () => queryClient.invalidateQueries({ queryKey: ["tokens"] });
  const create = useMutation({
    mutationFn: () => api.createToken(form.name.trim() || undefined, form.days ? Number(form.days) : undefined),
    onSuccess: async (t) => {
      setCreated(t);
      setForm({ name: "", days: "" });
      await refresh();
    },
  });
  const revoke = useMutation({
    mutationFn: (id: string) => api.deleteToken(id),
    onSuccess: async () => {
      setRevoking(undefined);
      await refresh();
    },
  });
  const columns: ColumnDef<TokenInfo, unknown>[] = [
    { id: "name", header: "Name", accessorFn: (t) => t.name ?? "", cell: ({ getValue }) => (getValue() as string) || <span className="text-muted-foreground">unnamed</span> },
    { accessorKey: "created_at", header: "Created", cell: ({ row }) => date(row.original.created_at) },
    { accessorKey: "expires_at", header: "Expires", cell: ({ row }) => (row.original.expires_at ? date(row.original.expires_at) : "never") },
    { accessorKey: "last_used_at", header: "Last used", cell: ({ row }) => date(row.original.last_used_at) },
    {
      id: "actions",
      header: "",
      enableSorting: false,
      cell: ({ row }) => (
        <div className="flex justify-end">
          <Button variant="ghost" size="icon" aria-label="Revoke token" onClick={() => setRevoking(row.original)}>
            <Trash2 />
          </Button>
        </div>
      ),
    },
  ];
  return (
    <Card>
      <CardHeader className="flex-row items-center justify-between">
        <div>
          <CardTitle>API tokens</CardTitle>
          <p className="mt-1 text-sm text-muted-foreground">
            For scripts and viewers: send as <code className="font-mono">Authorization: Bearer …</code> to the admin API and
            DICOMweb. A token can do what you can.
          </p>
        </div>
        <Button
          onClick={() => {
            create.reset();
            setCreated(undefined);
            setCreating(true);
          }}
        >
          <Plus /> New token
        </Button>
      </CardHeader>
      <CardContent>
        <ErrorAlert error={tokens.error} />
        <DataTable columns={columns} data={tokens.data ?? []} rowKey={(t) => t.id} empty="No tokens." />
      </CardContent>

      <Dialog open={creating} onOpenChange={setCreating}>
        <DialogContent>
          <DialogTitle>{created ? "Your new token" : "New API token"}</DialogTitle>
          {created ? (
            <>
              <Alert variant="warning">
                <div>Copy it now: it is shown only once.</div>
              </Alert>
              <div className="flex gap-2">
                <Input readOnly className="font-mono text-sm" value={created.token ?? ""} onFocus={(e) => e.target.select()} />
                <Button variant="outline" size="icon" aria-label="Copy" onClick={() => void navigator.clipboard?.writeText(created.token ?? "")}>
                  <Copy />
                </Button>
              </div>
              <DialogFooter>
                <Button onClick={() => setCreating(false)}>Done</Button>
              </DialogFooter>
            </>
          ) : (
            <form
              className="space-y-4"
              onSubmit={(e) => {
                e.preventDefault();
                create.mutate();
              }}
            >
              <DialogDescription>Name it after where it is used.</DialogDescription>
              <Field id="t-name" label="Name">
                <Input id="t-name" autoFocus value={form.name} onChange={(e) => setForm({ ...form, name: e.target.value })} />
              </Field>
              <Field id="t-days" label="Expires after (days)" hint="Empty: never.">
                <Input id="t-days" type="number" min={1} className="w-32" value={form.days} onChange={(e) => setForm({ ...form, days: e.target.value })} />
              </Field>
              <ErrorAlert error={create.error} />
              <DialogFooter>
                <Button type="button" variant="ghost" onClick={() => setCreating(false)}>
                  Cancel
                </Button>
                <Button type="submit" disabled={create.isPending}>
                  Create token
                </Button>
              </DialogFooter>
            </form>
          )}
        </DialogContent>
      </Dialog>
      <ConfirmDialog
        open={Boolean(revoking)}
        onOpenChange={(o) => !o && setRevoking(undefined)}
        title="Revoke this token?"
        consequence="Everything using it is refused from now on."
        action="Revoke"
        pending={revoke.isPending}
        error={revoke.error}
        onConfirm={() => revoking && revoke.mutate(revoking.id)}
      />
    </Card>
  );
}

export function Account() {
  return (
    <Page title="Your account" description="Your password and your API tokens.">
      <div className="grid gap-6 xl:grid-cols-[24rem_1fr]">
        <Password />
        <Tokens />
      </div>
    </Page>
  );
}
