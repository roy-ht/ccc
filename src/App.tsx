import { useState, useCallback, useRef, useEffect, useMemo } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Sidebar } from "./components/Sidebar";
import { TerminalPanel } from "./components/TerminalPanel";
import { MainTabs } from "./components/MainTabs";
import { SessionsPanel } from "./components/SessionsPanel";
import { MemoriesPanel } from "./components/MemoriesPanel";
import { ExplorerPanel } from "./components/explorer/ExplorerPanel";
import { NewInstanceDialog } from "./components/NewInstanceDialog";
import { SettingsPage } from "./components/SettingsPage";
import { ConfirmDialog } from "./components/ConfirmDialog";
import { useInstanceManager } from "./hooks/useInstanceManager";
import { useKeyboardShortcuts } from "./hooks/useKeyboardShortcuts";
import { useSettings } from "./hooks/useSettings";
import { useDirectoryHistory } from "./hooks/useDirectoryHistory";
import { useAgentCommands } from "./hooks/useAgentCommands";
import { useAuthSwitch } from "./hooks/useAuthSwitch";
import { useSidebarWidth } from "./hooks/useSidebarWidth";
import { useFileDrop } from "./hooks/useFileDrop";
import { InstanceId, InstanceKind, SshHost, InstanceInfo, MainTab } from "./types";
import "./App.css";

function App() {
  const {
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
  } = useInstanceManager();

  const [activeId, setActiveId] = useState<InstanceId | null>(null);
  // 主画面のタブ。インスタンス切替時は常に Terminal にリセットする（後述の useEffect）。
  const [activeTab, setActiveTab] = useState<MainTab>("terminal");
  const [showNewDialog, setShowNewDialog] = useState(false);
  const [showSettings, setShowSettings] = useState(false);
  const [sshHosts, setSshHosts] = useState<SshHost[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [closingId, setClosingId] = useState<InstanceId | null>(null);
  const [instanceOrder, setInstanceOrder] = useState<InstanceId[]>([]);
  const [reconnectEpochs, setReconnectEpochs] = useState<Record<InstanceId, number>>({});
  const writersRef = useRef<Map<InstanceId, (data: Uint8Array) => void>>(new Map());
  const shellWritersRef = useRef<Map<InstanceId, (data: Uint8Array) => void>>(new Map());
  // shell PTY を「フロント側で既に起動要求済み」の id 集合。activeTab が "shell" になる
  // たびに毎回 invoke するのを避けつつ、reconnectEpoch が変わったら再起動できるよう
  // 値は最後に観測した reconnectEpoch を保持する。
  const shellStartedRef = useRef<Map<InstanceId, number>>(new Map());
  // ensure 成功後に張った subscribe の cleanup。インスタンスごとに1本ぶら下げ、
  // 再 ensure 時に張り直すために旧 cleanup を発火する。
  const shellSubscriptionsRef = useRef<Map<InstanceId, () => void>>(new Map());
  const { settings, saveSettings } = useSettings();
  const { addToHistory } = useDirectoryHistory();
  const { commands: agentCommands, addCommand, removeCommand } = useAgentCommands();
  const { authSources, refresh: refreshAuthSources } = useAuthSwitch();
  const { width: sidebarWidth, startDrag: startSidebarResize, resetWidth: resetSidebarWidth } = useSidebarWidth();

  // レイアウト確定後にウィンドウを表示し、その後にインスタンスを復元する。
  // ウィンドウ描画前に復元するとコンテナサイズが 0 で fit() が効かないため。
  // restore_instances は復元完了後の一覧を戻り値で返すので、それを直接 setInstances する。
  // event 駆動だと listen() 登録が遅れた場合に取り逃して一覧が空のままになる race があった。
  // 復元は 1 プロセス 1 回きり。StrictMode の二重呼び出しや依存の同一性崩れで
  // 再実行されると、生きているインスタンスに PTY をもう一本張ってしまい、
  // 旧世代の破棄を通じて「入力は通るが画面が更新されない」状態を招く。
  // バックエンド側にも同じガードがあるが、無駄な往復を避けるため手前でも止める。
  const restoreStartedRef = useRef(false);
  useEffect(() => {
    if (restoreStartedRef.current) return;
    restoreStartedRef.current = true;
    invoke("show_main_window")
      .then(() => invoke<InstanceInfo[]>("restore_instances"))
      .then((restored) => {
        // リモートインスタンスは reattach 直後の PTY が default 80x24 で
        // 立ち上がる。手動 reconnect と同様、reconnectEpoch を加算して
        // TerminalPanel に「現サイズを再 push」させる。
        setReconnectEpochs((prev) => {
          const next = { ...prev };
          for (const inst of restored) {
            if (inst.kind === "remote") {
              next[inst.id] = (next[inst.id] ?? 0) + 1;
            }
          }
          return next;
        });
        setInstances(restored);
      })
      .catch(console.error);
  }, [setInstances]);

  // activeId が未設定または削除済みインスタンスを指す場合、先頭のインスタンスを選択する
  useEffect(() => {
    if (instances.length === 0) return;
    if (activeId === null || !instances.some((s) => s.id === activeId)) {
      setActiveId(instances[0].id);
    }
  }, [instances, activeId]);

  // インスタンスを切り替えたら常に Terminal タブに戻す。
  // Sessions / Memories より Terminal を見る機会が圧倒的に多いため。
  useEffect(() => {
    setActiveTab("terminal");
  }, [activeId]);

  // バックエンドの instances 一覧と instanceOrder を同期する。
  // - 新たに現れた id は順序末尾に追加
  // - 消えた id は順序から除外
  // 並び替えは UI セッション内のみ保持（リロードで初期順序）
  useEffect(() => {
    setInstanceOrder((prev) => {
      const known = new Set(prev);
      const currentIds = new Set(instances.map((s) => s.id));
      const kept = prev.filter((id) => currentIds.has(id));
      const appended = instances
        .map((s) => s.id)
        .filter((id) => !known.has(id));
      return [...kept, ...appended];
    });
  }, [instances]);

  // 並び順に従ってソートしたインスタンス（Sidebar 用）
  const orderedInstances = useMemo<InstanceInfo[]>(() => {
    const byId = new Map(instances.map((s) => [s.id, s]));
    const ordered: InstanceInfo[] = [];
    for (const id of instanceOrder) {
      const inst = byId.get(id);
      if (inst) ordered.push(inst);
    }
    // instanceOrder にまだ反映されていない id（直近追加分）も末尾に
    for (const inst of instances) {
      if (!instanceOrder.includes(inst.id)) ordered.push(inst);
    }
    return ordered;
  }, [instances, instanceOrder]);

  const handleReorder = useCallback((fromId: InstanceId, toId: InstanceId) => {
    setInstanceOrder((prev) => {
      const order = prev.length > 0 ? prev : instances.map((s) => s.id);
      const fromIdx = order.indexOf(fromId);
      const toIdx = order.indexOf(toId);
      if (fromIdx === -1 || toIdx === -1) return prev;
      const next = [...order];
      const [moved] = next.splice(fromIdx, 1);
      next.splice(toIdx, 0, moved);
      return next;
    });
  }, [instances]);

  // アクティブインスタンスの隣を選んで閉じる
  const handleClose = useCallback(
    async (id: InstanceId) => {
      if (activeId === id) {
        const idx = instances.findIndex((s) => s.id === id);
        const next = instances[idx + 1] ?? instances[idx - 1] ?? null;
        setActiveId(next?.id ?? null);
      }
      writersRef.current.delete(id);
      shellSubscriptionsRef.current.get(id)?.();
      shellSubscriptionsRef.current.delete(id);
      shellStartedRef.current.delete(id);
      shellWritersRef.current.delete(id);
      await closeInstance(id);
    },
    [activeId, instances, closeInstance]
  );

  // 新規インスタンスダイアログを開く
  const handleNew = useCallback(async () => {
    try {
      const hosts = await listSshHosts();
      setSshHosts(hosts);
    } catch {
      setSshHosts([]);
    }
    refreshAuthSources();
    setShowNewDialog(true);
  }, [listSshHosts, refreshAuthSources]);

  // ダイアログからの起動
  const handleLaunch = useCallback(
    async (config: { target: "local" | string; command: string; directory: string; name?: string; copyAuthFrom?: string; agentProfile?: string }) => {
      setShowNewDialog(false);
      const { target, command, directory, name, copyAuthFrom, agentProfile } = config;
      try {
        let id: InstanceId;
        if (target === "local") {
          id = await createLocalInstance(command, directory || undefined, copyAuthFrom, agentProfile, name);
          if (directory) addToHistory("local", directory);
        } else {
          id = await createRemoteInstance(target, command, directory || undefined, copyAuthFrom, agentProfile, name);
          if (directory) addToHistory(target, directory);
        }
        setActiveId(id);
        refreshAuthSources();
      } catch (err) {
        const label = target === "local" ? "ローカル" : target;
        setError(`インスタンス作成に失敗 (${label}): ${err}`);
      }
    },
    [createLocalInstance, createRemoteInstance, addToHistory, refreshAuthSources]
  );

  /// permission ボタン応答：選択肢のキー文字をターミナルに送信する。
  const handlePromptResponse = useCallback(
    (id: InstanceId, key: string) => {
      const encoder = new TextEncoder();
      // claude code は数字キー単独で選択を確定する
      writeToInstance(id, encoder.encode(key)).catch(() => {});
    },
    [writeToInstance]
  );

  /// マウスクリック由来の閉じる操作は確認ダイアログを挟む
  const handleCloseRequest = useCallback((id: InstanceId) => {
    setClosingId(id);
  }, []);

  const handleReconnect = useCallback(
    async (id: InstanceId) => {
      try {
        await reconnectInstance(id);
        setActiveId(id);
        // 新規 PTY は default 80x24 で生成されるため、TerminalPanel に再同期を促す
        setReconnectEpochs((prev) => ({ ...prev, [id]: (prev[id] ?? 0) + 1 }));
      } catch (err) {
        setError(`再接続に失敗: ${err}`);
      }
    },
    [reconnectInstance]
  );

  const handleRecreate = useCallback(
    async (id: InstanceId) => {
      try {
        const newId = await recreateInstance(id);
        setActiveId(newId);
      } catch (err) {
        setError(`再作成に失敗: ${err}`);
      }
    },
    [recreateInstance]
  );

  const handleTerminalReady = useCallback(
    (instanceId: InstanceId, writeOutput: (data: Uint8Array) => void) => {
      writersRef.current.set(instanceId, writeOutput);
      const cleanup = subscribeOutput(instanceId, (data) => {
        writersRef.current.get(instanceId)?.(data);
      });
      return () => {
        writersRef.current.delete(instanceId);
        cleanup();
      };
    },
    [subscribeOutput]
  );

  // 切断中の入力は PTY が無いので送っても届かないが、**黙って捨てない**。
  // 無言の握り潰しは「キーが全く効かない」という原因不明の症状に化けるため、
  // 必ず理由を提示して次の行動（再接続 / 再作成）へ誘導する。
  const handleData = useCallback(
    (instanceId: InstanceId, data: Uint8Array) => {
      const instance = instances.find((s) => s.id === instanceId);
      if (instance && (instance.status === "disconnected" || instance.status === "terminated")) {
        setError(
          instance.status === "disconnected"
            ? "接続が切れているため入力を送れません。サイドバーの「再接続」を実行してください。"
            : "このインスタンスは終了しています。サイドバーの「再作成」を実行してください。"
        );
        return;
      }
      writeToInstance(instanceId, data).catch(() => {});
    },
    [writeToInstance, instances]
  );

  const handleResize = useCallback(
    (instanceId: InstanceId, rows: number, cols: number) => {
      resizeInstance(instanceId, rows, cols).catch(() => {});
    },
    [resizeInstance]
  );

  // ─── Shell タブ用ハンドラ ────────────────────────────────────────────────

  // TerminalPanel(shell) 起動時に呼ばれる。writer を ref に登録するだけで、
  // subscribe は effect 側（ensure_shell_started 成功後）に集約する。
  // ここで subscribe してしまうと、PTY 未起動のうちに Rust 側で
  // "shell PTY not started" を返してしまい、後から ensure が走っても
  // subscribe は復活しないというレースを避けるための分離。
  const handleShellReady = useCallback(
    (instanceId: InstanceId, writeOutput: (data: Uint8Array) => void) => {
      shellWritersRef.current.set(instanceId, writeOutput);
      return () => {
        shellWritersRef.current.delete(instanceId);
      };
    },
    []
  );

  const handleShellData = useCallback(
    (instanceId: InstanceId, data: Uint8Array) => {
      writeToShell(instanceId, data).catch(() => {});
    },
    [writeToShell]
  );

  const handleShellResize = useCallback(
    (instanceId: InstanceId, rows: number, cols: number) => {
      resizeShell(instanceId, rows, cols).catch(() => {});
    },
    [resizeShell]
  );

  // Shell タブ初回表示時に PTY を lazy 起動 → subscribe を張る。
  // 同じ (activeId, reconnectEpoch) では一度だけ走り、タブ切替で複数回 invoke しない。
  useEffect(() => {
    if (activeTab !== "shell" || !activeId) return;
    const id = activeId;
    const epoch = reconnectEpochs[id] ?? 0;
    if (shellStartedRef.current.get(id) === epoch) return;
    shellStartedRef.current.set(id, epoch);

    let cancelled = false;
    (async () => {
      try {
        await ensureShellStarted(id);
        if (cancelled) return;
        // 旧 subscribe があれば外してから張り直す（reconnectEpoch 更新時に効く）。
        shellSubscriptionsRef.current.get(id)?.();
        const cleanup = subscribeShellOutput(id, (data) => {
          shellWritersRef.current.get(id)?.(data);
        });
        shellSubscriptionsRef.current.set(id, cleanup);
      } catch (err) {
        shellStartedRef.current.delete(id);
        setError(`Terminal タブの起動に失敗: ${err}`);
      }
    })();

    return () => {
      cancelled = true;
    };
  }, [activeTab, activeId, reconnectEpochs, ensureShellStarted, subscribeShellOutput]);

  useKeyboardShortcuts({
    instances,
    activeId,
    onNew: handleNew,
    onSelectById: setActiveId,
  });

  // ドラッグ&ドロップでローカルインスタンスにファイルパスを差し込む。
  // リモートはローカルパスが参照できないので useFileDrop 側で無視する。
  const activeInstance = instances.find((s) => s.id === activeId) ?? null;
  const activeKind: InstanceKind | null = activeInstance?.kind ?? null;
  useFileDrop({ activeId, activeKind, activeTab, writeToInstance });

  const appStyle = {
    "--status-message-lines": settings.display.status_message_lines ?? 2,
  } as React.CSSProperties;

  return (
    <div className="app" style={appStyle}>
      {showSettings && (
        <SettingsPage
          settings={settings}
          onSave={saveSettings}
          onClose={() => setShowSettings(false)}
        />
      )}
      {showNewDialog && (
        <NewInstanceDialog
          sshHosts={sshHosts}
          presets={settings.connections}
          agentCommands={agentCommands}
          authSources={authSources}
          onAddCommand={addCommand}
          onRemoveCommand={removeCommand}
          onLaunch={handleLaunch}
          onOpenSettings={() => setShowSettings(true)}
          onClose={() => setShowNewDialog(false)}
        />
      )}
      <Sidebar
        instances={orderedInstances}
        activeId={activeId}
        onSelect={setActiveId}
        onClose={handleCloseRequest}
        onReconnect={handleReconnect}
        onRecreate={handleRecreate}
        onNew={handleNew}
        onOpenSettings={() => setShowSettings(true)}
        onReorder={handleReorder}
        onPromptResponse={handlePromptResponse}
        width={sidebarWidth}
        onStartResize={startSidebarResize}
        onResetWidth={resetSidebarWidth}
      />
      <main className="main-panel">
        {activeInstance && (
          <MainTabs active={activeTab} onChange={setActiveTab} />
        )}
        <div className="main-body">
          {instances.length === 0 && (
            <div className="empty-state">
              <p>「New +」でインスタンスを作成してください</p>
              <p className="empty-hint">⌘T でインスタンス作成ダイアログを開く</p>
            </div>
          )}
          {/* Agent タブ: 常にマウントしたまま、active instance のみ表示する
              （xterm の状態を保持しつつタブ切替で隠す）。 */}
          {instances.map((s) => (
            <TerminalPanel
              key={s.id}
              instanceId={s.id}
              isVisible={s.id === activeId && activeTab === "terminal"}
              fontFamily={settings.display.font_family}
              fontSize={settings.display.font_size}
              colorTheme={settings.display.color_theme}
              reconnectEpoch={reconnectEpochs[s.id] ?? 0}
              useWebgl={settings.display.use_webgl ?? true}
              onData={handleData}
              onResize={handleResize}
              onReady={handleTerminalReady}
            />
          ))}
          {/* Terminal タブ: agent と同じ tmux session の session-group メンバーに
              attach する補助 PTY。Agent と同じく常時マウントして xterm 状態を保持する。
              ただし WebGL レンダラを使うと WKWebView 上で Agent 用 canvas と
              干渉する既存バグがあるため、Shell 用は useWebgl=false で DOM レンダラに倒す。
              Shell の出力速度はそれほど高くないので DOM でも実用上問題ない。 */}
          {instances.map((s) => (
            <TerminalPanel
              key={`shell-${s.id}`}
              instanceId={s.id}
              isVisible={s.id === activeId && activeTab === "shell"}
              fontFamily={settings.display.font_family}
              fontSize={settings.display.font_size}
              colorTheme={settings.display.color_theme}
              reconnectEpoch={reconnectEpochs[s.id] ?? 0}
              useWebgl={false}
              onData={handleShellData}
              onResize={handleShellResize}
              onReady={handleShellReady}
            />
          ))}
          {activeInstance && activeTab === "sessions" && (
            <SessionsPanel key={activeInstance.id} instance={activeInstance} />
          )}
          {activeInstance && activeTab === "memories" && (
            <MemoriesPanel key={activeInstance.id} instance={activeInstance} />
          )}
          {activeInstance && activeTab === "explorer" && (
            <ExplorerPanel key={activeInstance.id} instance={activeInstance} />
          )}
        </div>
      </main>
      {closingId && (
        <ConfirmDialog
          title="インスタンスを閉じる"
          message={(() => {
            const target = instances.find((s) => s.id === closingId);
            return target
              ? `「${target.name}」を閉じます。tmux セッションも終了し、復元できなくなります。続行しますか?`
              : "このインスタンスを閉じます。続行しますか?";
          })()}
          confirmLabel="閉じる"
          destructive
          onConfirm={() => {
            const id = closingId;
            setClosingId(null);
            handleClose(id);
          }}
          onCancel={() => setClosingId(null)}
        />
      )}
      {error && (
        <div className="error-toast">
          <span>{error}</span>
          <button className="error-toast-close" onClick={() => setError(null)}>✕</button>
        </div>
      )}
    </div>
  );
}

export default App;
