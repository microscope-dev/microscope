// The tab's model in a store React components subscribe to, and the one
// connection that feeds it.

import { createContext, useContext } from "react";
import { createStore, type StoreApi, useStore } from "zustand";

import { Connection, type ConnectionOptions } from "./connection";
import { initialModel, type Model, type ModelEvent, reduce } from "./model";

export interface Session {
  store: StoreApi<Model>;
  connection: Connection;
}

/** Creates a store and a connection that feeds it. */
export function createSession(
  options: Omit<ConnectionOptions, "emit">,
  onEvent?: (event: ModelEvent) => void,
): Session {
  const store = createStore<Model>(() => initialModel);
  const connection = new Connection({
    ...options,
    emit: (event) => {
      store.setState((model) => reduce(model, event), true);
      onEvent?.(event);
    },
  });
  return { store, connection };
}

export const SessionContext = createContext<Session | null>(null);

function useSession(): Session {
  const session = useContext(SessionContext);
  if (!session) {
    throw new Error("components need a SessionContext");
  }
  return session;
}

/** Selects from the model, rerendering when the selection changes. */
export function useModel<T>(select: (model: Model) => T): T {
  return useStore(useSession().store, select);
}

export function useConnection(): Connection {
  return useSession().connection;
}

export function useStoreApi(): StoreApi<Model> {
  return useSession().store;
}
