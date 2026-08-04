export type InstanceId = string;

export type InstanceKind = "local" | "remote";

export type InstanceStatus =
  | "connecting"
  | "running"
  | "agent_idle"
  | "agent_busy"
  | "agent_waiting_input"
  | "disconnected"
  | "terminated";

export interface PromptOption {
  key: string;
  label: string;
}

export type PendingPrompt =
  | { kind: "permission"; description: string; options: PromptOption[] }
  | { kind: "plan" };

export interface InstanceInfo {
  id: InstanceId;
  kind: InstanceKind;
  name: string;
  status: InstanceStatus;
  host_alias?: string | null;
  directory?: string | null;
  command?: string | null;
  tmux_session?: string | null;
  instance_hash: string;
  instance_dir: string;
  agent_profile: string;
  status_message?: string | null;
  pending_prompt?: PendingPrompt | null;
  /** 現在ライブで動いている claude code セッションの session_id（Sessions タブのマーカー用） */
  current_session_id?: string | null;
  /** 現在ライブのセッションの「タイトル」＝最初のユーザープロンプト先頭。 */
  session_title?: string | null;
}

export interface StatusChangedPayload {
  id: InstanceId;
  status: InstanceStatus;
  status_message?: string | null;
  pending_prompt?: PendingPrompt | null;
  current_session_id?: string | null;
  session_title?: string | null;
}

export interface AuthInfo {
  id: InstanceId;
  name: string;
  has_credentials: boolean;
  copied_from: string | null;
}

export interface SshHost {
  alias: string;
  hostname: string;
  port: number;
  user: string | null;
  identity_file: string | null;
}

export interface OutputPayload {
  instance_id: string;
  data: number[];
}

// ─── 設定 ────────────────────────────────────────────────────────────────────

/** xterm.js `ITheme` 互換のサブセット。プリセット定義に使う。 */
export interface TerminalTheme {
  background: string;
  foreground: string;
  cursor: string;
  selectionBackground: string;
  black: string;
  red: string;
  green: string;
  yellow: string;
  blue: string;
  magenta: string;
  cyan: string;
  white: string;
  brightBlack: string;
  brightRed: string;
  brightGreen: string;
  brightYellow: string;
  brightBlue: string;
  brightMagenta: string;
  brightCyan: string;
  brightWhite: string;
}

export interface DisplaySettings {
  font_family: string;
  font_size: number;
  color_theme: string;
  scrollback_lines: number;
  /** サイドバーの状態メッセージ表示行数（固定高さ、はみ出しは clip） */
  status_message_lines: number;
  /**
   * ターミナルの WebGL レンダラを使うか（既定 true）。WKWebView の WebGL 合成に
   * 不具合がある環境（テキスト選択位置のずれ・描画乱れ）では false で DOM レンダラに
   * 切替できる。
   */
  use_webgl: boolean;
}

export interface RemoteConfig {
  host_alias: string;
}

export type PresetTarget =
  | { type: "local" }
  | { type: "remote" } & RemoteConfig;

export interface ConnectionPreset {
  id: string;
  name: string;
  target: PresetTarget;
  command: string;
  directory: string;
  /** ~/.ccc/agent_settings/claude/<name>/ — 既定は "default" */
  agent_profile: string;
}

export interface AppSettings {
  display: DisplaySettings;
  connections: ConnectionPreset[];
}

// ─── アーカイブ（Sessions / Memories 主画面タブ） ──────────────────────────────

/** 主画面のタブ。インスタンス切替時は常に terminal にリセットする。 */
export type MainTab = "terminal" | "shell" | "sessions" | "memories" | "explorer" | "forwards";

/** セッション 1 件（`archive_list_sessions`）。Rust `SessionRow` と対応。 */
export interface SessionRow {
  session_id: string;
  instance_name?: string | null;
  kind?: string | null;
  host_alias?: string | null;
  project?: string | null;
  source?: string | null;
  attribution?: string | null;
  started_at?: number | null;
  ended_at?: number | null;
  message_count: number;
  summary?: string | null;
}

/** 検索ヒットしたセッション（`archive_search_sessions`）。`hits` はマッチ件数。 */
export interface SessionHit extends SessionRow {
  hits: number;
}

/** セッション内メッセージ 1 件（`archive_session_messages`）。Rust `MessageRow` と対応。 */
export interface MessageRow {
  id: number;
  seq?: number | null;
  ts?: number | null;
  role?: string | null;
  msg_type?: string | null;
  tool_name?: string | null;
  is_sidechain: boolean;
  agent_id?: string | null;
  text?: string | null;
  raw?: string | null;
}

/** メモリ 1 件の最新版（`archive_list_memory`）。Rust `MemoryEntry` と対応。 */
export interface MemoryEntry {
  agent_profile?: string | null;
  rel_path: string;
  scope?: string | null;
  project?: string | null;
  source_kind?: string | null;
  host_alias?: string | null;
  content_hash?: string | null;
  captured_at?: number | null;
  versions: number;
}

// ─── コマンドラインツール（設定 > ツール） ──────────────────────────────────────

/** 同梱 CLI（ccc-sessions / ccc-ssh）の導入状況（`cli_tool_status`）。Rust `CliToolStatus` と対応。 */
export interface CliToolStatus {
  bundled_found: boolean;
  link_path: string;
  installed: boolean;
  up_to_date: boolean;
  /** link_path のディレクトリが PATH に含まれるか */
  in_path: boolean;
}

/** CLI インストール結果（`install_cli_tool`）。Rust `InstallResult` と対応。 */
export interface InstallResult {
  link_path: string;
  in_path: boolean;
}

// ─── Explorer タブ ───────────────────────────────────────────────────────────

/** ディレクトリ内エントリ。Rust `FileNode` と対応。 */
export interface FileNode {
  name: string;
  /** ルートからの POSIX 相対パス。空文字はルート自身。 */
  path: string;
  is_dir: boolean;
  size?: number | null;
  hidden: boolean;
}

/** ファイル種別判定結果。Rust `FileMeta` と対応。 */
export interface FileMeta {
  size: number;
  mime?: string | null;
  is_binary: boolean;
}

/** プレビュー応答。`kind` で振り分けする tagged union。Rust `Preview` と対応。 */
export type Preview =
  | { kind: "text"; content: string; language?: string | null; truncated: boolean; size: number }
  | { kind: "markdown"; content: string; truncated: boolean; size: number }
  | { kind: "image"; mime: string; base64: string; size: number }
  | { kind: "pdf"; base64: string; size: number }
  | { kind: "binary"; size: number; mime?: string | null }
  | { kind: "too_large"; size: number; limit: number };

/** ripgrep の 1 行マッチ。Rust `SearchHit` と対応。 */
export interface SearchHit {
  path: string;
  line_number: number;
  line: string;
  match_start: number;
  match_end: number;
}

/** D&D コピー結果。Rust `CopySummary` と対応。 */
export interface CopySummary {
  copied: number;
  failed: CopyFailure[];
  dest_rel: string;
}

export interface CopyFailure {
  source: string;
  error: string;
}

// ─── Forwards タブ（port forwarding 管理） ─────────────────────────────────────

/** forward 1 本分の指定。Rust `ForwardSpec` と対応。listen_host 省略時は localhost。 */
export interface ForwardSpec {
  listen_host?: string | null;
  listen_port: number;
  dest_host: string;
  dest_port: number;
}

/**
 * 一覧 1 行。Rust `ForwardRow` と対応。
 * origin: "ledger"=ccc 追加 / "config"=ssh config 定義 / "reserved"=hook 用 ccc 予約
 */
export interface ForwardRow {
  spec: ForwardSpec;
  origin: "ledger" | "config" | "reserved";
  reverse: boolean;
  stale: boolean;
  error?: string | null;
  deletable: boolean;
}
