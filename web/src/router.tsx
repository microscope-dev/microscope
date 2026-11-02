// The URL is the focus: each route is a place that a link can name, which
// the one workbench opens (Routes.tsx). The path names the debugger state,
// the query how a tab looks at it (see focus.ts).

import {
  createRootRoute,
  createRoute,
  createRouter,
  type RouterHistory,
} from "@tanstack/react-router";
import { base } from "./base";
import { parseSearch, stringifySearch, validateLook } from "./focus";
import { validateProfileLook } from "./profile/look";
import { Frame } from "./ui/Frame";
import { Join } from "./ui/Join";
import { pickerSearch } from "./ui/Picker";
import { EmailLink, Home, Pick, ProfileRoute, SessionRoute } from "./ui/Routes";
import { Workspace } from "./ui/Workspace";

const rootRoute = createRootRoute({ component: Frame });

const homeRoute = createRoute({ getParentRoute: () => rootRoute, path: "/", component: Home });

const joinRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/join",
  validateSearch: (search: Record<string, unknown>): { to?: string } =>
    typeof search.to === "string" ? { to: search.to } : {},
  component: Join,
});

const pickRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/pick",
  validateSearch: pickerSearch,
  component: Pick,
});

// Services' emails link to these.
const confirmEmailRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/confirm-email",
  component: () => <EmailLink link="confirm" />,
});

const loginLinkRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/login-link",
  component: () => <EmailLink link="login" />,
});

const sessionRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/s/$session",
  validateSearch: validateLook,
  component: SessionRoute,
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
  component: ProfileRoute,
});

const routeTree = rootRoute.addChildren([
  homeRoute,
  joinRoute,
  pickRoute,
  confirmEmailRoute,
  loginLinkRoute,
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
