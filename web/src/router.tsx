// The URL is the focus: each route is a place in the session that a link
// can name. The path names the debugger state, the query how a tab looks at
// it (see focus.ts).

import {
  createRootRoute,
  createRoute,
  createRouter,
  type RouterHistory,
} from "@tanstack/react-router";
import { base } from "./base";
import { parseSearch, stringifySearch, validateLook } from "./focus";
import { validateProfileLook } from "./profile/look";
import { Landing } from "./ui/Landing";
import { Picker, pickerSearch } from "./ui/Picker";
import { Home, Join, SessionPage } from "./ui/pages";
import { ProfilePage } from "./ui/profile/ProfilePage";
import { Shell } from "./ui/Shell";
import { Workspace } from "./ui/Workspace";

const rootRoute = createRootRoute({ component: Shell });

const homeRoute = createRoute({ getParentRoute: () => rootRoute, path: "/", component: Home });

const joinRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/join",
  validateSearch: (search: Record<string, unknown>): { to?: string } =>
    typeof search.to === "string" ? { to: search.to } : {},
  component: Join,
});

/** A signed-in user's landing page, on a page served on its own. */
const landingRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/home",
  component: Landing,
});

const pickRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/pick",
  validateSearch: pickerSearch,
  component: Picker,
});

const sessionRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/s/$session",
  validateSearch: validateLook,
  component: SessionPage,
});

/** The session wherever it is, following it. */
const sessionIndexRoute = createRoute({
  getParentRoute: () => sessionRoute,
  path: "/",
  component: Workspace,
});

/** The same, by a name that says so. */
const liveRoute = createRoute({
  getParentRoute: () => sessionRoute,
  path: "/live",
  component: Workspace,
});

const stopRoute = createRoute({
  getParentRoute: () => sessionRoute,
  path: "/stop/$stop/t/$thread/f/$frame",
  component: Workspace,
});

/** A task's stack at a stop, which no thread may be running. */
const taskRoute = createRoute({
  getParentRoute: () => sessionRoute,
  path: "/stop/$stop/task/$task/f/$frame",
  component: Workspace,
});

/** A profile the server serves, which belongs to no session: its id
 * names its bytes, so the link opens it wherever it is served. */
const profileRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/profile/$profile",
  validateSearch: validateProfileLook,
  component: ProfilePage,
});

const routeTree = rootRoute.addChildren([
  homeRoute,
  joinRoute,
  landingRoute,
  pickRoute,
  profileRoute,
  sessionRoute.addChildren([sessionIndexRoute, liveRoute, stopRoute, taskRoute]),
]);

export function createAppRouter(history?: RouterHistory) {
  return createRouter({
    routeTree,
    parseSearch,
    stringifySearch,
    ...(base === "/" ? {} : { basepath: base.slice(0, -1) }),
    ...(history ? { history } : {}),
  });
}

declare module "@tanstack/react-router" {
  interface Register {
    router: ReturnType<typeof createAppRouter>;
  }
}
