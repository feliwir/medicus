import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, Navigate, Outlet, useNavigate } from "@tanstack/react-router";
import {
  Activity,
  AlertCircle,
  Boxes,
  Database,
  FolderSearch,
  KeyRound,
  LayoutDashboard,
  ListChecks,
  LogOut,
  Monitor,
  Moon,
  Network,
  RotateCcw,
  Server,
  Settings2,
  Shield,
  Sun,
  UserCog,
  Users,
} from "lucide-react";
import { useState } from "react";
import { Alert } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { ApiError, api, waitForRestart } from "@/lib/api";
import { type Theme, setTheme, storedTheme } from "@/lib/theme";
import { cn } from "@/lib/utils";

const navigation = [
  { group: "Overview", items: [{ to: "/", label: "Dashboard", icon: LayoutDashboard }] },
  { group: "Data", items: [{ to: "/browse", label: "Browse", icon: FolderSearch }] },
  {
    group: "Configuration",
    items: [
      { to: "/config/nodes", label: "Nodes", icon: Server },
      { to: "/config/partitions", label: "Partitions", icon: Database },
      { to: "/config/peers", label: "Peers", icon: Network },
      { to: "/config/addons", label: "Addons", icon: Boxes },
      { to: "/config/settings", label: "Settings", icon: Settings2 },
    ],
  },
  {
    group: "Access",
    items: [
      { to: "/access/users", label: "Users", icon: Users },
      { to: "/access/roles", label: "Roles", icon: Shield },
      { to: "/access/principals", label: "Principals", icon: KeyRound },
    ],
  },
  {
    group: "Operations",
    items: [
      { to: "/operations/jobs", label: "Jobs", icon: ListChecks },
      { to: "/operations/events", label: "Events", icon: Activity },
    ],
  },
] as const;

function Sidebar() {
  return (
    // Material 3 navigation drawer: section headers in title small (14/20, medium), items with a
    // full-shape active indicator and label large text; 40dp items instead of 56dp (desktop density).
    <aside className="hidden w-64 shrink-0 flex-col border-r bg-sidebar md:flex">
      <div className="flex h-16 items-center gap-3 px-6">
        <img src="/favicon.svg" alt="" className="size-7" />
        <span className="text-base font-semibold tracking-tight">medicus</span>
      </div>
      <nav className="flex-1 overflow-y-auto px-3 pb-4" aria-label="Main">
        {navigation.map((section) => (
          <div key={section.group} className="pt-2 first:pt-0">
            <div className="flex h-10 items-center px-4 text-sm font-medium text-muted-foreground">{section.group}</div>
            <ul>
              {section.items.map((item) => (
                <li key={item.to}>
                  <Link
                    to={item.to}
                    activeOptions={{ exact: item.to === "/" }}
                    className="flex h-10 items-center gap-3 rounded-full px-4 text-sm font-medium text-foreground/80 hover:bg-foreground/8 data-[status=active]:bg-accent data-[status=active]:text-accent-foreground"
                  >
                    <item.icon className="size-[18px]" />
                    <span className="flex-1">{item.label}</span>
                  </Link>
                </li>
              ))}
            </ul>
          </div>
        ))}
      </nav>
    </aside>
  );
}

function ThemeToggle() {
  const [theme, set] = useState<Theme>(storedTheme);
  const next: Record<Theme, Theme> = { system: "light", light: "dark", dark: "system" };
  const Icon = { system: Monitor, light: Sun, dark: Moon }[theme];
  return (
    <Button
      variant="ghost"
      size="icon"
      aria-label={`Theme: ${theme}. Switch to ${next[theme]}.`}
      title={`Theme: ${theme}`}
      onClick={() => {
        setTheme(next[theme]);
        set(next[theme]);
      }}
    >
      <Icon />
    </Button>
  );
}

function RestartBanner({ sections }: { sections: string[] }) {
  const queryClient = useQueryClient();
  const [error, setError] = useState<string>();
  const restart = useMutation({
    mutationFn: async () => {
      await api.restart();
      await waitForRestart();
    },
    onSuccess: () => queryClient.invalidateQueries(),
    onError: (e) => setError(e.message),
  });
  return (
    <Alert variant="warning" className="items-center rounded-none border-x-0 border-t-0 px-6">
      <RotateCcw />
      <div className="flex-1">
        Changes to {sections.join(", ")} take effect after a restart.
        {error && <span className="ml-2 text-destructive">{error}</span>}
      </div>
      <Button variant="outline" disabled={restart.isPending} onClick={() => restart.mutate()}>
        {restart.isPending ? "Restarting…" : "Restart now"}
      </Button>
    </Alert>
  );
}

/** The archive index cannot be opened: the server runs with the admin UI and API only. */
function DegradedBanner({ degraded }: { degraded: { index: string; error: string } }) {
  return (
    <Alert variant="destructive" className="items-center rounded-none border-x-0 border-t-0 px-6">
      <AlertCircle />
      <div className="flex-1">
        <div className="font-medium">
          The archive database (<span className="font-mono">{degraded.index}</span>) is unavailable: the archive is
          offline.
        </div>
        <div className="text-xs break-words">{degraded.error}</div>
      </div>
      <Button variant="outline" asChild>
        <Link to="/config/addons">Change the database</Link>
      </Button>
    </Alert>
  );
}

/** The signed-in application: sidebar, top bar and the current screen. */
export function AppShell() {
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const me = useQuery({ queryKey: ["me"], queryFn: api.me });
  const canSystem = me.data?.permissions.includes("admin.system") ?? false;
  const system = useQuery({ queryKey: ["system"], queryFn: api.system, enabled: canSystem });
  const logout = useMutation({
    mutationFn: api.logout,
    onSettled: () => {
      queryClient.clear();
      void navigate({ to: "/login" });
    },
  });

  if (me.error instanceof ApiError) {
    if (me.error.code === "setup_required") return <Navigate to="/setup" />;
    if (me.error.status === 401) return <Navigate to="/login" />;
  }
  if (!me.data) {
    return (
      <div className="grid h-screen place-items-center text-muted-foreground">
        {me.error ? `The server cannot be reached: ${me.error.message}` : "Loading…"}
      </div>
    );
  }
  const user = me.data.user;
  const restartRequired = system.data?.restart_required ?? [];

  return (
    <div className="flex h-screen">
      <Sidebar />
      <div className="flex min-w-0 flex-1 flex-col">
        <header className="flex h-16 shrink-0 items-center gap-2 border-b bg-card px-6">
          <div className="flex-1 truncate text-base font-medium">{system.data?.name}</div>
          <ThemeToggle />
          <DropdownMenu>
            <DropdownMenuTrigger asChild>
              <Button variant="ghost" className="gap-2 pr-4 pl-1 text-foreground">
                <span className="grid size-8 place-items-center rounded-full bg-primary text-sm font-medium text-primary-foreground">
                  {(user.display_name ?? user.name).slice(0, 1).toUpperCase()}
                </span>
                {user.display_name ?? user.name}
              </Button>
            </DropdownMenuTrigger>
            <DropdownMenuContent align="end">
              <DropdownMenuLabel>
                Signed in as <span className="font-medium text-foreground">{user.name}</span>
                <div className="mt-1 flex flex-wrap gap-1">
                  {user.roles.map((r) => (
                    <Badge key={r} variant="outline">
                      {r}
                    </Badge>
                  ))}
                </div>
              </DropdownMenuLabel>
              <DropdownMenuSeparator />
              <DropdownMenuItem onSelect={() => void navigate({ to: "/account" })}>
                <UserCog /> Your account
              </DropdownMenuItem>
              <DropdownMenuItem onSelect={() => logout.mutate()}>
                <LogOut /> Sign out
              </DropdownMenuItem>
            </DropdownMenuContent>
          </DropdownMenu>
        </header>
        {system.data?.degraded && <DegradedBanner degraded={system.data.degraded} />}
        {restartRequired.length > 0 && <RestartBanner sections={restartRequired} />}
        <main className={cn("min-h-0 flex-1 overflow-y-auto")}>
          <Outlet />
        </main>
      </div>
    </div>
  );
}
