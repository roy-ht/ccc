import { useState, useCallback, useEffect, useRef } from "react";
import { invoke } from "@tauri-apps/api/core";
import { AppSettings, ConnectionPreset, PresetTarget, SshHost, CliToolStatus, InstallResult } from "../types";
import { useClaudeProfiles } from "../hooks/useClaudeProfiles";
import { TERMINAL_THEMES, DEFAULT_TERMINAL_THEME_ID } from "../constants";
import { ConfirmDialog } from "./ConfirmDialog";

type Category = "display" | "connections" | "tools";

interface Props {
  settings: AppSettings;
  onSave: (settings: AppSettings) => Promise<void>;
  onClose: () => void;
}

export function SettingsPage({ settings, onSave, onClose }: Props) {
  const [category, setCategory] = useState<Category>("display");

  const handleKeyDown = useCallback(
    (e: React.KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    },
    [onClose]
  );

  return (
    <div className="settings-page" onKeyDown={handleKeyDown}>
      <div className="settings-page-header">
        <span className="settings-page-title">設定</span>
        <button className="btn-cancel" onClick={onClose}>閉じる</button>
      </div>
      <div className="settings-page-body">
        <nav className="settings-nav">
          <button
            className={`settings-nav-item${category === "display" ? " active" : ""}`}
            onClick={() => setCategory("display")}
          >
            画面設定
          </button>
          <button
            className={`settings-nav-item${category === "connections" ? " active" : ""}`}
            onClick={() => setCategory("connections")}
          >
            接続先設定
          </button>
          <button
            className={`settings-nav-item${category === "tools" ? " active" : ""}`}
            onClick={() => setCategory("tools")}
          >
            ツール
          </button>
        </nav>
        <div className="settings-detail">
          {category === "display" && (
            <DisplaySettingsPanel settings={settings} onSave={onSave} />
          )}
          {category === "connections" && (
            <ConnectionSettingsPanel settings={settings} onSave={onSave} />
          )}
          {category === "tools" && <ToolsSettingsPanel />}
        </div>
      </div>
    </div>
  );
}

// ─── 画面設定パネル ──────────────────────────────────────────────────────────

function DisplaySettingsPanel({
  settings,
  onSave,
}: {
  settings: AppSettings;
  onSave: (s: AppSettings) => Promise<void>;
}) {
  const [fontFamily, setFontFamily] = useState(settings.display.font_family);
  const [fontSize, setFontSize] = useState(String(settings.display.font_size));
  const [colorTheme, setColorTheme] = useState(
    TERMINAL_THEMES[settings.display.color_theme] ? settings.display.color_theme : DEFAULT_TERMINAL_THEME_ID
  );
  const [scrollbackLines, setScrollbackLines] = useState(String(settings.display.scrollback_lines ?? 1000));
  const [statusMessageLines, setStatusMessageLines] = useState(String(settings.display.status_message_lines ?? 2));
  const [systemFonts, setSystemFonts] = useState<string[]>([]);
  const [showFontList, setShowFontList] = useState(false);
  const inputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    invoke<string[]>("list_system_fonts").then(setSystemFonts).catch(() => {});
  }, []);

  // settings prop が変わったら同期
  useEffect(() => {
    setFontFamily(settings.display.font_family);
    setFontSize(String(settings.display.font_size));
    setColorTheme(
      TERMINAL_THEMES[settings.display.color_theme] ? settings.display.color_theme : DEFAULT_TERMINAL_THEME_ID
    );
    setScrollbackLines(String(settings.display.scrollback_lines ?? 1000));
    setStatusMessageLines(String(settings.display.status_message_lines ?? 2));
  }, [settings.display.font_family, settings.display.font_size, settings.display.color_theme, settings.display.scrollback_lines, settings.display.status_message_lines]);

  const query = fontFamily.toLowerCase().replace(/['"]/g, "").split(",")[0].trim();
  const filteredFonts = systemFonts.filter((f) =>
    f.toLowerCase().startsWith(query)
  );

  const handleSelectFont = useCallback((name: string) => {
    setFontFamily(`"${name}", monospace`);
    setShowFontList(false);
    inputRef.current?.focus();
  }, []);

  const handleSave = useCallback(async () => {
    const size = parseInt(fontSize, 10);
    const scrollback = parseInt(scrollbackLines, 10);
    const msgLines = parseInt(statusMessageLines, 10);
    const updated: AppSettings = {
      ...settings,
      display: {
        ...settings.display,
        font_family: fontFamily.trim() || settings.display.font_family,
        font_size: Number.isFinite(size) && size > 0 ? size : settings.display.font_size,
        color_theme: TERMINAL_THEMES[colorTheme] ? colorTheme : DEFAULT_TERMINAL_THEME_ID,
        scrollback_lines: Number.isFinite(scrollback) && scrollback >= 0 ? scrollback : settings.display.scrollback_lines,
        status_message_lines:
          Number.isFinite(msgLines) && msgLines >= 1 && msgLines <= 10
            ? msgLines
            : settings.display.status_message_lines,
      },
    };
    await onSave(updated);
  }, [fontFamily, fontSize, colorTheme, scrollbackLines, statusMessageLines, settings, onSave]);

  return (
    <div className="settings-panel">
      <h3 className="settings-panel-title">画面設定</h3>

      <label className="settings-label">
        フォントファミリー
        <span className="settings-hint">CSS font-family 形式</span>
      </label>
      <div className="settings-font-wrap">
        <input
          ref={inputRef}
          className="settings-input"
          type="text"
          value={fontFamily}
          onChange={(e) => { setFontFamily(e.target.value); setShowFontList(true); }}
          onFocus={() => setShowFontList(true)}
          onBlur={() => setTimeout(() => setShowFontList(false), 150)}
          placeholder='"Cascadia Code", monospace'
          spellCheck={false}
          autoComplete="off"
        />
        {showFontList && filteredFonts.length > 0 && (
          <ul className="font-dropdown">
            {filteredFonts.map((name) => (
              <li
                key={name}
                className="font-dropdown-item"
                style={{ fontFamily: name }}
                onMouseDown={(e) => { e.preventDefault(); handleSelectFont(name); }}
              >
                {name}
              </li>
            ))}
          </ul>
        )}
      </div>

      <label className="settings-label">フォントサイズ（px）</label>
      <input
        className="settings-input settings-input-narrow"
        type="number"
        min={8}
        max={32}
        value={fontSize}
        onChange={(e) => setFontSize(e.target.value)}
        onFocus={() => setShowFontList(false)}
      />

      <label className="settings-label">
        ターミナルテーマ
        <span className="settings-hint">保存後、再生成されたターミナルから適用されます</span>
      </label>
      <select
        className="settings-input settings-input-narrow"
        value={colorTheme}
        onChange={(e) => setColorTheme(e.target.value)}
        onFocus={() => setShowFontList(false)}
      >
        {Object.values(TERMINAL_THEMES).map((preset) => (
          <option key={preset.id} value={preset.id}>
            {preset.label}
          </option>
        ))}
      </select>

      <label className="settings-label">
        復帰時スクロールバック復元行数
        <span className="settings-hint">0 で無効</span>
      </label>
      <input
        className="settings-input settings-input-narrow"
        type="number"
        min={0}
        max={10000}
        value={scrollbackLines}
        onChange={(e) => setScrollbackLines(e.target.value)}
        onFocus={() => setShowFontList(false)}
      />

      <label className="settings-label">
        サイドバー状態メッセージ行数
        <span className="settings-hint">高さ固定。はみ出した分は表示されません</span>
      </label>
      <input
        className="settings-input settings-input-narrow"
        type="number"
        min={1}
        max={10}
        value={statusMessageLines}
        onChange={(e) => setStatusMessageLines(e.target.value)}
        onFocus={() => setShowFontList(false)}
      />

      <div className="settings-panel-actions">
        <button className="btn-primary" onClick={handleSave}>保存</button>
      </div>
    </div>
  );
}

// ─── 接続先設定パネル ────────────────────────────────────────────────────────

function ConnectionSettingsPanel({
  settings,
  onSave,
}: {
  settings: AppSettings;
  onSave: (s: AppSettings) => Promise<void>;
}) {
  const [editingId, setEditingId] = useState<string | null>(null);
  const [pendingRemoveId, setPendingRemoveId] = useState<string | null>(null);

  const handleAdd = useCallback(() => {
    const newPreset: ConnectionPreset = {
      id: crypto.randomUUID(),
      name: "",
      target: { type: "local" },
      command: "claude",
      directory: "",
      agent_profile: "default",
    };
    setEditingId(newPreset.id);
    onSave({
      ...settings,
      connections: [...settings.connections, newPreset],
    });
  }, [settings, onSave]);

  const handleUpdate = useCallback(
    async (preset: ConnectionPreset) => {
      const updated: AppSettings = {
        ...settings,
        connections: settings.connections.map((c) =>
          c.id === preset.id ? preset : c
        ),
      };
      await onSave(updated);
    },
    [settings, onSave]
  );

  const handleRemove = useCallback(
    async (id: string) => {
      if (editingId === id) setEditingId(null);
      const updated: AppSettings = {
        ...settings,
        connections: settings.connections.filter((c) => c.id !== id),
      };
      await onSave(updated);
    },
    [settings, onSave, editingId]
  );

  const pendingRemoveTarget = pendingRemoveId
    ? settings.connections.find((c) => c.id === pendingRemoveId) ?? null
    : null;

  const editing = settings.connections.find((c) => c.id === editingId) ?? null;

  return (
    <div className="settings-panel">
      <h3 className="settings-panel-title">接続先設定</h3>

      <div className="preset-list-header">
        <span className="preset-list-label">登録済み接続先</span>
        <button className="btn-primary btn-sm" onClick={handleAdd}>追加</button>
      </div>

      {settings.connections.length === 0 && (
        <p className="preset-empty">接続先がまだ登録されていません。「追加」ボタンで登録できます。</p>
      )}

      <ul className="preset-list">
        {settings.connections.map((c) => (
          <li
            key={c.id}
            className={`preset-item${c.id === editingId ? " active" : ""}`}
            onClick={() => setEditingId(c.id)}
          >
            <div className="preset-item-info">
              <span className="preset-item-name">{c.name || "(未設定)"}</span>
              <span className="preset-item-detail">
                {c.target.type === "local" ? "Local" : c.target.host_alias}
                {c.command ? ` / ${c.command}` : ""}
                {c.agent_profile && c.agent_profile !== "default" ? ` [${c.agent_profile}]` : ""}
              </span>
            </div>
            <button
              className="btn-close"
              title="この接続先を削除"
              onClick={(e) => { e.stopPropagation(); setPendingRemoveId(c.id); }}
            >
              ×
            </button>
          </li>
        ))}
      </ul>

      {editing && (
        <ConnectionPresetEditor
          preset={editing}
          onUpdate={handleUpdate}
        />
      )}

      {pendingRemoveTarget && (
        <ConfirmDialog
          title="接続先を削除"
          message={`「${pendingRemoveTarget.name || "(未設定)"}」を削除します。よろしいですか？`}
          confirmLabel="削除"
          destructive
          onConfirm={() => {
            const id = pendingRemoveTarget.id;
            setPendingRemoveId(null);
            handleRemove(id);
          }}
          onCancel={() => setPendingRemoveId(null)}
        />
      )}
    </div>
  );
}

// ─── 接続先プリセットエディタ ────────────────────────────────────────────────

function ConnectionPresetEditor({
  preset,
  onUpdate,
}: {
  preset: ConnectionPreset;
  onUpdate: (p: ConnectionPreset) => Promise<void>;
}) {
  const [name, setName] = useState(preset.name);
  const [targetType, setTargetType] = useState<"local" | "remote">(preset.target.type);
  const [command, setCommand] = useState(preset.command);
  const [directory, setDirectory] = useState(preset.directory);
  const [hostAlias, setHostAlias] = useState(
    preset.target.type === "remote" ? preset.target.host_alias : ""
  );
  const [agentProfile, setAgentProfile] = useState(preset.agent_profile || "default");
  const profileChoices = useClaudeProfiles(preset.agent_profile || "default");
  const [sshHosts, setSshHosts] = useState<SshHost[]>([]);
  const [showHostPicker, setShowHostPicker] = useState(false);

  // preset が変わったら同期
  useEffect(() => {
    setName(preset.name);
    setTargetType(preset.target.type);
    setCommand(preset.command);
    setDirectory(preset.directory);
    setAgentProfile(preset.agent_profile || "default");
    if (preset.target.type === "remote") {
      setHostAlias(preset.target.host_alias);
    } else {
      setHostAlias("");
    }
  }, [preset.id]);

  const handleSave = useCallback(async () => {
    let target: PresetTarget;
    if (targetType === "remote") {
      target = {
        type: "remote",
        host_alias: hostAlias,
      };
    } else {
      target = { type: "local" };
    }
    await onUpdate({
      ...preset,
      name: name.trim(),
      target,
      command: command.trim() || "claude",
      directory: directory.trim(),
      agent_profile: agentProfile.trim() || "default",
    });
  }, [preset, name, targetType, command, directory, hostAlias, agentProfile, onUpdate]);

  const handleLoadSshHosts = useCallback(async () => {
    try {
      const hosts = await invoke<SshHost[]>("list_ssh_hosts");
      setSshHosts(hosts);
      setShowHostPicker(true);
    } catch {
      setSshHosts([]);
    }
  }, []);

  const handlePickHost = useCallback((alias: string) => {
    setShowHostPicker(false);
    setHostAlias(alias);
  }, []);

  return (
    <div className="preset-editor">
      <h4 className="preset-editor-title">接続先の編集</h4>

      <label className="settings-label">名前</label>
      <input
        className="settings-input"
        type="text"
        value={name}
        onChange={(e) => setName(e.target.value)}
        placeholder="例: ccc, myproject"
        spellCheck={false}
      />

      <label className="settings-label">ターゲット</label>
      <div className="connection-toggle">
        <button
          className={`connection-toggle-btn${targetType === "local" ? " active" : ""}`}
          onClick={() => setTargetType("local")}
        >
          Local
        </button>
        <button
          className={`connection-toggle-btn${targetType === "remote" ? " active" : ""}`}
          onClick={() => setTargetType("remote")}
        >
          Remote
        </button>
      </div>

      <label className="settings-label">コマンド</label>
      <input
        className="settings-input"
        type="text"
        value={command}
        onChange={(e) => setCommand(e.target.value)}
        placeholder="claude"
        spellCheck={false}
        style={{ fontFamily: "monospace" }}
      />

      <label className="settings-label">作業ディレクトリ</label>
      <input
        className="settings-input"
        type="text"
        value={directory}
        onChange={(e) => setDirectory(e.target.value)}
        placeholder={targetType === "local" ? "~/projects/myapp" : "/home/user/work"}
        spellCheck={false}
        style={{ fontFamily: "monospace" }}
      />

      <label className="settings-label">
        Claude プロファイル
        <span className="settings-hint">~/.ccc/agent_settings/claude/&lt;name&gt;/</span>
      </label>
      <input
        className="settings-input"
        type="text"
        list={`profile-list-${preset.id}`}
        value={agentProfile}
        onChange={(e) => setAgentProfile(e.target.value)}
        placeholder="default"
        spellCheck={false}
        style={{ fontFamily: "monospace" }}
      />
      <datalist id={`profile-list-${preset.id}`}>
        {profileChoices.map((p) => (
          <option key={p} value={p} />
        ))}
      </datalist>

      {targetType === "remote" && (
        <>
          <div className="preset-ssh-header">
            <label className="settings-label">
              ホストエイリアス
              <span className="settings-hint">~/.ssh/config の Host 名</span>
            </label>
            <button className="btn-primary btn-sm" onClick={handleLoadSshHosts}>
              一覧から選択
            </button>
          </div>

          {showHostPicker && (
            <ul className="host-list compact">
              {sshHosts.map((h) => (
                <li
                  key={h.alias}
                  className={`host-item${h.alias === hostAlias ? " selected" : ""}`}
                  onClick={() => handlePickHost(h.alias)}
                >
                  <div className="host-alias">{h.alias}</div>
                  <div className="host-details">
                    {h.user ? `${h.user}@` : ""}
                    {h.hostname}
                    {h.port !== 22 ? `:${h.port}` : ""}
                  </div>
                </li>
              ))}
            </ul>
          )}

          <input
            className="settings-input"
            type="text"
            value={hostAlias}
            onChange={(e) => setHostAlias(e.target.value)}
            placeholder="例: dev-host"
            spellCheck={false}
          />
        </>
      )}

      <div className="settings-panel-actions">
        <button className="btn-primary" onClick={handleSave}>保存</button>
      </div>
    </div>
  );
}

// ─── ツール設定パネル ────────────────────────────────────────────────────────

function ToolsSettingsPanel() {
  const [status, setStatus] = useState<CliToolStatus | null>(null);
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(() => {
    invoke<CliToolStatus>("cli_tool_status")
      .then(setStatus)
      .catch(() => setStatus(null));
  }, []);

  useEffect(() => {
    refresh();
  }, [refresh]);

  const handleInstall = useCallback(async () => {
    setBusy(true);
    setError(null);
    setMessage(null);
    try {
      const result = await invoke<InstallResult>("install_cli_tool");
      const note = result.in_path
        ? ""
        : "（このディレクトリは PATH に含まれていません。シェル設定への追加が必要です）";
      setMessage(`インストールしました: ${result.link_path}${note}`);
      refresh();
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  }, [refresh]);

  const stateLabel = !status
    ? "確認中…"
    : !status.installed
      ? "未インストール"
      : status.up_to_date
        ? "✓ インストール済み（最新）"
        : "⚠ インストール済み（古い／別のバイナリを指しています）";

  return (
    <div className="settings-panel">
      <h3 className="settings-panel-title">コマンドラインツール</h3>
      <p className="settings-hint" style={{ display: "block", marginBottom: 12 }}>
        同梱 CLI（<code>ccc-sessions</code>: セッション検索・振り返りの外部連携 /{" "}
        <code>ccc-ssh</code>: forward 台帳・gpg 修復付き ssh ラッパー）を{" "}
        <code>~/.local/bin</code> に symlink して、ターミナルから呼べるようにします。
      </p>

      <div className="settings-label" style={{ display: "block" }}>
        <div>同梱バイナリ: {status?.bundled_found ? "✓ あり" : "✗ 見つかりません"}</div>
        <div>
          リンク先: <code>{status?.link_path ?? "(未定)"}</code>
          {status && !status.in_path && (
            <span style={{ color: "#e5c07b" }}>（PATH 外）</span>
          )}
        </div>
        <div>状態: {stateLabel}</div>
      </div>

      {status && status.installed && !status.in_path && (
        <div className="settings-hint" style={{ display: "block", marginTop: 8 }}>
          <code>~/.local/bin</code> が PATH に含まれていません。お使いのシェルの設定ファイルに
          次の行を追記してください（追記後、新しいターミナルから有効になります）:
          <pre
            style={{
              margin: "6px 0",
              padding: "8px",
              background: "rgba(0,0,0,0.25)",
              borderRadius: 4,
              overflowX: "auto",
            }}
          >
            export PATH="$HOME/.local/bin:$PATH"
          </pre>
          zsh は <code>~/.zshrc</code>、bash は <code>~/.bashrc</code>（または{" "}
          <code>~/.bash_profile</code>）に追記します。
        </div>
      )}

      {error && (
        <p className="settings-hint" style={{ display: "block", color: "#e06c75" }}>
          失敗しました: {error}
        </p>
      )}
      {message && (
        <p className="settings-hint" style={{ display: "block", color: "#98c379" }}>
          {message}
        </p>
      )}

      <div className="settings-panel-actions">
        <button
          className="btn-primary"
          onClick={handleInstall}
          disabled={busy || !status?.bundled_found}
        >
          {status?.installed ? "再インストール／更新" : "インストール"}
        </button>
      </div>
    </div>
  );
}
