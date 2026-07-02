import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { AuthInfo, InstanceId } from "../types";

interface UseAuthSwitchReturn {
  authSources: AuthInfo[];
  refresh: () => Promise<void>;
  copyAuth: (sourceId: InstanceId, targetId: InstanceId) => Promise<boolean>;
  clearAuth: (id: InstanceId) => Promise<void>;
}

export function useAuthSwitch(): UseAuthSwitchReturn {
  const [authSources, setAuthSources] = useState<AuthInfo[]>([]);

  const refresh = useCallback(async () => {
    try {
      const list = await invoke<AuthInfo[]>("list_auth_sources");
      setAuthSources(list);
    } catch (e) {
      console.error("list_auth_sources failed", e);
    }
  }, []);

  useEffect(() => {
    refresh();
  }, [refresh]);

  const copyAuth = useCallback(
    async (sourceId: InstanceId, targetId: InstanceId): Promise<boolean> => {
      const ok = await invoke<boolean>("copy_auth_from_instance", {
        sourceId,
        targetId,
      });
      await refresh();
      return ok;
    },
    [refresh]
  );

  const clearAuth = useCallback(
    async (id: InstanceId): Promise<void> => {
      await invoke("clear_instance_auth", { id });
      await refresh();
    },
    [refresh]
  );

  return { authSources, refresh, copyAuth, clearAuth };
}
