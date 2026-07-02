import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { InstanceInfo, MemoryEntry } from "../types";
import { useDebouncedValue } from "../hooks/useDebouncedValue";
import { highlightText } from "../utils/highlight";
import { Markdown } from "./Markdown";
import { SyncStatus } from "./SyncStatus";
import { formatTimestamp } from "../utils/datetime";

interface Props {
  instance: InstanceInfo;
}

/** `projects/<encoded-cwd>/...` 形式から表示用の短いラベルを作る。 */
function memoryLabel(relPath: string): string {
  if (relPath.startsWith("projects/")) {
    const parts = relPath.split("/");
    // projects/<encoded>/ 以降を表示（例: memory/fact.md, MEMORY.md）
    return parts.slice(2).join("/") || relPath;
  }
  return relPath;
}

/**
 * メモリ一覧画面。選択中インスタンスの **プロファイル＋当該プロジェクト** のメモリを
 * 新しい順に一覧する。検索バーで rel_path / 本文の部分一致で絞り込み、メモリを開くと
 * 最新版本文を表示する。検索状態なら本文中の検索語がハイライトされる。
 */
export function MemoriesPanel({ instance }: Props) {
  const directory = instance.directory ?? "";
  const agentProfile = instance.agent_profile;

  const [rawQuery, setRawQuery] = useState("");
  const query = useDebouncedValue(rawQuery.trim(), 250);
  const searching = query !== "";

  const [list, setList] = useState<MemoryEntry[]>([]);
  const [selected, setSelected] = useState<MemoryEntry | null>(null);
  const [content, setContent] = useState<string | null>(null);
  const [listLoading, setListLoading] = useState(false);
  const [detailLoading, setDetailLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // 一覧取得（検索語があれば絞り込み）。
  useEffect(() => {
    let cancelled = false;
    setListLoading(true);
    setError(null);
    invoke<MemoryEntry[]>("archive_list_memory", {
      directory,
      agentProfile,
      query: searching ? query : null,
    })
      .then((rows) => {
        if (!cancelled) setList(rows);
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
  }, [directory, agentProfile, query, searching]);

  // 選択中メモリの最新版本文。
  useEffect(() => {
    if (!selected) {
      setContent(null);
      return;
    }
    let cancelled = false;
    setDetailLoading(true);
    invoke<string | null>("archive_memory_content", {
      agentProfile: selected.agent_profile ?? agentProfile,
      relPath: selected.rel_path,
    })
      .then((c) => {
        if (!cancelled) setContent(c);
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
  }, [selected, agentProfile]);

  return (
    <div className="archive-panel">
      <div className="archive-search-row">
        <input
          className="archive-search"
          type="text"
          value={rawQuery}
          placeholder="このインスタンスのメモリを検索"
          onChange={(e) => setRawQuery(e.target.value)}
          autoFocus
        />
        {rawQuery && (
          <button className="archive-search-clear" onClick={() => setRawQuery("")} title="クリア">
            ✕
          </button>
        )}
      </div>

      {error && <div className="archive-error">{error}</div>}

      {selected ? (
        <div className="archive-detail">
          <div className="archive-detail-header">
            <button className="archive-back" onClick={() => setSelected(null)}>
              ← 一覧へ
            </button>
            <span className="archive-detail-title">{selected.rel_path}</span>
            {selected.versions > 1 && (
              <span className="muted">{selected.versions} 版</span>
            )}
          </div>
          <div className="archive-memory-content">
            {detailLoading && <div className="archive-empty">読み込み中…</div>}
            {!detailLoading && !content && (
              <div className="archive-empty">(本文なし)</div>
            )}
            {!detailLoading && content && (
              // 検索中はハイライトが隠れないようプレーン表示、それ以外は Markdown 描画。
              searching ? (
                <pre className="memory-pre">{highlightText(content, query)}</pre>
              ) : (
                <div className="msg-md">
                  <Markdown text={content} />
                </div>
              )
            )}
          </div>
        </div>
      ) : (
        <div className="archive-list">
          {listLoading && <div className="archive-empty">読み込み中…</div>}
          {!listLoading && list.length === 0 && (
            <div className="archive-empty">
              {searching ? "ヒットするメモリがありません" : "メモリがありません"}
            </div>
          )}
          {list.map((m) => (
            <button
              key={m.rel_path}
              className="archive-list-item"
              onClick={() => setSelected(m)}
            >
              <div className="item-title">
                {highlightText(memoryLabel(m.rel_path), searching ? query : null)}
              </div>
              <div className="item-meta">
                <span className={`badge badge-${m.scope ?? "user"}`}>{m.scope ?? "user"}</span>
                {m.project && <span className="badge">{m.project}</span>}
                <span className="muted">{formatTimestamp(m.captured_at)}</span>
                {m.versions > 1 && <span className="muted">{m.versions} 版</span>}
              </div>
            </button>
          ))}
        </div>
      )}

      <SyncStatus />
    </div>
  );
}
