import { useState, useCallback, useMemo, useEffect } from "react";
import { SshHost, ConnectionPreset, AuthInfo } from "../types";
import { useDirectoryHistory } from "../hooks/useDirectoryHistory";
import { useClaudeProfiles } from "../hooks/useClaudeProfiles";

interface LaunchConfig {
  target: "local" | string;
  command: string;
  directory: string;
  name?: string;
  copyAuthFrom?: string;
  agentProfile?: string;
}

/// バックエンド `derive_instance_name` 相当のフロント実装。
/// 接続先と作業ディレクトリから既定の接続名を生成する。
function deriveDefaultName(target: "local" | string, directory: string): string {
  const trimmed = directory.trim();
  let dirname = "~";
  if (trimmed && trimmed !== "/") {
    const parts = trimmed.split("/").filter((p) => p !== "");
    dirname = parts[parts.length - 1] || "~";
  }
  return target === "local" ? dirname : `${target}:${dirname}`;
}

interface Props {
  sshHosts: SshHost[];
  presets: ConnectionPreset[];
  agentCommands: string[];
  authSources: AuthInfo[];
  onAddCommand: (cmd: string) => void;
  onRemoveCommand: (cmd: string) => void;
  onLaunch: (config: LaunchConfig) => void;
  onOpenSettings: () => void;
  onClose: () => void;
}

export function NewInstanceDialog({
  sshHosts,
  presets,
  agentCommands,
  authSources,
  onAddCommand,
  onRemoveCommand,
  onLaunch,
  onOpenSettings,
  onClose,
}: Props) {
  const [mode, setMode] = useState<"preset" | "manual">(presets.length > 0 ? "preset" : "manual");
  const [copyAuthFrom, setCopyAuthFrom] = useState<string>("");

  const credentialedAuthSources = useMemo(
    () => authSources.filter((a) => a.has_credentials),
    [authSources]
  );

  const wrappedLaunch = useCallback(
    (config: LaunchConfig) => {
      onLaunch({ ...config, copyAuthFrom: copyAuthFrom || undefined });
    },
    [onLaunch, copyAuthFrom]
  );

  const handleKeyDown = useCallback(
    (e: React.KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    },
    [onClose]
  );

  return (
    <div className="dialog-overlay" onClick={onClose}>
      <div
        className="dialog new-instance-dialog"
        onClick={(e) => e.stopPropagation()}
        onKeyDown={handleKeyDown}
      >
        <div className="dialog-header">
          <span className="dialog-title">新規インスタンス</span>
          <button className="dialog-close" onClick={onClose}>✕</button>
        </div>

        <div className="new-instance-mode-toggle">
          <button
            className={`connection-toggle-btn${mode === "preset" ? " active" : ""}`}
            onClick={() => setMode("preset")}
          >
            接続先から選択
          </button>
          <button
            className={`connection-toggle-btn${mode === "manual" ? " active" : ""}`}
            onClick={() => setMode("manual")}
          >
            手動接続
          </button>
        </div>

        {mode === "preset" ? (
          <PresetMode
            presets={presets}
            onLaunch={wrappedLaunch}
            onOpenSettings={onOpenSettings}
            onClose={onClose}
            authSelector={
              <AuthSourceSelector
                sources={credentialedAuthSources}
                value={copyAuthFrom}
                onChange={setCopyAuthFrom}
              />
            }
          />
        ) : (
          <ManualMode
            sshHosts={sshHosts}
            agentCommands={agentCommands}
            onAddCommand={onAddCommand}
            onRemoveCommand={onRemoveCommand}
            onLaunch={wrappedLaunch}
            onClose={onClose}
            authSelector={
              <AuthSourceSelector
                sources={credentialedAuthSources}
                value={copyAuthFrom}
                onChange={setCopyAuthFrom}
              />
            }
          />
        )}
      </div>
    </div>
  );
}

// ─── 認証元セレクタ ──────────────────────────────────────────────────────────

function AuthSourceSelector({
  sources,
  value,
  onChange,
}: {
  sources: AuthInfo[];
  value: string;
  onChange: (id: string) => void;
}) {
  if (sources.length === 0) return null;
  return (
    <div className="form-group">
      <label className="form-label">認証元</label>
      <select
        className="settings-input"
        value={value}
        onChange={(e) => onChange(e.target.value)}
      >
        <option value="">新規 (空)</option>
        {sources.map((a) => (
          <option key={a.id} value={a.id}>{a.name}</option>
        ))}
      </select>
      <span className="form-hint">
        既存インスタンスの認証情報 (.credentials.json) を新インスタンスにコピーします。空欄の場合は claude code の /login で個別認証してください。
      </span>
    </div>
  );
}

// ─── プリセットモード ────────────────────────────────────────────────────────

function PresetMode({
  presets,
  onLaunch,
  onOpenSettings,
  onClose,
  authSelector,
}: {
  presets: ConnectionPreset[];
  onLaunch: (config: LaunchConfig) => void;
  onOpenSettings: () => void;
  onClose: () => void;
  authSelector?: React.ReactNode;
}) {
  const handleSelect = useCallback(
    (preset: ConnectionPreset) => {
      const target =
        preset.target.type === "local" ? "local" : preset.target.host_alias;
      const name =
        preset.name && preset.name.trim() !== ""
          ? preset.name
          : deriveDefaultName(target, preset.directory);
      onLaunch({
        target,
        command: preset.command,
        directory: preset.directory,
        name,
        agentProfile: preset.agent_profile || "default",
      });
    },
    [onLaunch]
  );

  const handleEditSettings = useCallback(() => {
    onClose();
    onOpenSettings();
  }, [onClose, onOpenSettings]);

  return (
    <div className="new-instance-body">
      {presets.length === 0 ? (
        <div className="preset-empty-state">
          <p>接続先が登録されていません。</p>
          <button className="btn-primary btn-sm" onClick={handleEditSettings}>
            接続先を設定する
          </button>
        </div>
      ) : (
        <ul className="preset-launch-list">
          {presets.map((p) => (
            <li
              key={p.id}
              className="preset-launch-item"
              onClick={() => handleSelect(p)}
            >
              <div className="preset-launch-info">
                <span className="preset-launch-name">{p.name || "(未設定)"}</span>
                <span className="preset-launch-detail">
                  {p.target.type === "local" ? "Local" : p.target.host_alias}
                  {p.directory ? ` - ${p.directory}` : ""}
                  {p.command ? ` / ${p.command}` : ""}
                  {p.agent_profile && p.agent_profile !== "default" ? ` [${p.agent_profile}]` : ""}
                </span>
              </div>
              <span className="preset-launch-arrow">&rarr;</span>
            </li>
          ))}
        </ul>
      )}

      {authSelector}

      <div className="settings-footer">
        <button className="btn-cancel" onClick={handleEditSettings}>
          接続先を編集
        </button>
      </div>
    </div>
  );
}

// ─── 手動接続モード ──────────────────────────────────────────────────────────

function ManualMode({
  sshHosts,
  agentCommands,
  onAddCommand,
  onRemoveCommand,
  onLaunch,
  onClose,
  authSelector,
}: {
  sshHosts: SshHost[];
  agentCommands: string[];
  onAddCommand: (cmd: string) => void;
  onRemoveCommand: (cmd: string) => void;
  onLaunch: (config: LaunchConfig) => void;
  onClose: () => void;
  authSelector?: React.ReactNode;
}) {
  const [connectionType, setConnectionType] = useState<"local" | "ssh">("local");
  const [selectedHost, setSelectedHost] = useState(sshHosts[0]?.alias ?? "");
  const [hostFilter, setHostFilter] = useState("");
  const [selectedCommand, setSelectedCommand] = useState(agentCommands[0] ?? "claude");
  const [newCommand, setNewCommand] = useState("");
  const [showAddCommand, setShowAddCommand] = useState(false);
  const [directory, setDirectory] = useState("");
  const [showDirDropdown, setShowDirDropdown] = useState(false);
  const [agentProfile, setAgentProfile] = useState("default");
  const [name, setName] = useState("");
  const [nameDirty, setNameDirty] = useState(false);
  const profileChoices = useClaudeProfiles();
  const { getHistory } = useDirectoryHistory();

  const target = connectionType === "local" ? "local" : selectedHost;
  const dirHistory = useMemo(() => getHistory(target), [getHistory, target]);

  // 接続先 / ディレクトリの変化に応じて接続名のデフォルトを自動更新する。
  // ユーザーが name 入力欄を編集した（nameDirty=true）後は自動更新を停止する。
  useEffect(() => {
    if (nameDirty) return;
    setName(deriveDefaultName(target, directory));
  }, [target, directory, nameDirty]);

  const filteredHosts = useMemo(() => {
    if (!hostFilter) return sshHosts;
    const q = hostFilter.toLowerCase();
    return sshHosts.filter(
      (h) => h.alias.toLowerCase().includes(q) || h.hostname.toLowerCase().includes(q)
    );
  }, [sshHosts, hostFilter]);

  const filteredDirHistory = useMemo(() => {
    if (!directory) return dirHistory;
    return dirHistory.filter((d) => d.toLowerCase().startsWith(directory.toLowerCase()));
  }, [dirHistory, directory]);

  const handleLaunch = useCallback(() => {
    onLaunch({
      target,
      command: selectedCommand,
      directory: directory.trim(),
      name: name.trim(),
      agentProfile: agentProfile.trim() || "default",
    });
  }, [target, selectedCommand, directory, name, agentProfile, onLaunch]);

  const handleAddCommand = useCallback(() => {
    const trimmed = newCommand.trim();
    if (trimmed) {
      onAddCommand(trimmed);
      setSelectedCommand(trimmed);
      setNewCommand("");
      setShowAddCommand(false);
    }
  }, [newCommand, onAddCommand]);

  const canLaunch =
    (connectionType === "local" || selectedHost !== "") &&
    selectedCommand !== "" &&
    name.trim() !== "";

  return (
    <div className="new-instance-body">
      {/* 接続名 */}
      <div className="form-group">
        <label className="form-label">接続名</label>
        <input
          className="settings-input"
          type="text"
          value={name}
          onChange={(e) => { setName(e.target.value); setNameDirty(true); }}
          placeholder="一覧画面に表示される名前 (必須)"
          spellCheck={false}
        />
        <span className="form-hint">
          接続先・作業ディレクトリから自動入力されます。任意の名前に書き換え可能です。
        </span>
      </div>

      {/* 接続先 */}
      <div className="form-group">
        <label className="form-label">接続先</label>
        <div className="connection-toggle">
          <button
            className={`connection-toggle-btn${connectionType === "local" ? " active" : ""}`}
            onClick={() => setConnectionType("local")}
          >
            Local
          </button>
          <button
            className={`connection-toggle-btn${connectionType === "ssh" ? " active" : ""}`}
            onClick={() => setConnectionType("ssh")}
          >
            SSH
          </button>
        </div>

        {connectionType === "ssh" && (
          <div className="ssh-host-select">
            <input
              className="settings-input"
              type="text"
              placeholder="ホスト名で絞り込み..."
              value={hostFilter}
              onChange={(e) => setHostFilter(e.target.value)}
            />
            <ul className="host-list compact">
              {filteredHosts.map((h) => (
                <li
                  key={h.alias}
                  className={`host-item${h.alias === selectedHost ? " selected" : ""}`}
                  onClick={() => { setSelectedHost(h.alias); setHostFilter(""); }}
                >
                  <div className="host-alias">{h.alias}</div>
                  <div className="host-details">
                    {h.user ? `${h.user}@` : ""}
                    {h.hostname}
                    {h.port !== 22 ? `:${h.port}` : ""}
                  </div>
                </li>
              ))}
              {filteredHosts.length === 0 && (
                <li className="host-empty">一致するホストがありません</li>
              )}
            </ul>
          </div>
        )}
      </div>

      {/* コマンド */}
      <div className="form-group">
        <label className="form-label">コマンド</label>
        <div className="command-row">
          <select
            className="settings-input command-select"
            value={selectedCommand}
            onChange={(e) => setSelectedCommand(e.target.value)}
          >
            {agentCommands.map((cmd) => (
              <option key={cmd} value={cmd}>{cmd}</option>
            ))}
          </select>
          <button
            className="btn-icon"
            title="コマンドを追加"
            onClick={() => setShowAddCommand(!showAddCommand)}
          >+</button>
          {agentCommands.length > 1 && (
            <button
              className="btn-icon btn-icon-danger"
              title="選択中のコマンドを削除"
              onClick={() => {
                onRemoveCommand(selectedCommand);
                setSelectedCommand(agentCommands.find((c) => c !== selectedCommand) ?? "claude");
              }}
            >-</button>
          )}
        </div>
        {showAddCommand && (
          <div className="command-add-row">
            <input
              className="settings-input"
              type="text"
              value={newCommand}
              onChange={(e) => setNewCommand(e.target.value)}
              onKeyDown={(e) => { if (e.key === "Enter") { e.stopPropagation(); handleAddCommand(); } }}
              placeholder="実行コマンドを入力"
              autoFocus
              spellCheck={false}
            />
            <button className="btn-primary btn-sm" onClick={handleAddCommand}>追加</button>
          </div>
        )}
        <span className="form-hint">
          インスタンス起動時に実行されるコマンド。フルパスやラッパーコマンドも指定可能。
        </span>
      </div>

      {/* 作業ディレクトリ */}
      <div className="form-group">
        <label className="form-label">作業ディレクトリ</label>
        <div className="dir-combo">
          <input
            className="settings-input"
            type="text"
            value={directory}
            onChange={(e) => { setDirectory(e.target.value); setShowDirDropdown(true); }}
            onFocus={() => setShowDirDropdown(true)}
            onBlur={() => setTimeout(() => setShowDirDropdown(false), 150)}
            placeholder={connectionType === "local" ? "~ (HOME)" : "絶対パスを入力"}
            spellCheck={false}
            autoComplete="off"
          />
          {showDirDropdown && filteredDirHistory.length > 0 && (
            <ul className="dir-dropdown">
              {filteredDirHistory.map((d) => (
                <li
                  key={d}
                  className="dir-dropdown-item"
                  onMouseDown={(e) => { e.preventDefault(); setDirectory(d); setShowDirDropdown(false); }}
                >
                  {d}
                </li>
              ))}
            </ul>
          )}
        </div>
        <span className="form-hint">
          {connectionType === "local"
            ? "空欄の場合は HOME ディレクトリで起動します。~ も使えます。"
            : "空欄の場合はリモートのデフォルトディレクトリで起動します。絶対パスを指定してください。"}
        </span>
      </div>

      {/* Claude プロファイル */}
      <div className="form-group">
        <label className="form-label">
          Claude プロファイル
          <span className="form-hint" style={{ marginLeft: 8 }}>
            ~/.ccc/agent_settings/claude/&lt;name&gt;/
          </span>
        </label>
        <input
          className="settings-input"
          type="text"
          list="manual-profile-list"
          value={agentProfile}
          onChange={(e) => setAgentProfile(e.target.value)}
          placeholder="default"
          spellCheck={false}
          style={{ fontFamily: "monospace" }}
        />
        <datalist id="manual-profile-list">
          {profileChoices.map((p) => (
            <option key={p} value={p} />
          ))}
        </datalist>
      </div>

      {authSelector}

      <div className="settings-footer">
        <button className="btn-cancel" onClick={onClose}>キャンセル</button>
        <button className="btn-primary" onClick={handleLaunch} disabled={!canLaunch}>起動</button>
      </div>
    </div>
  );
}
