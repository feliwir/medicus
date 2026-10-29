import { useQuery } from "@tanstack/react-query";
import { Settings2 } from "lucide-react";
import { useState } from "react";
import { ErrorAlert, Page } from "@/components/page";
import { SchemaForm } from "@/components/schema-form";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogTitle } from "@/components/ui/dialog";
import { Switch } from "@/components/ui/switch";
import { Textarea } from "@/components/ui/textarea";
import { type AddonInfo, api } from "@/lib/api";
import { useSettingsMutation } from "@/lib/settings";

/** Why an addon cannot be switched off (or on), if it cannot. */
function locked(a: AddonInfo): string | undefined {
  if (a.required) return "Required: always enabled.";
  if (a.bootstrap) return "The index: configured in the configuration file.";
  if (a.selected && a.required_by.length) return `Needed by ${a.required_by.join(", ")}.`;
  return undefined;
}

function status(a: AddonInfo) {
  const running = Boolean(a.loaded_by);
  if (running && a.selected) return <Badge variant="success">running</Badge>;
  if (running) return <Badge variant="warning">stops after restart</Badge>;
  if (a.selected) return <Badge variant="warning">starts after restart</Badge>;
  return <Badge variant="outline">off</Badge>;
}

function Configure({ addon, onClose }: { addon: AddonInfo; onClose: () => void }) {
  const [config, setConfig] = useState<Record<string, unknown>>(
    addon.config && typeof addon.config === "object" ? (addon.config as Record<string, unknown>) : {},
  );
  const [raw, setRaw] = useState(false);
  const [text, setText] = useState(() => JSON.stringify(config, null, 2));
  const [jsonError, setJsonError] = useState<string>();
  const save = useSettingsMutation(() =>
    api.putAddon(addon.name, { config: Object.keys(config).length ? config : null }),
  );
  return (
    <Dialog open onOpenChange={(o) => !o && onClose()}>
      <DialogContent className="max-w-2xl">
        <DialogTitle>{addon.name}</DialogTitle>
        <DialogDescription>
          Empty fields use the addon's defaults. The configuration is checked against the addon's schema and takes effect
          after a restart.
        </DialogDescription>
        <form
          className="space-y-4"
          onSubmit={(e) => {
            e.preventDefault();
            save.mutate(undefined, { onSuccess: onClose });
          }}
        >
          {raw || !addon.config_schema ? (
            <>
              <Textarea
                className="min-h-64 font-mono text-xs"
                value={text}
                onChange={(e) => {
                  setText(e.target.value);
                  try {
                    setConfig(e.target.value.trim() ? JSON.parse(e.target.value) : {});
                    setJsonError(undefined);
                  } catch {
                    setJsonError("Not valid JSON.");
                  }
                }}
              />
              {jsonError && <p className="text-sm text-destructive">{jsonError}</p>}
            </>
          ) : (
            <SchemaForm schema={addon.config_schema} value={config} onChange={setConfig} />
          )}
          <ErrorAlert error={save.error} />
          <DialogFooter className="justify-between">
            {addon.config_schema ? (
              <Button
                type="button"
                variant="ghost"
                onClick={() => {
                  setText(JSON.stringify(config, null, 2));
                  setRaw(!raw);
                }}
              >
                {raw ? "Edit as form" : "Edit as JSON"}
              </Button>
            ) : (
              <span />
            )}
            <div className="flex gap-2">
              <Button type="button" variant="ghost" onClick={onClose}>
                Cancel
              </Button>
              <Button type="submit" disabled={Boolean(jsonError) || save.isPending}>
                {save.isPending ? "Saving…" : "Save"}
              </Button>
            </div>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}

export function Addons() {
  const addons = useQuery({ queryKey: ["addons"], queryFn: api.addons });
  const toggle = useSettingsMutation(({ name, enabled }: { name: string; enabled: boolean }) =>
    api.putAddon(name, { enabled }),
  );
  const [configuring, setConfiguring] = useState<AddonInfo>();
  return (
    <Page
      title="Addons"
      description="What the server is built from. Enabling an addon pulls in what it needs; changes take effect after a restart."
    >
      <ErrorAlert error={addons.error ?? toggle.error} />
      <div className="grid gap-4 lg:grid-cols-2">
        {(addons.data ?? []).map((a) => {
          const reason = locked(a);
          return (
            <Card key={a.name} className="flex flex-col gap-3 p-4">
              <div className="flex items-start gap-3">
                <div className="min-w-0 flex-1">
                  <div className="flex flex-wrap items-center gap-2">
                    <span className="font-mono text-base font-medium">{a.name}</span>
                    <span className="text-xs text-muted-foreground">{a.version}</span>
                    {status(a)}
                  </div>
                  <div className="mt-1 flex flex-wrap gap-1">
                    {a.provides.map((p) => (
                      <Badge key={p} variant="outline">
                        {p.replace("_", " ")}
                      </Badge>
                    ))}
                  </div>
                </div>
                <Switch
                  aria-label={`${a.selected ? "Disable" : "Enable"} ${a.name}`}
                  checked={a.selected}
                  disabled={Boolean(reason) || toggle.isPending}
                  onCheckedChange={(enabled) => toggle.mutate({ name: a.name, enabled })}
                />
              </div>
              <div className="text-sm text-muted-foreground">
                {reason ??
                  (a.requires.length
                    ? `Needs ${a.requires.map((r) => (r.addon ? `${r.capability} from ${r.addon}` : r.capability).replace("_", " ")).join(", ")}.`
                    : "Needs nothing else.")}
              </div>
              {!a.bootstrap && (a.config_schema || (a.config !== null && a.config !== undefined)) && (
                <div>
                  <Button variant="outline" size="sm" onClick={() => setConfiguring(a)}>
                    <Settings2 /> Configure
                  </Button>
                </div>
              )}
            </Card>
          );
        })}
      </div>
      {configuring && <Configure addon={configuring} onClose={() => setConfiguring(undefined)} />}
    </Page>
  );
}
