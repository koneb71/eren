import {
  createContext,
  ReactNode,
  useCallback,
  useContext,
  useEffect,
  useState,
} from "react";
import { api, Workspace } from "./api";
import { readStored, writeStored } from "./storage";

interface WorkspaceCtx {
  workspaces: Workspace[];
  active: Workspace | null;
  setActive: (id: string) => void;
  refresh: () => Promise<void>;
}

const Ctx = createContext<WorkspaceCtx>({
  workspaces: [],
  active: null,
  setActive: () => {},
  refresh: async () => {},
});

const STORAGE_KEY = "eren.workspace";

export function WorkspaceProvider({ children }: { children: ReactNode }) {
  const [workspaces, setWorkspaces] = useState<Workspace[]>([]);
  const [activeId, setActiveId] = useState<string | null>(() =>
    readStored(STORAGE_KEY),
  );

  const refresh = useCallback(async () => {
    const { workspaces } = await api.workspaces();
    // Keep the old array (and therefore `active`'s identity) when nothing
    // actually changed. Every consumer that keys an effect on `active` sees
    // a new object otherwise, and pages that reset visible state in those
    // effects flicker — the chat thread blanking for a beat was this.
    setWorkspaces((prev) =>
      JSON.stringify(prev) === JSON.stringify(workspaces) ? prev : workspaces,
    );
    setActiveId((current) =>
      current && workspaces.some((w) => w.id === current)
        ? current
        : (workspaces[0]?.id ?? null),
    );
  }, []);

  useEffect(() => {
    refresh().catch(() => {});
  }, [refresh]);

  const setActive = useCallback((id: string) => {
    writeStored(STORAGE_KEY, id);
    setActiveId(id);
  }, []);

  const active = workspaces.find((w) => w.id === activeId) ?? null;
  return (
    <Ctx.Provider value={{ workspaces, active, setActive, refresh }}>
      {children}
    </Ctx.Provider>
  );
}

export const useWorkspace = () => useContext(Ctx);
