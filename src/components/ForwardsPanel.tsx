import { useCallback, useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { ForwardSpec, ForwardState, GlobalForwardRow, SshHost } from "../types";

const ORIGIN_LABEL: Record<GlobalForwardRow["origin"], string> = {
  ledger: "ccc",
  config: "ssh config",
  reserved: "ccc 予約",
};

const STATE_LABEL: Record<ForwardState, string> = {
  active: "稼働中",
  blocked: "ポート専有",
  inactive: "停止",
};

const STATE_TITLE: Record<ForwardState, string> = {
  active: "現在の control master に適用されています",
  blocked: "適用されていませんが、listen ポートは別の何かが専有しています",
  inactive: "適用されていません。listen ポートは空いています",
};

/**
 * ポートフォワード管理（ホスト横断）。
 *
 * forward の実体はホスト単位の台帳なので、インスタンスの起動状態とは無関係に
 * 列挙・削除できる。インスタンスのタブに置いていた頃は起動中ホストしか見えず、
 * 停止中ホストが握ったままのポートを掃除できなかった。
 *
 * listen ポート昇順に並べ、同じポートを複数ホストが登録している行には衝突バッジを
 * 出す。これはホスト単位のビューでは原理的に出せなかった情報。
 */
export function ForwardsPanel() {
  const [rows, setRows] = useState<GlobalForwardRow[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  /** ssh config の全ホストまで走査するか（ホストごとに ssh -G が走る） */
  const [includeConfigHosts, setIncludeConfigHosts] = useState(false);

  // 追加フォーム
  const [hosts, setHosts] = useState<SshHost[]>([]);
  const [formHost, setFormHost] = useState("");
  const [listenPort, setListenPort] = useState("");
  const [destHost, setDestHost] = useState("localhost");
  const [destPort, setDestPort] = useState("");
  const [submitting, setSubmitting] = useState(false);
  const [formError, setFormError] = useState<string | null>(null);

  const reload = useCallback(() => {
    setLoading(true);
    setError(null);
    invoke<GlobalForwardRow[]>("forwards_list_all", { includeConfigHosts })
      .then(setRows)
      .catch((e) => setError(String(e)))
      .finally(() => setLoading(false));
  }, [includeConfigHosts]);

  useEffect(() => {
    reload();
  }, [reload]);

  useEffect(() => {
    invoke<SshHost[]>("list_ssh_hosts")
      .then((h) => {
        setHosts(h);
        setFormHost((prev) => prev || h[0]?.alias || "");
      })
      .catch(() => {});
  }, []);

  /** 衝突しているポート番号（ヘッダの警告表示用） */
  const conflictPorts = useMemo(
    () => [...new Set(rows.filter((r) => r.conflict).map((r) => r.spec.listen_port))].sort((a, b) => a - b),
    [rows]
  );

  const handleAdd = async () => {
    const lp = Number(listenPort);
    const dp = Number(destPort || listenPort); // 転送先ポート省略時は listen と同じ
    if (!formHost) {
      setFormError("ホストを選択してください");
      return;
    }
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
    // 追加前に他ホストとの衝突を知らせる（ssh 側のエラーは master に出て手元に届かない
    // ことがあるため、台帳で分かる分は先に伝える）
    const clash = rows.find(
      (r) => r.spec.listen_port === lp && r.host_alias && r.host_alias !== formHost
    );
    if (clash) {
      setFormError(
        `listen ポート ${lp} は既に ${clash.host_alias} に登録されています。` +
          `先にそちらを削除してください。`
      );
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
      await invoke("forwards_add", { hostAlias: formHost, spec });
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

  const handleRemove = async (row: GlobalForwardRow) => {
    setError(null);
    try {
      await invoke("forwards_remove", { hostAlias: row.host_alias, spec: row.spec });
      reload();
    } catch (e) {
      setError(String(e));
    }
  };

  const formatSpec = (row: GlobalForwardRow) => {
    const listen = `${row.spec.listen_host ?? "localhost"}:${row.spec.listen_port}`;
    const dest = `${row.spec.dest_host}:${row.spec.dest_port}`;
    // reverse (-R) はリモート側 listen → ローカルへの転送
    return row.reverse ? `${listen} ← ${dest} (remote)` : `${listen} → ${dest}`;
  };

  return (
    <div className="settings-panel">
      <h3 className="settings-panel-title">ポートフォワード</h3>

      <div className="forwards-header">
        <span className="muted">
          台帳を持つホストと接続中ホストの forward を listen ポート順に表示します
        </span>
        <label className="forwards-scope">
          <input
            type="checkbox"
            checked={includeConfigHosts}
            onChange={(e) => setIncludeConfigHosts(e.target.checked)}
          />
          ssh config の全ホストも走査
        </label>
        <button className="forwards-reload" onClick={reload} disabled={loading}>
          再読込
        </button>
      </div>

      {conflictPorts.length > 0 && (
        <div className="forwards-conflict-banner">
          ポート {conflictPorts.join(", ")} が複数のホストに登録されています。
          実際に listen できるのは 1 つだけなので、不要な方を削除してください。
        </div>
      )}

      {error && <div className="archive-error">{error}</div>}

      <div className="forwards-add-row">
        <select
          className="forwards-input forwards-host-select"
          value={formHost}
          onChange={(e) => setFormHost(e.target.value)}
        >
          {hosts.map((h) => (
            <option key={h.alias} value={h.alias}>
              {h.alias}
            </option>
          ))}
        </select>
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
          disabled={submitting || !listenPort || !formHost}
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
            key={`${row.host_alias}-${row.origin}-${row.spec.listen_port}-${row.reverse}`}
            className={`forwards-item${row.state !== "active" ? " forwards-stale" : ""}${
              row.conflict ? " forwards-conflict" : ""
            }`}
          >
            <div className="forwards-item-main">
              <span className="forwards-port-col">{row.spec.listen_port}</span>
              <span className="forwards-spec">{formatSpec(row)}</span>
              {row.host_alias && (
                <span className="forwards-host-col" title={row.host_alias}>
                  {row.host_alias}
                </span>
              )}
              <span className={`badge badge-fwd-${row.origin}`}>{ORIGIN_LABEL[row.origin]}</span>
              <span
                className={`badge badge-fwd-state-${row.state}`}
                title={STATE_TITLE[row.state]}
              >
                {STATE_LABEL[row.state]}
              </span>
              {row.conflict && (
                <span
                  className="badge badge-fwd-conflict"
                  title="同じ listen ポートを別のホストも登録しています"
                >
                  衝突
                </span>
              )}
            </div>
            {row.error && <div className="forwards-item-error">{row.error}</div>}
            {row.deletable && (
              <button
                className="forwards-remove"
                onClick={() => handleRemove(row)}
                title="この forward を削除（ホストに接続していなくても台帳から消せます）"
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
        接続していないホストの forward も削除できます（台帳から消えるだけで、
        次に接続しても再適用されません）。
      </div>
    </div>
  );
}
