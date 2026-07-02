import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { ForwardRow, ForwardSpec } from "../types";

interface Props {
  /** 対象ホスト。同一ホストを共有するインスタンスは同じ一覧を見る。 */
  hostAlias: string;
}

const ORIGIN_LABEL: Record<ForwardRow["origin"], string> = {
  ledger: "ccc",
  config: "ssh config",
  reserved: "ccc 予約",
};

/**
 * Port forwarding 管理画面。
 * control master が張っている forward を一覧し、`-L` 相当の追加と
 * ccc 経由で追加した forward の削除を行う。一覧の取得自体が
 * master 世代チェック＋台帳リプレイを兼ねる（forwards_list 内で実施）。
 */
export function ForwardsPanel({ hostAlias }: Props) {
  const [rows, setRows] = useState<ForwardRow[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // 追加フォーム
  const [listenPort, setListenPort] = useState("");
  const [destHost, setDestHost] = useState("localhost");
  const [destPort, setDestPort] = useState("");
  const [submitting, setSubmitting] = useState(false);
  const [formError, setFormError] = useState<string | null>(null);

  const reload = useCallback(() => {
    setLoading(true);
    setError(null);
    invoke<ForwardRow[]>("forwards_list", { hostAlias })
      .then(setRows)
      .catch((e) => setError(String(e)))
      .finally(() => setLoading(false));
  }, [hostAlias]);

  useEffect(() => {
    reload();
  }, [reload]);

  const handleAdd = async () => {
    const lp = Number(listenPort);
    const dp = Number(destPort || listenPort); // 転送先ポート省略時は listen と同じ
    if (!Number.isInteger(lp) || lp < 1 || lp > 65535) {
      setFormError("listen ポートが不正です");
      return;
    }
    if (!Number.isInteger(dp) || dp < 1 || dp > 65535) {
      setFormError("転送先ポートが不正です");
      return;
    }
    if (!destHost.trim()) {
      setFormError("転送先ホストを入力してください");
      return;
    }
    const spec: ForwardSpec = {
      listen_port: lp,
      dest_host: destHost.trim(),
      dest_port: dp,
    };
    setSubmitting(true);
    setFormError(null);
    try {
      await invoke("forwards_add", { hostAlias, spec });
      setListenPort("");
      setDestPort("");
      reload();
    } catch (e) {
      // ssh の stderr がそのまま入る（例: ポートが他プロセスに使用されている）
      setFormError(String(e));
    } finally {
      setSubmitting(false);
    }
  };

  const handleRemove = async (row: ForwardRow) => {
    setError(null);
    try {
      await invoke("forwards_remove", { hostAlias, spec: row.spec });
      reload();
    } catch (e) {
      setError(String(e));
    }
  };

  const formatSpec = (row: ForwardRow) => {
    const listen = `${row.spec.listen_host ?? "localhost"}:${row.spec.listen_port}`;
    const dest = `${row.spec.dest_host}:${row.spec.dest_port}`;
    // reverse (-R) はリモート側 listen → ローカルへの転送
    return row.reverse ? `${listen} ← ${dest} (remote)` : `${listen} → ${dest}`;
  };

  return (
    <div className="archive-panel">
      <div className="forwards-header">
        <span className="forwards-host">
          {hostAlias} の forward 一覧
        </span>
        <button className="forwards-reload" onClick={reload} disabled={loading}>
          再読込
        </button>
      </div>

      {error && <div className="archive-error">{error}</div>}

      <div className="forwards-add-row">
        <input
          className="forwards-input forwards-port"
          type="text"
          inputMode="numeric"
          placeholder="local port"
          value={listenPort}
          onChange={(e) => setListenPort(e.target.value)}
        />
        <span className="muted">→</span>
        <input
          className="forwards-input forwards-port"
          type="text"
          inputMode="numeric"
          placeholder="remote port"
          value={destPort}
          onChange={(e) => setDestPort(e.target.value)}
        />
        <span className="muted">@</span>
        <input
          className="forwards-input forwards-host-input"
          type="text"
          placeholder="host"
          value={destHost}
          onChange={(e) => setDestHost(e.target.value)}
        />
        <button
          className="forwards-add-button"
          onClick={handleAdd}
          disabled={submitting || !listenPort}
        >
          {submitting ? "追加中…" : "追加"}
        </button>
      </div>
      {formError && <div className="archive-error">{formError}</div>}

      <div className="archive-list">
        {loading && rows.length === 0 && <div className="archive-empty">読み込み中…</div>}
        {!loading && rows.length === 0 && (
          <div className="archive-empty">forward がありません</div>
        )}
        {rows.map((row) => (
          <div
            key={`${row.origin}-${row.spec.listen_host ?? ""}-${row.spec.listen_port}-${row.reverse}`}
            className={`forwards-item ${row.stale ? "forwards-stale" : ""}`}
          >
            <div className="forwards-item-main">
              <span className="forwards-spec">{formatSpec(row)}</span>
              <span className={`badge badge-fwd-${row.origin}`}>
                {ORIGIN_LABEL[row.origin]}
              </span>
              {row.stale && (
                <span className="badge badge-fwd-stale" title="現在の master に適用されていません">
                  失効
                </span>
              )}
            </div>
            {row.error && <div className="forwards-item-error">{row.error}</div>}
            {row.deletable && (
              <button
                className="forwards-remove"
                onClick={() => handleRemove(row)}
                title="この forward を削除"
              >
                削除
              </button>
            )}
          </div>
        ))}
      </div>

      <div className="forwards-note muted">
        ssh config 定義分と ccc 予約（hook 用）は削除できません。ユーザー管理の
        ControlMaster を使用している場合、ccc 追加分は ccc 終了後も master に残ります。
      </div>
    </div>
  );
}
