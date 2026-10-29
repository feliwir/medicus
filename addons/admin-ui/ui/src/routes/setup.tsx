import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Navigate, useNavigate } from "@tanstack/react-router";
import { AlertCircle, Check, Terminal } from "lucide-react";
import { type FormEvent, type ReactNode, useState } from "react";
import { Alert } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardDescription, CardFooter, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Switch } from "@/components/ui/switch";
import { api, waitForRestart } from "@/lib/api";
import { type SetupChoices, defaultChoices, planSettings, plannedAddons, validate } from "@/lib/setup";
import { cn } from "@/lib/utils";
import { Centered } from "./login";

const steps = ["Token", "Administrator", "Server", "Network", "First node", "Summary"] as const;

function Progress({ step }: { step: number }) {
  return (
    <ol className="flex items-center gap-2" aria-label="Setup progress">
      {steps.map((name, i) => (
        <li key={name} className="flex flex-1 flex-col gap-2" aria-current={i === step ? "step" : undefined}>
          <div className={cn("h-1 rounded-full", i <= step ? "bg-primary" : "bg-muted")} />
          <span className={cn("text-xs", i === step ? "font-medium" : "text-muted-foreground")}>{name}</span>
        </li>
      ))}
    </ol>
  );
}

function Field({
  id,
  label,
  hint,
  error,
  children,
}: {
  id: string;
  label: string;
  hint?: ReactNode;
  error?: string;
  children: ReactNode;
}) {
  return (
    <div className="space-y-2">
      <Label htmlFor={id}>{label}</Label>
      {children}
      {error ? (
        <p className="text-xs text-destructive">{error}</p>
      ) : (
        hint && <p className="text-xs text-muted-foreground">{hint}</p>
      )}
    </div>
  );
}

export function Setup() {
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const status = useQuery({ queryKey: ["setup"], queryFn: api.setupStatus });
  const [step, setStep] = useState(0);
  const [token, setToken] = useState("");
  const [admin, setAdmin] = useState({ name: "admin", displayName: "", password: "", repeat: "" });
  const [choices, setChoices] = useState<SetupChoices>(defaultChoices);
  const [busy, setBusy] = useState<string>();
  const [error, setError] = useState<string>();
  const errors = validate(choices);

  // Once an administrator exists the wizard continues as that user; before, it needs setup mode.
  if (step < 2 && status.data && !status.data.required) return <Navigate to="/login" />;

  const update = (change: Partial<SetupChoices>) => setChoices((c) => ({ ...c, ...change }));

  const createAdmin = async () => {
    if (admin.password !== admin.repeat) {
      setError("The passwords differ.");
      return;
    }
    setBusy("Creating the administrator…");
    try {
      await api.setup({
        token: token.trim(),
        name: admin.name.trim(),
        password: admin.password,
        display_name: admin.displayName.trim() || undefined,
      });
      setStep(2);
    } catch (e) {
      setError((e as Error).message);
      // A wrong token is fixed on the first step.
      if ((e as { status?: number }).status === 403) setStep(0);
    } finally {
      setBusy(undefined);
    }
  };

  const finish = async () => {
    setBusy("Saving the settings…");
    try {
      const current = await api.settings();
      await api.putSettings(current.revision, planSettings(current.settings, choices), "setup");
      setBusy("Restarting the server…");
      await api.restart();
      await waitForRestart();
      await queryClient.invalidateQueries();
      void navigate({ to: "/" });
    } catch (e) {
      setError((e as Error).message);
      setBusy(undefined);
    }
  };

  const fieldsOfStep: Record<number, string[]> = {
    2: ["serverName"],
    3: ["dimsePort", "dicomwebPort"],
    4: ["nodeName", "aeTitle", "dicomwebPath"],
  };
  const stepValid =
    step === 0
      ? token.trim().length > 0
      : step === 1
        ? admin.name.trim().length > 0 && admin.password.length >= 8 && admin.repeat.length > 0
        : !(fieldsOfStep[step] ?? []).some((f) => errors[f]);

  const next = (e: FormEvent) => {
    e.preventDefault();
    setError(undefined);
    if (!stepValid || busy) return;
    if (step === 1) void createAdmin();
    else if (step === steps.length - 1) void finish();
    else setStep(step + 1);
  };

  return (
    <Centered wide>
      <form onSubmit={next}>
        <Card>
          <CardHeader className="gap-4">
            <Progress step={step} />
            <div>
              <CardTitle className="text-headline-sm font-normal">{titles[step]}</CardTitle>
              <CardDescription>{descriptions[step]}</CardDescription>
            </div>
          </CardHeader>
          <CardContent className="space-y-5">
            {step === 0 && (
              <>
                <Alert variant="info">
                  <Terminal />
                  <div>
                    The server printed a one-time setup token on its console when it started, like{" "}
                    <code className="font-mono">setup token: mdsetup_…</code>.
                  </div>
                </Alert>
                <Field id="token" label="Setup token">
                  <Input
                    id="token"
                    className="font-mono"
                    autoFocus
                    value={token}
                    onChange={(e) => setToken(e.target.value)}
                  />
                </Field>
              </>
            )}
            {step === 1 && (
              <>
                <div className="grid gap-4 sm:grid-cols-2">
                  <Field id="admin-name" label="User name">
                    <Input
                      id="admin-name"
                      autoComplete="username"
                      value={admin.name}
                      onChange={(e) => setAdmin({ ...admin, name: e.target.value })}
                    />
                  </Field>
                  <Field id="admin-display" label="Display name" hint="Optional.">
                    <Input
                      id="admin-display"
                      value={admin.displayName}
                      onChange={(e) => setAdmin({ ...admin, displayName: e.target.value })}
                    />
                  </Field>
                </div>
                <div className="grid gap-4 sm:grid-cols-2">
                  <Field id="admin-password" label="Password" hint="At least 8 characters.">
                    <Input
                      id="admin-password"
                      type="password"
                      autoComplete="new-password"
                      value={admin.password}
                      onChange={(e) => setAdmin({ ...admin, password: e.target.value })}
                    />
                  </Field>
                  <Field id="admin-repeat" label="Repeat the password">
                    <Input
                      id="admin-repeat"
                      type="password"
                      autoComplete="new-password"
                      value={admin.repeat}
                      onChange={(e) => setAdmin({ ...admin, repeat: e.target.value })}
                    />
                  </Field>
                </div>
              </>
            )}
            {step === 2 && (
              <>
                <Field id="server-name" label="Server name" error={errors.serverName} hint="Shown in the interface and the API.">
                  <Input id="server-name" value={choices.serverName} onChange={(e) => update({ serverName: e.target.value })} />
                </Field>
                <Field
                  id="storage"
                  label="Storage location"
                  hint="A directory on the server for the DICOM files. Empty: inside the server's data directory."
                >
                  <Input
                    id="storage"
                    className="font-mono"
                    placeholder="data/blobs"
                    value={choices.storagePath}
                    onChange={(e) => update({ storagePath: e.target.value })}
                  />
                </Field>
              </>
            )}
            {step === 3 && (
              <div className="space-y-3">
                {(
                  [
                    ["dimse", "DIMSE", "C-STORE, C-FIND, C-MOVE and C-GET for modalities and other PACS."],
                    ["dicomweb", "DICOMweb", "QIDO-RS, WADO-RS and STOW-RS; also what the data browser uses."],
                  ] as const
                ).map(([key, name, what]) => (
                  <div key={key} className="flex items-start gap-4 rounded-md border p-4">
                    <Switch
                      id={`${key}-enabled`}
                      aria-label={`Enable ${name}`}
                      checked={choices[key].enabled}
                      onCheckedChange={(enabled) => update({ [key]: { ...choices[key], enabled } })}
                    />
                    <div className="flex-1 space-y-2">
                      <div>
                        <div className="text-base font-medium">{name}</div>
                        <div className="text-xs text-muted-foreground">{what}</div>
                      </div>
                      {choices[key].enabled && (
                        <Field id={`${key}-port`} label="Port" error={errors[`${key}Port`]}>
                          <Input
                            id={`${key}-port`}
                            type="number"
                            className="w-32 font-mono"
                            value={choices[key].port}
                            onChange={(e) => update({ [key]: { ...choices[key], port: Number(e.target.value) } })}
                          />
                        </Field>
                      )}
                    </div>
                  </div>
                ))}
              </div>
            )}
            {step === 4 && (
              <>
                <Field id="node-name" label="Node name" error={errors.nodeName}>
                  <Input
                    id="node-name"
                    value={choices.node.name}
                    onChange={(e) => update({ node: { ...choices.node, name: e.target.value } })}
                  />
                </Field>
                <div className="grid gap-4 sm:grid-cols-2">
                  {choices.dimse.enabled && (
                    <Field id="ae-title" label="AE title" error={errors.aeTitle} hint="What DIMSE peers call.">
                      <Input
                        id="ae-title"
                        className="font-mono"
                        value={choices.node.aeTitle}
                        onChange={(e) => update({ node: { ...choices.node, aeTitle: e.target.value.toUpperCase() } })}
                      />
                    </Field>
                  )}
                  {choices.dicomweb.enabled && (
                    <Field
                      id="dicomweb-path"
                      label="DICOMweb path"
                      error={errors.dicomwebPath}
                      hint={
                        <>
                          Served at <code className="font-mono">/nodes/{choices.node.dicomwebPath || "…"}/rs</code>.
                        </>
                      }
                    >
                      <Input
                        id="dicomweb-path"
                        className="font-mono"
                        value={choices.node.dicomwebPath}
                        onChange={(e) => update({ node: { ...choices.node, dicomwebPath: e.target.value } })}
                      />
                    </Field>
                  )}
                </div>
                <p className="text-xs text-muted-foreground">
                  The node stores into the <code className="font-mono">default</code> partition. More nodes and partitions
                  can be added later.
                </p>
              </>
            )}
            {step === 5 && <Summary choices={choices} />}
            {busy && <p className="text-sm text-muted-foreground">{busy}</p>}
            {error && (
              <Alert variant="destructive">
                <AlertCircle />
                {error}
              </Alert>
            )}
          </CardContent>
          <CardFooter className="justify-between">
            <Button
              type="button"
              variant="ghost"
              disabled={step === 0 || step === 2 || Boolean(busy)}
              onClick={() => {
                setError(undefined);
                setStep(step - 1);
              }}
            >
              Back
            </Button>
            <Button type="submit" disabled={!stepValid || Boolean(busy)}>
              {step === 1 ? "Create administrator" : step === steps.length - 1 ? "Finish & restart" : "Next"}
            </Button>
          </CardFooter>
        </Card>
      </form>
    </Centered>
  );
}

const titles = [
  "Welcome to medicus",
  "Create the administrator",
  "Server and storage",
  "Network",
  "First node",
  "Summary",
];

const descriptions = [
  "Set up the archive in a few steps.",
  "The first account; it can do everything, including creating more users.",
  "Where the archive keeps its files.",
  "The protocols the archive speaks.",
  "A node is how the archive appears to its peers: an AE title and a DICOMweb path.",
  "The server applies these settings and restarts.",
];

function Summary({ choices }: { choices: SetupChoices }) {
  const rows: [string, ReactNode][] = [
    ["Server", choices.serverName],
    ["Storage", choices.storagePath || "inside the data directory"],
    [
      "Node",
      <span key="node">
        {choices.node.name}
        {choices.dimse.enabled && (
          <span className="text-muted-foreground"> · AE {choices.node.aeTitle}</span>
        )}
        {choices.dicomweb.enabled && (
          <span className="text-muted-foreground"> · /nodes/{choices.node.dicomwebPath}/rs</span>
        )}
      </span>,
    ],
  ];
  return (
    <div className="space-y-4">
      <dl className="grid grid-cols-[8rem_1fr] gap-y-2 text-sm">
        {rows.map(([k, v]) => (
          <div key={k} className="contents">
            <dt className="text-muted-foreground">{k}</dt>
            <dd>{v}</dd>
          </div>
        ))}
      </dl>
      <div>
        <div className="mb-2 text-sm font-medium">Addons</div>
        <ul className="divide-y rounded-md border">
          {plannedAddons(choices).map((a) => (
            <li key={a.name} className="flex min-h-12 items-center gap-3 px-4 py-2 text-sm">
              <Check className="size-[18px] text-success" />
              <span className="font-mono">{a.name}</span>
              <Badge variant="outline" className="ml-auto">
                {a.why}
              </Badge>
            </li>
          ))}
        </ul>
      </div>
    </div>
  );
}
