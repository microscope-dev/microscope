import "@fontsource/ibm-plex-sans/400.css";
import "@fontsource/ibm-plex-sans/500.css";
import "@fontsource/ibm-plex-sans/600.css";
import "@fontsource/jetbrains-mono/400.css";
import "@fontsource/jetbrains-mono/600.css";
import "./styles.css";
import { RouterProvider } from "@tanstack/react-router";
import { StrictMode } from "react";
import { createRoot } from "react-dom/client";

import { withinBase } from "./base";
import { browserAuthorized, browserConnect, leaveWhileCached } from "./connection";
import { forgetting } from "./forget";
import { createAppRouter } from "./router";
import { isString, read } from "./storage";
import { createSession, SessionContext } from "./store";
import { apply, savedTheme } from "./theme";

apply(savedTheme());

const session = createSession(
  { connect: browserConnect, authorized: browserAuthorized },
  (event) => {
    // A name chosen in an earlier visit is this browser's from now on.
    if (event.type === "message" && event.message.type === "hello") {
      const name = read("microscope-name", "", isString);
      if (name) {
        void session.connection.request("setName", { name }).catch(() => undefined);
      }
    }
  },
);
forgetting(session.store);
leaveWhileCached(session.connection, window);
// The join page connects once it has traded its token for a cookie.
if (withinBase(window.location.pathname) !== "/join") {
  session.connection.start();
}

const router = createAppRouter();
const root = document.getElementById("root");
if (root) {
  createRoot(root).render(
    <StrictMode>
      <SessionContext value={session}>
        <RouterProvider router={router} />
      </SessionContext>
    </StrictMode>,
  );
}
