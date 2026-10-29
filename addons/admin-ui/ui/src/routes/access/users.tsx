import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import type { ColumnDef } from "@tanstack/react-table";
import { Pencil, Plus, Trash2 } from "lucide-react";
import { useState } from "react";
import { ConfirmDialog, ErrorAlert, Field, Page } from "@/components/page";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { DataTable } from "@/components/ui/data-table";
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogTitle } from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Switch } from "@/components/ui/switch";
import { type UserInfo, api } from "@/lib/api";
import type { RoleItem } from "@/lib/items";
import { useSection } from "@/lib/settings";

interface UserForm {
  name: string;
  displayName: string;
  password: string;
  roles: string[];
  disabled: boolean;
}

function RolePicker({ roles, onChange }: { roles: string[]; onChange: (roles: string[]) => void }) {
  const configured = useSection<RoleItem>("roles");
  const names = ["admin", ...Object.keys(configured.data ?? {})];
  return (
    <div className="space-y-1">
      <div className="text-sm font-medium">Roles</div>
      <div className="flex flex-wrap gap-x-6">
        {names.map((r) => (
          <Checkbox
            key={r}
            label={r === "admin" ? "admin (everything)" : r}
            checked={roles.includes(r)}
            onChange={(e) => onChange(e.target.checked ? [...roles, r] : roles.filter((x) => x !== r))}
          />
        ))}
      </div>
    </div>
  );
}

export function Users() {
  const queryClient = useQueryClient();
  const users = useQuery({ queryKey: ["users"], queryFn: api.users });
  const [editing, setEditing] = useState<{ form: UserForm; isNew: boolean }>();
  const [deleting, setDeleting] = useState<string>();
  const refresh = () => queryClient.invalidateQueries({ queryKey: ["users"] });

  const save = useMutation({
    mutationFn: async ({ form, isNew }: { form: UserForm; isNew: boolean }) => {
      if (isNew) {
        await api.createUser({
          name: form.name.trim(),
          password: form.password || undefined,
          display_name: form.displayName.trim() || undefined,
          roles: form.roles,
        });
      } else {
        await api.updateUser(form.name, {
          display_name: form.displayName.trim(),
          roles: form.roles,
          disabled: form.disabled,
          password: form.password || undefined,
        });
      }
    },
    onSuccess: async () => {
      setEditing(undefined);
      await refresh();
    },
  });
  const remove = useMutation({
    mutationFn: (name: string) => api.deleteUser(name),
    onSuccess: async () => {
      setDeleting(undefined);
      await refresh();
    },
  });

  const open = (u?: UserInfo) => {
    save.reset();
    setEditing(
      u
        ? { form: { name: u.name, displayName: u.display_name ?? "", password: "", roles: u.roles, disabled: u.disabled }, isNew: false }
        : { form: { name: "", displayName: "", password: "", roles: [], disabled: false }, isNew: true },
    );
  };

  const columns: ColumnDef<UserInfo, unknown>[] = [
    {
      accessorKey: "name",
      header: "User",
      cell: ({ row }) => (
        <div>
          <div className="font-medium">{row.original.name}</div>
          {row.original.display_name && <div className="text-xs text-muted-foreground">{row.original.display_name}</div>}
        </div>
      ),
    },
    {
      id: "roles",
      header: "Roles",
      enableSorting: false,
      cell: ({ row }) => (
        <div className="flex flex-wrap gap-1">
          {row.original.roles.length ? row.original.roles.map((r) => <Badge key={r}>{r}</Badge>) : <span className="text-muted-foreground">none</span>}
        </div>
      ),
    },
    {
      id: "signin",
      header: "Signs in with",
      accessorFn: (u) => (u.password ? "password" : "tokens"),
      cell: ({ getValue }) => <span className="text-muted-foreground">{getValue() === "password" ? "password or tokens" : "API tokens only"}</span>,
    },
    {
      id: "status",
      header: "Status",
      accessorFn: (u) => (u.disabled ? "disabled" : "active"),
      cell: ({ row }) => (row.original.disabled ? <Badge variant="destructive">disabled</Badge> : <Badge variant="success">active</Badge>),
    },
    {
      id: "actions",
      header: "",
      enableSorting: false,
      cell: ({ row }) => (
        <div className="flex justify-end gap-1" onClick={(e) => e.stopPropagation()}>
          <Button variant="ghost" size="icon" aria-label={`Edit ${row.original.name}`} onClick={() => open(row.original)}>
            <Pencil />
          </Button>
          <Button
            variant="ghost"
            size="icon"
            aria-label={`Delete ${row.original.name}`}
            onClick={() => {
              remove.reset();
              setDeleting(row.original.name);
            }}
          >
            <Trash2 />
          </Button>
        </div>
      ),
    },
  ];

  const form = editing?.form;
  const problem = !form
    ? undefined
    : !form.name.trim()
      ? "Give the user a name."
      : form.password && form.password.length < 8
        ? "Passwords have at least 8 characters."
        : undefined;

  return (
    <Page
      title="Users"
      description="Accounts that sign in to this interface or use the APIs with tokens. What they may do comes from their roles."
      actions={
        <Button onClick={() => open()}>
          <Plus /> Add user
        </Button>
      }
    >
      <ErrorAlert error={users.error} />
      <DataTable columns={columns} data={users.data ?? []} rowKey={(u) => u.name} onRowClick={open} empty={users.isLoading ? "Loading…" : "No users."} />

      <Dialog open={Boolean(editing)} onOpenChange={(o) => !o && setEditing(undefined)}>
        {editing && form && (
          <DialogContent>
            <DialogTitle>{editing.isNew ? "Add user" : form.name}</DialogTitle>
            <DialogDescription>
              {editing.isNew ? "Without a password, the user can only use API tokens." : "Leave the password empty to keep it."}
            </DialogDescription>
            <form
              className="space-y-4"
              onSubmit={(e) => {
                e.preventDefault();
                if (!problem) save.mutate(editing);
              }}
            >
              <div className="grid gap-4 sm:grid-cols-2">
                <Field id="u-name" label="User name">
                  <Input
                    id="u-name"
                    autoFocus={editing.isNew}
                    disabled={!editing.isNew}
                    value={form.name}
                    onChange={(e) => setEditing({ ...editing, form: { ...form, name: e.target.value } })}
                  />
                </Field>
                <Field id="u-display" label="Display name">
                  <Input
                    id="u-display"
                    value={form.displayName}
                    onChange={(e) => setEditing({ ...editing, form: { ...form, displayName: e.target.value } })}
                  />
                </Field>
              </div>
              <Field id="u-password" label={editing.isNew ? "Password" : "New password"} hint="At least 8 characters.">
                <Input
                  id="u-password"
                  type="password"
                  autoComplete="new-password"
                  value={form.password}
                  onChange={(e) => setEditing({ ...editing, form: { ...form, password: e.target.value } })}
                />
              </Field>
              <RolePicker roles={form.roles} onChange={(roles) => setEditing({ ...editing, form: { ...form, roles } })} />
              {!editing.isNew && (
                <label className="flex min-h-10 items-center gap-3 text-sm">
                  <Switch
                    checked={!form.disabled}
                    aria-label="Active"
                    onCheckedChange={(on) => setEditing({ ...editing, form: { ...form, disabled: !on } })}
                  />
                  Active — a disabled user cannot sign in, and their sessions end.
                </label>
              )}
              {problem && form.name && <p className="text-sm text-destructive">{problem}</p>}
              <ErrorAlert error={save.error} />
              <DialogFooter>
                <Button type="button" variant="ghost" onClick={() => setEditing(undefined)}>
                  Cancel
                </Button>
                <Button type="submit" disabled={Boolean(problem) || save.isPending}>
                  {save.isPending ? "Saving…" : "Save"}
                </Button>
              </DialogFooter>
            </form>
          </DialogContent>
        )}
      </Dialog>

      <ConfirmDialog
        open={Boolean(deleting)}
        onOpenChange={(o) => !o && setDeleting(undefined)}
        title={`Delete ${deleting ?? ""}?`}
        consequence="The account, its sessions and its API tokens are deleted. This cannot be undone."
        action="Delete user"
        pending={remove.isPending}
        error={remove.error}
        onConfirm={() => deleting && remove.mutate(deleting)}
      />
    </Page>
  );
}
