import type { ColumnDef } from "@tanstack/react-table";
import { Pencil, Plus, Trash2 } from "lucide-react";
import { type ReactNode, useState } from "react";
import { ConfirmDialog, ErrorAlert, Field, Page } from "@/components/page";
import { Button } from "@/components/ui/button";
import { DataTable } from "@/components/ui/data-table";
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogTitle } from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { type Section, api } from "@/lib/api";
import { useSection, useSettingsMutation } from "@/lib/settings";

export type Entry<T> = { name: string; item: T };

export interface SectionEditor<T, F> {
  section: Section;
  title: string;
  description: ReactNode;
  /** "node", "peer", ... */
  singular: string;
  columns: ColumnDef<Entry<T>, unknown>[];
  blank: () => F;
  toForm: (item: T) => F;
  fromForm: (form: F) => unknown;
  Form: (props: { value: F; onChange: (form: F) => void }) => ReactNode;
  /** Problems the form can tell without the server. */
  validate?: (form: F) => string | undefined;
  /** What deleting the item breaks. */
  consequence?: (name: string) => ReactNode;
  /** Extra actions per row. */
  rowActions?: (entry: Entry<T>) => ReactNode;
  /** Shown above the table. */
  notice?: ReactNode;
}

/** A settings section as a table: add, edit and delete items, each change a new revision. */
export function SectionScreen<T, F>(editor: SectionEditor<T, F>) {
  const items = useSection<T>(editor.section);
  const [editing, setEditing] = useState<{ name: string; form: F; isNew: boolean }>();
  const [deleting, setDeleting] = useState<string>();
  const [saved, setSaved] = useState<string>();

  const save = useSettingsMutation(({ name, form }: { name: string; form: F }) =>
    api.putItem(editor.section, name, editor.fromForm(form)),
  );
  const remove = useSettingsMutation((name: string) => api.deleteItem(editor.section, name));

  const entries: Entry<T>[] = Object.entries(items.data ?? {}).map(([name, item]) => ({ name, item }));
  const columns: ColumnDef<Entry<T>, unknown>[] = [
    ...editor.columns,
    {
      id: "actions",
      header: "",
      enableSorting: false,
      cell: ({ row }) => (
        <div className="flex justify-end gap-1" onClick={(e) => e.stopPropagation()}>
          {editor.rowActions?.(row.original)}
          <Button
            variant="ghost"
            size="icon"
            aria-label={`Edit ${row.original.name}`}
            onClick={() => open(row.original)}
          >
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

  const open = (entry?: Entry<T>) => {
    save.reset();
    setSaved(undefined);
    setEditing(
      entry
        ? { name: entry.name, form: editor.toForm(entry.item), isNew: false }
        : { name: "", form: editor.blank(), isNew: true },
    );
  };

  const problem = editing
    ? !editing.name.trim()
      ? `Give the ${editor.singular} a name.`
      : editing.isNew && items.data && editing.name.trim() in items.data
        ? `There is a ${editor.singular} of that name already.`
        : editor.validate?.(editing.form)
    : undefined;

  return (
    <Page
      title={editor.title}
      description={editor.description}
      actions={
        <Button onClick={() => open()}>
          <Plus /> Add {editor.singular}
        </Button>
      }
    >
      {editor.notice}
      {saved && <p className="text-sm text-success">{saved}</p>}
      <ErrorAlert error={items.error} />
      <DataTable
        columns={columns}
        data={entries}
        rowKey={(e) => e.name}
        onRowClick={(e) => open(e)}
        empty={items.isLoading ? "Loading…" : `No ${editor.singular}s configured.`}
      />

      <Dialog open={Boolean(editing)} onOpenChange={(o) => !o && setEditing(undefined)}>
        {editing && (
          <DialogContent>
            <DialogTitle>{editing.isNew ? `Add ${editor.singular}` : `${editing.name}`}</DialogTitle>
            <DialogDescription>
              {editing.isNew ? `A new ${editor.singular}.` : `Change the ${editor.singular}.`} The change is stored as a
              new settings revision.
            </DialogDescription>
            <form
              className="space-y-4"
              onSubmit={(e) => {
                e.preventDefault();
                if (problem) return;
                save.mutate(
                  { name: editing.name.trim(), form: editing.form },
                  {
                    onSuccess: (applied) => {
                      setEditing(undefined);
                      setSaved(
                        `Saved as revision ${applied.revision}.` +
                          (applied.restart_required.length ? " Some changes wait for a restart." : ""),
                      );
                    },
                  },
                );
              }}
            >
              {editing.isNew && (
                <Field id="item-name" label="Name">
                  <Input
                    id="item-name"
                    autoFocus
                    value={editing.name}
                    onChange={(e) => setEditing({ ...editing, name: e.target.value })}
                  />
                </Field>
              )}
              <editor.Form value={editing.form} onChange={(form) => setEditing({ ...editing, form })} />
              {problem && editing.name && <p className="text-sm text-destructive">{problem}</p>}
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
        consequence={deleting && (editor.consequence?.(deleting) ?? `The ${editor.singular} is removed from the settings.`)}
        action="Delete"
        pending={remove.isPending}
        error={remove.error}
        onConfirm={() =>
          deleting &&
          remove.mutate(deleting, {
            onSuccess: (applied) => {
              setDeleting(undefined);
              setSaved(`Deleted; settings revision ${applied.revision}.`);
            },
          })
        }
      />
    </Page>
  );
}
