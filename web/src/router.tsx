import { createRouter, defineRoutes } from "@solidjs/router";

import { type AppHistory, BASE_PREFIX, createBrowserHistory, HistoryContext } from "./history";
import { DashboardRoute } from "./ui/dashboards/DashboardRoutes";
import { Frame } from "./ui/Frame";
import { Join } from "./ui/Join";
import {
  EmailLink,
  Home,
  OrgSignIn,
  Pick,
  ProfileRoute,
  SessionRoute,
  SignedIn,
} from "./ui/Routes";
import { Workspace } from "./ui/Workspace";

export {
  type AppHistory,
  createBrowserHistory,
  createMemoryHistory,
  type HistoryAction,
  HistoryContext,
  type HistoryLocation,
  toHref,
  useHistory,
} from "./history";

export const routes = defineRoutes([
  { path: "/", component: Home },
  { path: "/join", component: Join },
  { path: "/pick", component: Pick },
  { path: "/confirm-email", component: () => <EmailLink link="confirm" /> },
  { path: "/login-link", component: () => <EmailLink link="login" /> },
  { path: "/reset-password", component: () => <EmailLink link="reset" /> },
  { path: "/signed-in", component: SignedIn },
  { path: "/sso", component: OrgSignIn },
  { path: "/d/:dashboard", component: DashboardRoute },
  { path: "/profile/:profile", component: ProfileRoute },
  {
    path: "/s/:session",
    component: SessionRoute,
    children: [
      { path: "/", component: Workspace },
      { path: "/live", component: Workspace },
      { path: "/stop/:stop/t/:thread/f/:frame", component: Workspace },
      { path: "/stop/:stop/task/:task/f/:frame", component: Workspace },
    ],
  },
]);

export function AppRouter(props: { history?: AppHistory }) {
  const history = props.history ?? createBrowserHistory();
  const Router = createRouter({
    routes,
    ...(BASE_PREFIX === "" ? {} : { base: BASE_PREFIX }),
    history,
  });
  return (
    <HistoryContext value={history}>
      <Router>{(section) => <Frame>{section.children}</Frame>}</Router>
    </HistoryContext>
  );
}
