import { ReactNode, useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { InstanceInfo, MessageRow, SessionHit, SessionRow } from "../types";
import { useDebouncedValue } from "../hooks/useDebouncedValue";
import { highlightText } from "../utils/highlight";
import { Markdown } from "./Markdown";
import { SyncStatus } from "./SyncStatus";
import {
  AskQuestion,
  DisplayPart,
  PartCategory,
  expandTurn,
  formatToolInput,
  partCategory,
} from "../utils/messageParts";
import { formatTimestamp, sessionTime } from "../utils/datetime";

interface Props {
  instance: InstanceInfo;
}

type ListItem = SessionRow & { hits?: number };

const SHOW_SYSTEM_KEY = "ccc.sessions.show-system";
const SHOW_TOOLS_KEY = "ccc.sessions.show-tools";
const SHOW_THINKING_KEY = "ccc.sessions.show-thinking";

/** localStorage に永続化する真偽トグル。 */
function usePersistentToggle(key: string, def = false): [boolean, (v: boolean) => void] {
  const [value, setValue] = useState<boolean>(() => {
    try {
      const s = localStorage.getItem(key);
      return s == null ? def : s === "1";
    } catch {
      return def;
    }
  });
  useEffect(() => {
    try {
      localStorage.setItem(key, value ? "1" : "0");
    } catch {
      /* localStorage 不可でも無視 */
    }
  }, [key, value]);
  return [value, setValue];
}

interface Filters {
  system: boolean;
  tools: boolean;
  thinking: boolean;
}

/** カテゴリがトグル設定で表示対象か（main は常に表示）。 */
function isVisibleCategory(cat: PartCategory, f: Filters): boolean {
  switch (cat) {
    case "thinking":
      return f.thinking;
    case "tool":
      return f.tools;
    case "system":
      return f.system;
    default:
      return true;
  }
}

/**
 * 本文を持たない system 行（フック実行サマリ・ターン計測などの内部メタ）を、
 * raw JSON から人間が読める説明に変換する。本文を持つ system（API エラー等）は
 * トップレベル `content` を優先する。
 */
function describeSystemMessage(raw: string | null | undefined): string {
  if (!raw) return "(system)";
  let j: any;
  try {
    j = JSON.parse(raw);
  } catch {
    return "(system)";
  }
  if (typeof j.content === "string" && j.content.trim()) {
    return j.content.trim();
  }
  const sub: string | undefined = j.subtype;
  switch (sub) {
    case "stop_hook_summary": {
      const n = typeof j.hookCount === "number" ? j.hookCount : 0;
      const errs = Array.isArray(j.hookErrors) ? j.hookErrors.length : 0;
      const stop = j.stopReason ? `, stopReason: ${j.stopReason}` : "";
      return `Stop フック実行サマリ（${n} 件${errs ? `, エラー ${errs}` : ""}${stop}）`;
    }
    case "turn_duration": {
      const ms = typeof j.durationMs === "number" ? j.durationMs : null;
      const mc = typeof j.messageCount === "number" ? j.messageCount : null;
      const parts = [
        ms != null ? `所要 ${(ms / 1000).toFixed(1)}s` : "",
        mc != null ? `${mc} msgs` : "",
      ].filter(Boolean);
      return `ターン計測（${parts.join(" / ")}）`;
    }
    default:
      return sub ? `system: ${sub}` : "(system)";
  }
}

/** 本物のユーザー入力か（ツール結果も role=user なので msg_type で除外）。 */
function isUserPrompt(m: MessageRow): boolean {
  return m.role === "user" && m.msg_type !== "tool_result";
}

/**
 * 時系列（seq 昇順）のメッセージ列を「ターン」単位に束ねる。
 * 1 ターン = ユーザー入力から次のユーザー入力の手前まで（その間の Claude 応答・
 * ツール呼び出し・ツール結果・system を含む）。先頭のユーザー入力前のメッセージ
 * （セッション開始通知など）は先頭ターンにまとまる。
 */
function buildTurns(messages: MessageRow[]): MessageRow[][] {
  const turns: MessageRow[][] = [];
  let current: MessageRow[] = [];
  for (const m of messages) {
    if (isUserPrompt(m) && current.length > 0) {
      turns.push(current);
      current = [];
    }
    current.push(m);
  }
  if (current.length > 0) turns.push(current);
  return turns;
}

/** これを超える本文は折りたたむ。 */
const COLLAPSE_CHARS = 700;
const COLLAPSE_LINES = 14;
// tool_result（コマンド出力など）は嵩みやすいので、より小さい閾値で畳む。
const TOOL_COLLAPSE_CHARS = 400;
const TOOL_COLLAPSE_LINES = 8;
// 1 ターンの可視 part がこれを超えたら、中間を省略してフローティングボタンを出す。
const TURN_HEAD = 2;
const TURN_TAIL = 1;

/**
 * メッセージ本文を表示する。長文はクリックで全文展開できるよう折りたたむ。
 *
 * - 検索中（query あり）: ハイライト箇所が隠れないよう常に全文・プレーン表示。
 * - `mono`（tool_result 等）: 等幅 `<pre>` で生のまま（Markdown 化しない）。
 * - それ以外: Markdown（GFM）として描画する。
 */
function MessageBody({
  text,
  query,
  mono = false,
}: {
  text: string;
  query: string | null;
  mono?: boolean;
}) {
  const [expanded, setExpanded] = useState(false);
  const collapseChars = mono ? TOOL_COLLAPSE_CHARS : COLLAPSE_CHARS;
  const collapseLines = mono ? TOOL_COLLAPSE_LINES : COLLAPSE_LINES;
  const long = text.length > collapseChars || text.split("\n").length > collapseLines;

  const inner: ReactNode = query
    ? highlightText(text, query)
    : mono
      ? text
      : <Markdown text={text} />;

  // 検索中はプレーン（ハイライト）、mono は等幅、通常は Markdown 用の余白付きクラス。
  const cls = mono ? "msg-pre" : query ? "msg-body" : "msg-md";
  const Wrapper = mono ? "pre" : "div";

  if (!long || query) {
    return <Wrapper className={cls}>{inner}</Wrapper>;
  }
  return (
    <div className="msg-collapsible">
      <Wrapper className={`${cls} ${expanded ? "" : "collapsed"}`}>{inner}</Wrapper>
      <button className="msg-expand" onClick={() => setExpanded((v) => !v)}>
        {expanded ? "折りたたむ" : `全文を表示（${text.length.toLocaleString()} 文字）`}
      </button>
    </div>
  );
}

/**
 * セッション履歴画面。選択中インスタンスの **接続ホスト(or local)＋作業ディレクトリ** に
 * 紐づくセッションを新しい順に一覧する。検索バーで全文検索すると一覧がヒットした
 * セッションに絞り込まれ、セッションを開くと各ターンを最新順（下にスクロールで過去）で
 * 表示する。検索状態のまま開くとヒットしたメッセージのみ＋検索語ハイライトになる。
 */
export function SessionsPanel({ instance }: Props) {
  const directory = instance.directory ?? "";
  const hostAlias = instance.host_alias ?? null;
  // このインスタンスで現在ライブのセッション（一覧・詳細でマーカー表示する）。
  const liveSessionId = instance.current_session_id ?? null;

  const [rawQuery, setRawQuery] = useState("");
  const trimmed = rawQuery.trim();
  const debouncedQuery = useDebouncedValue(trimmed, 250);
  // 空入力（クリア / Esc）はデバウンスを介さず即時 searching=false に落とす。
  // 入力中の絞り込みだけ debounce が効く挙動になる。
  const query = trimmed === "" ? "" : debouncedQuery;
  const searching = query !== "";

  const [list, setList] = useState<ListItem[]>([]);
  const [selected, setSelected] = useState<SessionRow | null>(null);
  const [messages, setMessages] = useState<MessageRow[]>([]);
  // 選択的表示トグル（既定はすべて隠す）。設定は localStorage に永続化する。
  // System=フック実行サマリ等の内部メタ、ツール=tool_use/tool_result、thinking=思考。
  const [showSystem, setShowSystem] = usePersistentToggle(SHOW_SYSTEM_KEY, false);
  const [showTools, setShowTools] = usePersistentToggle(SHOW_TOOLS_KEY, false);
  const [showThinking, setShowThinking] = usePersistentToggle(SHOW_THINKING_KEY, false);
  const filters = useMemo<Filters>(
    () => ({ system: showSystem, tools: showTools, thinking: showThinking }),
    [showSystem, showTools, showThinking]
  );
  const [listLoading, setListLoading] = useState(false);
  const [detailLoading, setDetailLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // 一覧の取得（検索語に応じて全件 or 絞り込み）。
  useEffect(() => {
    let cancelled = false;
    setListLoading(true);
    setError(null);
    const load = searching
      ? invoke<SessionHit[]>("archive_search_sessions", { directory, hostAlias, query })
      : invoke<SessionRow[]>("archive_list_sessions", { directory, hostAlias });
    load
      .then((rows) => {
        if (!cancelled) setList(rows as ListItem[]);
      })
      .catch((e) => {
        if (!cancelled) setError(String(e));
      })
      .finally(() => {
        if (!cancelled) setListLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [directory, hostAlias, query, searching]);

  // セッションを切り替えたタイミングでは、前セッションのメッセージを即時に消す
  // （別セッションの中身が一瞬残るのは混乱の元）。一方、同じセッション内で検索語だけが
  // 変わった場合は前回データを残し、検索バー右端のスピナーで進行を示す（点滅回避）。
  const selectedSid = selected?.session_id ?? null;
  useEffect(() => {
    setMessages([]);
  }, [selectedSid]);

  // 選択中セッションの中身（検索語があればヒットしたもののみ）。
  useEffect(() => {
    if (!selected) {
      setMessages([]);
      return;
    }
    let cancelled = false;
    setDetailLoading(true);
    invoke<MessageRow[]>("archive_session_messages", {
      sessionId: selected.session_id,
      query: searching ? query : null,
    })
      .then((rows) => {
        if (!cancelled) setMessages(rows);
      })
      .catch((e) => {
        if (!cancelled) setError(String(e));
      })
      .finally(() => {
        if (!cancelled) setDetailLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [selected, query, searching]);

  // ターン単位に束ね、新しいターンを上に並べる（ターン内は時系列のまま）。
  // part への展開・トグル絞り込み・省略は各 TurnView に委ねる。
  const displayTurns = useMemo(() => buildTurns(messages).reverse(), [messages]);

  // 「初回ロード」= まだ表示できるデータが無い状態。これだけは中央に大きく「読み込み中…」を出す。
  // 「再フェッチ」= すでに前回データが見えている状態。点滅を避けるため前回データを残し、
  // 検索バー右端のスピナーだけで進行を示す（ユーザーが検索操作中のときのみ）。
  const listIsInitial = listLoading && list.length === 0;
  const detailIsInitial = detailLoading && messages.length === 0;
  const showInlineSpinner =
    rawQuery !== "" && ((listLoading && list.length > 0) || (detailLoading && messages.length > 0));

  return (
    <div className="archive-panel">
      <div className="archive-search-row">
        <input
          className="archive-search"
          type="text"
          value={rawQuery}
          placeholder="メッセージ本文を検索（ホスト + ディレクトリで絞り込み済み）"
          onChange={(e) => setRawQuery(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Escape" && rawQuery !== "") {
              e.preventDefault();
              setRawQuery("");
            }
          }}
          autoFocus
        />
        {/* 再フェッチ中は ✕ を控えめなスピナーに差し替え、UI が「動いている」ことを示す。
            初回ロード（list が空）はスピナーではなく下のローディング行で示すので出さない。 */}
        {showInlineSpinner ? (
          <span className="archive-search-spinner" aria-label="検索中" />
        ) : (
          rawQuery && (
            <button
              className="archive-search-clear"
              onClick={() => setRawQuery("")}
              title="クリア（Esc）"
            >
              ✕
            </button>
          )
        )}
      </div>

      {error && <div className="archive-error">{error}</div>}

      {selected ? (
        <div className="archive-detail">
          <div className="archive-detail-header">
            <button className="archive-back" onClick={() => setSelected(null)}>
              ← 一覧へ
            </button>
            <span className="archive-detail-title">
              {selected.summary || selected.session_id.slice(0, 8)}
            </span>
            {selected.session_id === liveSessionId && (
              <span className="badge badge-live" title="現在開いているセッション">
                ● LIVE
              </span>
            )}
            {searching && <span className="muted">ヒット {messages.length} 件</span>}
            <div className="archive-toggles">
              <label className="archive-toggle" title="Claude のツール呼び出し・ツール結果を表示">
                <input
                  type="checkbox"
                  checked={showTools}
                  onChange={(e) => setShowTools(e.target.checked)}
                />
                ツール
              </label>
              <label className="archive-toggle" title="Claude の思考（thinking）を表示">
                <input
                  type="checkbox"
                  checked={showThinking}
                  onChange={(e) => setShowThinking(e.target.checked)}
                />
                thinking
              </label>
              <label className="archive-toggle" title="フック実行サマリ等の内部メタ行を表示">
                <input
                  type="checkbox"
                  checked={showSystem}
                  onChange={(e) => setShowSystem(e.target.checked)}
                />
                System
              </label>
            </div>
          </div>
          <div className="archive-messages">
            {detailIsInitial ? (
              <div className="archive-empty">読み込み中…</div>
            ) : displayTurns.length === 0 ? (
              <div className="archive-empty">
                {searching ? "ヒットするメッセージがありません" : "メッセージがありません"}
              </div>
            ) : (
              displayTurns.map((turn) => (
                <TurnView
                  key={turn[0].id}
                  turn={turn}
                  query={searching ? query : null}
                  searching={searching}
                  filters={filters}
                />
              ))
            )}
          </div>
        </div>
      ) : (
        <div className="archive-list">
          {listIsInitial ? (
            <div className="archive-empty">読み込み中…</div>
          ) : list.length === 0 ? (
            <div className="archive-empty">
              {searching
                ? "このディレクトリのセッションに該当なし"
                : "セッションがありません"}
            </div>
          ) : (
            list.map((s) => (
              <button
                key={s.session_id}
                className={`archive-list-item ${s.session_id === liveSessionId ? "is-live" : ""}`}
                onClick={() => setSelected(s)}
              >
                <div className="item-title">
                  {s.session_id === liveSessionId && (
                    <span className="badge badge-live" title="現在開いているセッション">
                      ● LIVE
                    </span>
                  )}
                  {highlightText(
                    s.summary || s.session_id.slice(0, 8),
                    searching ? query : null
                  )}
                </div>
                <div className="item-meta">
                  {s.project && <span className="badge">{s.project}</span>}
                  {s.host_alias && <span className="badge badge-host">{s.host_alias}</span>}
                  <span className="muted">{sessionTime(s.started_at, s.ended_at)}</span>
                  <span className="muted">{s.message_count} msgs</span>
                  {/* hits バッジは検索中のときだけ意味があるので、クリア直後の旧データに残らないよう
                      searching を条件に入れる（A4）。 */}
                  {searching && s.hits != null && (
                    <span className="badge badge-hit">{s.hits} hits</span>
                  )}
                </div>
              </button>
            ))
          )}
        </div>
      )}

      <SyncStatus />
    </div>
  );
}

/**
 * 1 ターンを描画する。raw を part に展開してトグルで絞り込み、可視 part が多い場合は
 * 先頭 / 末尾だけ残して中間を省略し、半透明フローティングの「全て表示」で展開する。
 * 検索中はヒットの取りこぼしを避けるため絞り込み・省略を行わず全 part を表示する。
 */
function TurnView({
  turn,
  query,
  searching,
  filters,
}: {
  turn: MessageRow[];
  query: string | null;
  searching: boolean;
  filters: Filters;
}) {
  const parts = useMemo(() => expandTurn(turn), [turn]);
  const visible = useMemo(
    () =>
      searching
        ? parts
        : parts.filter((dp) => isVisibleCategory(partCategory(dp.part), filters)),
    [parts, searching, filters]
  );
  const [expanded, setExpanded] = useState(false);

  if (visible.length === 0) return null;

  const canCollapse = !searching && visible.length > TURN_HEAD + TURN_TAIL + 1;

  if (!canCollapse) {
    return (
      <div className="turn">
        {visible.map((dp) => (
          <PartItem key={dp.key} dp={dp} query={query} />
        ))}
      </div>
    );
  }

  if (expanded) {
    return (
      <div className="turn">
        {visible.map((dp) => (
          <PartItem key={dp.key} dp={dp} query={query} />
        ))}
        <button className="turn-collapse-btn" onClick={() => setExpanded(false)}>
          ターンを折りたたむ
        </button>
      </div>
    );
  }

  const head = visible.slice(0, TURN_HEAD);
  const tail = visible.slice(visible.length - TURN_TAIL);
  const hidden = visible.length - TURN_HEAD - TURN_TAIL;
  return (
    <div className="turn">
      {head.map((dp) => (
        <PartItem key={dp.key} dp={dp} query={query} />
      ))}
      <div className="turn-collapsed">
        <button className="turn-show-all" onClick={() => setExpanded(true)}>
          ⋯ 全て表示（残り {hidden} 件）
        </button>
      </div>
      {tail.map((dp) => (
        <PartItem key={dp.key} dp={dp} query={query} />
      ))}
    </div>
  );
}

/** バブルの外枠＋メタ行（ロール / タグ / ツール名 / subagent / 時刻）。 */
function Bubble({
  className,
  label,
  tag,
  tool,
  isSidechain,
  ts,
  children,
}: {
  className: string;
  label: string;
  tag?: string;
  tool?: string;
  isSidechain?: boolean;
  ts?: number | null;
  children: ReactNode;
}) {
  return (
    <div className={`msg ${className}`}>
      <div className="msg-meta">
        <span className="msg-role">{label}</span>
        {tag && <span className="msg-tag">{tag}</span>}
        {tool && <span className="msg-tool">🔧 {tool}</span>}
        {isSidechain && <span className="msg-tag">subagent</span>}
        <span className="msg-time">{formatTimestamp(ts)}</span>
      </div>
      {children}
    </div>
  );
}

/** AskUserQuestion（質問と選択肢）を描画する。 */
function AskBlock({ questions }: { questions: AskQuestion[] }) {
  if (questions.length === 0) return <div className="msg-body muted">(質問内容なし)</div>;
  return (
    <div className="ask-block">
      {questions.map((q, i) => (
        <div className="ask-q" key={i}>
          <div className="ask-q-head">
            {q.header && <span className="ask-header">{q.header}</span>}
            {q.multiSelect && <span className="ask-multi">複数選択</span>}
          </div>
          <div className="ask-question">{q.question}</div>
          {q.options?.length > 0 && (
            <ul className="ask-options">
              {q.options.map((o, j) => (
                <li key={j}>
                  <span className="ask-opt-label">{o.label}</span>
                  {o.description && <span className="ask-opt-desc">{o.description}</span>}
                </li>
              ))}
            </ul>
          )}
        </div>
      ))}
    </div>
  );
}

/** 1 つの part を種別ごとに描画する。 */
function PartItem({ dp, query }: { dp: DisplayPart; query: string | null }) {
  const { part, ts, isSidechain } = dp;

  switch (part.kind) {
    case "user":
      return (
        <Bubble className="msg-user" label="User" ts={ts} isSidechain={isSidechain}>
          {part.text && <MessageBody text={part.text} query={query} />}
        </Bubble>
      );
    case "text":
      return (
        <Bubble className="msg-assistant" label="Claude" ts={ts} isSidechain={isSidechain}>
          <MessageBody text={part.text} query={query} />
        </Bubble>
      );
    case "thinking":
      return (
        <Bubble
          className="msg-assistant msg-thinking"
          label="Claude"
          tag="thinking"
          ts={ts}
          isSidechain={isSidechain}
        >
          {part.text ? (
            <MessageBody text={part.text} query={query} />
          ) : (
            <div className="msg-body muted">(thinking — 本文は記録されていません)</div>
          )}
        </Bubble>
      );
    case "tool_use":
      return (
        <Bubble
          className="msg-assistant msg-tool-use"
          label="Claude"
          tool={part.name}
          ts={ts}
          isSidechain={isSidechain}
        >
          <MessageBody text={formatToolInput(part.input)} query={query} mono />
        </Bubble>
      );
    case "ask":
      return (
        <Bubble className="msg-ask" label="🤝 質問（ユーザー確認）" ts={ts} isSidechain={isSidechain}>
          <AskBlock questions={part.questions} />
        </Bubble>
      );
    case "ask_answer":
      return (
        <Bubble
          className="msg-ask-answer"
          label={part.rejected ? "🙅 回答（保留/却下）" : "🙆 ユーザー回答"}
          ts={ts}
          isSidechain={isSidechain}
        >
          {part.text && <MessageBody text={part.text} query={query} />}
        </Bubble>
      );
    case "tool_result":
      return (
        <Bubble
          className={`msg-tool-result ${part.isError ? "msg-error" : ""}`}
          label="Tool"
          tag={part.isError ? "error" : undefined}
          ts={ts}
          isSidechain={isSidechain}
        >
          {part.text ? (
            <MessageBody text={part.text} query={query} mono />
          ) : (
            <div className="msg-body muted">(結果なし)</div>
          )}
        </Bubble>
      );
    case "system":
      return (
        <Bubble className="msg-system" label="System" ts={ts} isSidechain={isSidechain}>
          {part.text ? (
            <MessageBody text={part.text} query={query} />
          ) : (
            <div className="msg-body muted">{describeSystemMessage(part.raw)}</div>
          )}
        </Bubble>
      );
  }
}
