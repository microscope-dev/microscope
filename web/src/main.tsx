import "@fontsource/ibm-plex-sans/400.css";
import "@fontsource/ibm-plex-sans/500.css";
import "@fontsource/ibm-plex-sans/600.css";
import "@fontsource/jetbrains-mono/400.css";
import "@fontsource/jetbrains-mono/600.css";
import "./styles.css";
import { render } from "@solidjs/web";

import { withinBase } from "./base";
import { browserAuthorized, browserConnect, leaveWhileCached } from "./connection";
import { forgetting } from "./forget";
import { AppRouter } from "./router";
import { isString, read } from "./storage";
import { createSession, SessionContext } from "./store";
import { apply, savedTheme } from "./theme";

apply(savedTheme());

const session = createSession(
  { connect: browserConnect, authorized: browserAuthorized },
  (event) => {
    if (event.type === "message" && event.message.type === "hello") {
      const name = read("microscope-name", "", isString);
      if (name) {
        void session.connection.request("setName", { name }).catch(() => {});
      }
    }
  },
);
forgetting(session.store);
leaveWhileCached(session.connection, window);

if (withinBase(window.location.pathname) !== "/join") {
  session.connection.start();
}

const root = document.querySelector("#root");
if (root) {
  render(
    () => (
      <SessionContext value={session}>
        <AppRouter />
      </SessionContext>
    ),
    root,
  );
}
