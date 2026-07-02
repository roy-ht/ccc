import { useState, useEffect, useCallback, useRef } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Channel } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { InstanceId, InstanceInfo, SshHost, OutputPayload, StatusChangedPayload } from "../types";
import { reconcileInstances } from "../utils/reconcileInstances";

interface UseInstanceManagerReturn {
  instances: InstanceInfo[];
  /** 復元 / 一括取得用。App.tsx の起動シーケンスから呼ばれる。 */
  setInstances: React.Dispatch<React.SetStateAction<InstanceInfo[]>>;
  createLocalInstance: (command: string, directory?: string, copyAuthFrom?: string, agentProfile?: string, name?: string) => Promise<InstanceId>;
  createRemoteInstance: (hostAlias: string, command: string, directory?: string, copyAuthFrom?: string, agentProfile?: string, name?: string) => Promise<InstanceId>;
  reconnectInstance: (id: InstanceId) => Promise<void>;
  recreateInstance: (id: InstanceId) => Promise<InstanceId>;
  listSshHosts: () => Promise<SshHost[]>;
  writeToInstance: (id: InstanceId, data: Uint8Array) => Promise<void>;
  resizeInstance: (id: InstanceId, rows: number, cols: number) => Promise<void>;
  closeInstance: (id: InstanceId) => Promise<void>;
  subscribeOutput: (id: InstanceId, onData: (data: Uint8Array) => void) => () => void;
  /** Terminal タブ（shell）用 PTY を lazy 起動する。冪等。 */
  ensureShellStarted: (id: InstanceId) => Promise<void>;
  writeToShell: (id: InstanceId, data: Uint8Array) => Promise<void>;
  resizeShell: (id: InstanceId, rows: number, cols: number) => Promise<void>;
  subscribeShellOutput: (id: InstanceId, onData: (data: Uint8Array) => void) => () => void;
}

export function useInstanceManager(): UseInstanceManagerReturn {
  const [instances, setInstances] = useState<InstanceInfo[]>([]);
  const channelsRef = useRef<Map<InstanceId, Channel<OutputPayload>>>(new Map());
  const shellChannelsRef = useRef<Map<InstanceId, Channel<OutputPayload>>>(new Map());

  // --- インスタンス状態更新ヘルパー ---
  const updateInstanceStatus = useCallback(
    (payload: StatusChangedPayload) => {
      setInstances((prev) =>
        prev.map((s) =>
          s.id === payload.id
            ? {
                ...s,
                status: payload.status,
                status_message: payload.status_message ?? null,
                pending_prompt: payload.pending_prompt ?? null,
                // session_id を含まない hook では既存のライブセッションを保持する。
                current_session_id:
                  payload.current_session_id ?? s.current_session_id ?? null,
                // session_title はバックエンドが現在値（None も含む）を毎回送ってくる。
                // None＝抽出未完了なので、既存値を上書きしないと session 切替時に古い
                // タイトルが残り続けてしまう。
                session_title: payload.session_title ?? null,
              }
            : s
        )
      );
    },
    []
  );

  const setStatusOnly = useCallback(
    (id: InstanceId, status: InstanceInfo["status"]) => {
      setInstances((prev) =>
        prev.map((s) => (s.id === id ? { ...s, status } : s))
      );
    },
    []
  );

  const removeInstance = useCallback(
    (id: InstanceId) => {
      setInstances((prev) => prev.filter((s) => s.id !== id));
    },
    []
  );

  // 復元は App.tsx 側で invoke("restore_instances") の戻り値を直接受け取って
  // setInstances する。ここで list_instances を投げると restore 完了前に
  // 空配列で上書きしてしまう race があったため、初期取得は行わない。

  // バックエンドからの切断イベントを監視
  useEffect(() => {
    const unlisten = listen<string>("instance-disconnected", (event) => {
      setStatusOnly(event.payload, "disconnected");
    });
    return () => {
      unlisten.then((fn) => fn());
    };
  }, [setStatusOnly]);

  // バックエンドからのエージェント状態変更イベントを監視
  useEffect(() => {
    const unlisten = listen<StatusChangedPayload>(
      "instance-status-changed",
      (event) => {
        updateInstanceStatus(event.payload);
      }
    );
    return () => {
      unlisten.then((fn) => fn());
    };
  }, [updateInstanceStatus]);

  const createLocalInstance = useCallback(async (command: string, directory?: string, copyAuthFrom?: string, agentProfile?: string, name?: string): Promise<InstanceId> => {
    const dir = directory || null;
    const id = await invoke<InstanceId>("create_local_instance", {
      command,
      directory: dir,
      copyAuthFrom: copyAuthFrom || null,
      agentProfile: agentProfile || null,
      name: name || null,
    });
    const updated = await invoke<InstanceInfo[]>("list_instances");
    // 全置換ではなくマージ: スナップショット取得中に届いた status イベントを
    // 古い値で巻き戻さない（reconcileInstances の docコメント参照）。
    setInstances((prev) => reconcileInstances(prev, updated));
    return id;
  }, []);

  const createRemoteInstance = useCallback(async (hostAlias: string, command: string, directory?: string, copyAuthFrom?: string, agentProfile?: string, name?: string): Promise<InstanceId> => {
    const dir = directory || null;
    const tempId = crypto.randomUUID();
    const dirname = dir ? dir.split("/").pop() || hostAlias : hostAlias;
    const displayName = name && name.trim() !== "" ? name : `${hostAlias}:${dirname}`;
    setInstances((prev) => [
      ...prev,
      {
        id: tempId,
        kind: "remote",
        name: displayName,
        status: "connecting",
        instance_hash: "",
        instance_dir: "",
        agent_profile: agentProfile || "default",
      },
    ]);

    try {
      const id = await invoke<InstanceId>("create_remote_instance", {
        host: hostAlias,
        command,
        directory: dir,
        copyAuthFrom: copyAuthFrom || null,
        agentProfile: agentProfile || null,
        name: name || null,
      });
      const updated = await invoke<InstanceInfo[]>("list_instances");
      // マージ反映。仮エントリ (tempId) はスナップショットに無いので消える。
      setInstances((prev) => reconcileInstances(prev, updated));
      return id;
    } catch (err) {
      removeInstance(tempId);
      throw err;
    }
  }, [removeInstance]);

  const reconnectInstance = useCallback(async (id: InstanceId): Promise<void> => {
    // 即時フィードバック用。以降の遷移（running / disconnected、reconnect 中に
    // 届く hook 由来の状態を含む）はバックエンドが instance-status-changed を
    // emit するのでここでは上書きしない。invoke 成功後に "running" を強制すると
    // 接続中に届いた agent_busy 等を巻き戻すレースがあった。
    setStatusOnly(id, "connecting");
    await invoke("reconnect_instance", { id });
  }, [setStatusOnly]);

  // 同じ設定 (kind / host / dir / command / profile / name) で新規インスタンスを
  // 作り直す。古いインスタンスはバックエンド側で自動 close される。
  // 戻り値は新しい instance id（呼び出し側で active 切り替えに使う）。
  const recreateInstance = useCallback(async (id: InstanceId): Promise<InstanceId> => {
    const newId = await invoke<InstanceId>("recreate_instance", { id });
    const updated = await invoke<InstanceInfo[]>("list_instances");
    setInstances((prev) => reconcileInstances(prev, updated));
    return newId;
  }, []);

  const listSshHosts = useCallback(async (): Promise<SshHost[]> => {
    return invoke<SshHost[]>("list_ssh_hosts");
  }, []);

  const writeToInstance = useCallback(
    async (id: InstanceId, data: Uint8Array): Promise<void> => {
      await invoke("write_to_instance", { id, data: Array.from(data) });
    },
    []
  );

  const resizeInstance = useCallback(
    async (id: InstanceId, rows: number, cols: number): Promise<void> => {
      await invoke("resize_instance", { id, rows, cols });
    },
    []
  );

  const closeInstance = useCallback(async (id: InstanceId): Promise<void> => {
    await invoke("close_instance", { id });
    channelsRef.current.delete(id);
    removeInstance(id);
  }, [removeInstance]);

  // ローカルインスタンスのプロセス終了時に自動クローズ
  useEffect(() => {
    const unlisten = listen<string>("instance-terminated", (event) => {
      closeInstance(event.payload);
    });
    return () => {
      unlisten.then((fn) => fn());
    };
  }, [closeInstance]);

  const subscribeOutput = useCallback(
    (id: InstanceId, onData: (data: Uint8Array) => void): (() => void) => {
      const channel = new Channel<OutputPayload>();
      channel.onmessage = (payload) => {
        onData(new Uint8Array(payload.data));
      };
      channelsRef.current.set(id, channel);
      invoke("subscribe_instance_output", { id, channel }).catch(console.error);
      return () => {
        channelsRef.current.delete(id);
      };
    },
    []
  );

  // ─── Terminal タブ（shell）操作 ────────────────────────────────────────────

  const ensureShellStarted = useCallback(async (id: InstanceId): Promise<void> => {
    await invoke("ensure_shell_started", { id });
  }, []);

  const writeToShell = useCallback(
    async (id: InstanceId, data: Uint8Array): Promise<void> => {
      await invoke("write_to_shell", { id, data: Array.from(data) });
    },
    []
  );

  const resizeShell = useCallback(
    async (id: InstanceId, rows: number, cols: number): Promise<void> => {
      await invoke("resize_shell", { id, rows, cols });
    },
    []
  );

  const subscribeShellOutput = useCallback(
    (id: InstanceId, onData: (data: Uint8Array) => void): (() => void) => {
      const channel = new Channel<OutputPayload>();
      channel.onmessage = (payload) => {
        onData(new Uint8Array(payload.data));
      };
      shellChannelsRef.current.set(id, channel);
      invoke("subscribe_shell_output", { id, channel }).catch(console.error);
      return () => {
        shellChannelsRef.current.delete(id);
      };
    },
    []
  );

  return {
    instances,
    setInstances,
    createLocalInstance,
    createRemoteInstance,
    reconnectInstance,
    recreateInstance,
    listSshHosts,
    writeToInstance,
    resizeInstance,
    closeInstance,
    subscribeOutput,
    ensureShellStarted,
    writeToShell,
    resizeShell,
    subscribeShellOutput,
  };
}
