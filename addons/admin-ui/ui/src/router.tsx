import { Outlet, createRootRoute, createRoute, createRouter } from "@tanstack/react-router";
import { AppShell } from "./components/app-shell";
import { Principals } from "./routes/access/principals";
import { Roles } from "./routes/access/roles";
import { Users } from "./routes/access/users";
import { Account } from "./routes/account";
import { Studies } from "./routes/browse/studies";
import { Study } from "./routes/browse/study";
import { Addons } from "./routes/config/addons";
import { Nodes } from "./routes/config/nodes";
import { Partitions } from "./routes/config/partitions";
import { Peers } from "./routes/config/peers";
import { Settings } from "./routes/config/settings";
import { Dashboard } from "./routes/dashboard";
import { Login } from "./routes/login";
import { Events } from "./routes/operations/events";
import { Jobs } from "./routes/operations/jobs";
import { Setup } from "./routes/setup";

// Screen paths. The core reserves /api, /health, /rs and /nodes (the DICOMweb proxy): a screen
// under them would not load on reload, so configuration lives under /config.

const rootRoute = createRootRoute({ component: Outlet });

const loginRoute = createRoute({ getParentRoute: () => rootRoute, path: "/login", component: Login });
const setupRoute = createRoute({ getParentRoute: () => rootRoute, path: "/setup", component: Setup });

/** Everything behind a login, inside the application shell. */
const appRoute = createRoute({ getParentRoute: () => rootRoute, id: "app", component: AppShell });

const dashboardRoute = createRoute({ getParentRoute: () => appRoute, path: "/", component: Dashboard });
const studiesRoute = createRoute({ getParentRoute: () => appRoute, path: "/browse", component: Studies });
const studyRoute = createRoute({ getParentRoute: () => appRoute, path: "/browse/$node/$study", component: Study });
const nodesRoute = createRoute({ getParentRoute: () => appRoute, path: "/config/nodes", component: Nodes });
const partitionsRoute = createRoute({ getParentRoute: () => appRoute, path: "/config/partitions", component: Partitions });
const peersRoute = createRoute({ getParentRoute: () => appRoute, path: "/config/peers", component: Peers });
const addonsRoute = createRoute({ getParentRoute: () => appRoute, path: "/config/addons", component: Addons });
const settingsRoute = createRoute({ getParentRoute: () => appRoute, path: "/config/settings", component: Settings });
const usersRoute = createRoute({ getParentRoute: () => appRoute, path: "/access/users", component: Users });
const rolesRoute = createRoute({ getParentRoute: () => appRoute, path: "/access/roles", component: Roles });
const principalsRoute = createRoute({ getParentRoute: () => appRoute, path: "/access/principals", component: Principals });
const jobsRoute = createRoute({ getParentRoute: () => appRoute, path: "/operations/jobs", component: Jobs });
const eventsRoute = createRoute({ getParentRoute: () => appRoute, path: "/operations/events", component: Events });
const accountRoute = createRoute({ getParentRoute: () => appRoute, path: "/account", component: Account });

const routeTree = rootRoute.addChildren([loginRoute, setupRoute, appRoute.addChildren([
    dashboardRoute,
    studiesRoute,
    studyRoute,
    nodesRoute,
    partitionsRoute,
    peersRoute,
    addonsRoute,
    settingsRoute,
    usersRoute,
    rolesRoute,
    principalsRoute,
    jobsRoute,
    eventsRoute,
    accountRoute,
  ])]);

export const router = createRouter({ routeTree, defaultPreload: "intent" });

declare module "@tanstack/react-router" {
  interface Register {
    router: typeof router;
  }
}
