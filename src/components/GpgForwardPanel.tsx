import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { GpgForwardRow, SshHost } from "../types";

const HEALTH_LABEL: Record<string, string> = {
  healthy: "疎通中",
  broken: "不調",
  unreachable: "未接続",
  no_forward: "無効",
};

const PHASE_LABEL: Record<string, string> = {
  connecting: "接続中",
  connected: "疎通しています",
  retrying: "再接続待ち",
  auth_failed: "認証に失敗（BatchMode では自動復旧しません）",
  stopped: "停止",
};

/** 状態に応じたドットの色分け（healthy 以外だけ目立たせる）。 */
function healthKind(row: GpgForwardRow): "ok" | "warn" | "bad" | "off" {
  if (!row.enabled) return "off";
  if (!row.running) return "warn";
  switch (row.health) {
    case "healthy":
      return "ok";
    case "broken":
      return "bad";
    default:
      return "warn";
  }
}

function ago(epoch: number): string {
  const diff = Math.max(0, Math.floor(Date.now() / 1000) - epoch);
  if (diff < 60) return `${diff} 秒前`;
  if (diff < 3600) return `${Math.floor(diff / 60)} 分前`;
  if (diff < 86400) return `${Math.floor(diff / 3600)} 時間前`;
  return `${Math.floor(diff / 86400)} 日前`;
}

/**
 * gpg agent forward（relay 方式, v0.14）の管理。
 *
 * 実体はホスト単位の常駐 uplink プロセスなので、ここでの操作は
 * `ccc-ssh gpg` と完全に同じバックエンドを叩く（GUI と CLI で挙動が割れない）。
 * 復旧は uplink 自身が行うため、この画面は「今どうなっているか」の提示と
 * 有効化・起動停止に絞る。socket パスなどの詳細編集は ~/.ccc/gpg.json を直接編集する。
 */
export function GpgForwardPanel() {
  const [rows, setRows] = useState<GpgForwardRow[]>([]);
  const [hosts, setHosts] = useState<SshHost[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [busyHost, setBusyHost] = useState<string | null>(null);
  const [addHost, setAddHost] = useState("");
  /** ホストごとの移行漏れ診断結果（ssh -G を走らせるので明示操作でのみ実行） */
  const [staleConfig, setStaleConfig] = useState<Record<string, string[]>>({});

  const reload = useCallback(async () => {
    setLoading(true);
    try {
      setRows(await invoke<GpgForwardRow[]>("gpg_forward_list"));
      setError(null);
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    reload();
    invoke<SshHost[]>("list_ssh_hosts")
      .then(setHosts)
      .catch(() => {});
  }, [reload]);

  // uplink の状態は常駐プロセス側で変わるので、開いている間だけ緩く追従する
  useEffect(() => {
    const timer = setInterval(reload, 5000);
    return () => clearInterval(timer);
  }, [reload]);

  /** 1 ホストに対する操作。戻り値の行で該当行だけ差し替える。 */
  const run = useCallback(
    async (host: string, command: string, args: Record<string, unknown> = {}) => {
      setBusyHost(host);
      setError(null);
      try {
        const updated = await invoke<GpgForwardRow>(command, {
          hostAlias: host,
          ...args,
        });
        setRows((prev) =>
          prev.map((r) => (r.host_alias === host ? updated : r))
        );
      } catch (e) {
        setError(String(e));
        reload();
      } finally {
        setBusyHost(null);
      }
    },
    [reload]
  );

  const handleAdd = useCallback(async () => {
    const host = addHost.trim();
    if (!host) return;
    setBusyHost(host);
    setError(null);
    try {
      await invoke("gpg_forward_set_enabled", { hostAlias: host, enabled: true });
      setAddHost("");
      await reload();
    } catch (e) {
      setError(String(e));
    } finally {
      setBusyHost(null);
    }
  }, [addHost, reload]);

  const handleRemove = useCallback(
    async (host: string) => {
      setBusyHost(host);
      setError(null);
      try {
        await invoke("gpg_forward_remove", { hostAlias: host });
        await reload();
      } catch (e) {
        setError(String(e));
      } finally {
        setBusyHost(null);
      }
    },
    [reload]
  );

  const handleDiagnose = useCallback(async (host: string) => {
    setBusyHost(host);
    try {
      const found = await invoke<string[]>("gpg_forward_stale_config", {
        hostAlias: host,
      });
      setStaleConfig((prev) => ({ ...prev, [host]: found }));
    } catch (e) {
      setError(String(e));
    } finally {
      setBusyHost(null);
    }
  }, []);

  // 既に登録済みのホストは追加候補から外す
  const known = new Set(rows.map((r) => r.host_alias));
  const candidates = hosts.filter((h) => !known.has(h.alias));

  return (
    <div className="settings-panel">
      <h3 className="settings-panel-title">gpg agent forward</h3>

      <div className="forwards-header">
        <span className="forwards-note muted" style={{ padding: 0, border: "none" }}>
          リモートの常駐 relay が gpg socket を所有します。復旧は自動です。
        </span>
        <button className="forwards-reload" onClick={reload} disabled={loading}>
          {loading ? "更新中…" : "更新"}
        </button>
      </div>

      {error && <div className="archive-error">{error}</div>}

      {rows.length === 0 ? (
        <div className="archive-empty">
          有効なホストがありません。下の欄からホストを追加してください。
        </div>
      ) : (
        <div className="archive-list">
          {rows.map((row) => {
            const kind = healthKind(row);
            const busy = busyHost === row.host_alias;
            const stale = staleConfig[row.host_alias];
            return (
              <div key={row.host_alias} className="gpg-row">
                <div className="forwards-item-main">
                  <span className={`gpg-dot gpg-dot--${kind}`} aria-hidden="true" />
                  <span className="forwards-host">{row.host_alias}</span>
                  <span className="badge">
                    {row.enabled
                      ? HEALTH_LABEL[row.health] ?? row.health
                      : "無効"}
                  </span>
                  {row.last_ok_epoch != null && (
                    <span className="forwards-note muted" style={{ padding: 0, border: "none" }}>
                      最終疎通 {ago(row.last_ok_epoch)}
                    </span>
                  )}
                  <span className="gpg-actions">
                    {row.enabled && !row.running && (
                      <button
                        className="forwards-add-button"
                        onClick={() => run(row.host_alias, "gpg_forward_up")}
                        disabled={busy}
                      >
                        起動
                      </button>
                    )}
                    {row.enabled && row.running && (
                      <>
                        <button
                          className="forwards-remove"
                          onClick={() => run(row.host_alias, "gpg_forward_restart")}
                          disabled={busy}
                          title="停止してから起動し直します"
                        >
                          張り直す
                        </button>
                        <button
                          className="forwards-remove"
                          onClick={() => run(row.host_alias, "gpg_forward_down")}
                          disabled={busy}
                        >
                          停止
                        </button>
                      </>
                    )}
                    <button
                      className="forwards-remove"
                      onClick={() =>
                        run(row.host_alias, "gpg_forward_set_enabled", {
                          enabled: !row.enabled,
                        })
                      }
                      disabled={busy}
                    >
                      {row.enabled ? "無効化" : "有効化"}
                    </button>
                    <button
                      className="forwards-remove"
                      onClick={() => handleDiagnose(row.host_alias)}
                      disabled={busy}
                      title="ssh config に古い RemoteForward が残っていないか調べます"
                    >
                      診断
                    </button>
                    <button
                      className="forwards-remove"
                      onClick={() => handleRemove(row.host_alias)}
                      disabled={busy}
                      title="設定ごと削除します"
                    >
                      削除
                    </button>
                  </span>
                </div>

                <div className="gpg-detail muted">
                  {row.enabled ? (
                    row.running ? (
                      <>
                        {PHASE_LABEL[row.phase ?? ""] ?? "起動直後"}
                        {row.generation > 0 &&
                          ` / 世代 ${row.generation} / 再接続 ${row.reconnects} 回`}
                      </>
                    ) : (
                      "uplink 停止中"
                    )
                  ) : (
                    "このホストでは relay を使いません"
                  )}
                  {row.local_socket && (
                    <>
                      <br />
                      local: {row.local_socket}
                      {row.local_agent && ` [${row.local_agent}]`}
                    </>
                  )}
                  {row.remote_socket && (
                    <>
                      <br />
                      remote: {row.remote_socket}
                    </>
                  )}
                </div>

                {row.last_error && (
                  <div className="forwards-item-error">{row.last_error}</div>
                )}

                {stale != null &&
                  (stale.length === 0 ? (
                    <div className="gpg-detail muted">
                      ssh config に unix socket の RemoteForward は残っていません。
                    </div>
                  ) : (
                    <div className="forwards-item-error">
                      ssh config に RemoteForward が残っています（{stale.length} 件）。
                      素の ssh で接続した瞬間にリモートの socket が上書きされます。
                      ~/.ssh/config から削除し、`ccc-ssh down` で既存 master を畳んでください。
                      {stale.map((line) => (
                        <div key={line} className="forwards-spec">
                          {line}
                        </div>
                      ))}
                    </div>
                  ))}
              </div>
            );
          })}
        </div>
      )}

      <div className="forwards-add-row">
        <input
          className="forwards-input forwards-host-input"
          list="gpg-host-candidates"
          placeholder="ホスト（ssh の接続先名）"
          value={addHost}
          onChange={(e) => setAddHost(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") handleAdd();
          }}
        />
        <datalist id="gpg-host-candidates">
          {candidates.map((h) => (
            <option key={h.alias} value={h.alias} />
          ))}
        </datalist>
        <button
          className="forwards-add-button"
          onClick={handleAdd}
          disabled={!addHost.trim() || busyHost != null}
        >
          有効にする
        </button>
      </div>

      <div className="forwards-note muted">
        設定の実体は <code>~/.ccc/gpg.json</code>、CLI からは{" "}
        <code>ccc-ssh gpg</code> で同じ操作ができます。
        <br />
        移行時は <strong>ssh config の gpg 用 RemoteForward を削除</strong>してください
        （「診断」で検出できます）。残っていると素の ssh 接続時にリモートの socket
        が上書きされます。
      </div>
    </div>
  );
}
