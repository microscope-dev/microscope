import { createContext, useContext } from "solid-js";

import { Connection, type ConnectionOptions } from "./connection";
import { initialModel, type Model, type ModelEvent, reduce } from "./model";
import { createState, type Store, useSelector } from "./state";

export interface Session {
  store: Store<Model>;
  connection: Connection;
}

export function createSession(
  options: Omit<ConnectionOptions, "emit">,
  onEvent?: (event: ModelEvent) => void,
): Session {
  const store = createState<Model>(() => initialModel);
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

export function useModel<T>(
  select: (model: Model) => T,
  equals?: (a: T, b: T) => boolean,
): () => T {
  return useSelector(useSession().store, select, equals);
}

export function useConnection(): Connection {
  return useSession().connection;
}

export function useStoreApi(): Store<Model> {
  return useSession().store;
}
