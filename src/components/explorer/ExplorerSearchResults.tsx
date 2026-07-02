import { useMemo } from "react";
import type { SearchHit } from "../../types";

interface Props {
  query: string;
  results: SearchHit[];
  loading: boolean;
  error: string | null;
  onPick: (path: string, line: number) => void;
}

/**
 * 検索結果一覧。ファイル単位でグルーピングし、ヒットした行をクリックすると
 * 親が PreviewPane に飛ばす（行ハイライト付き）。
 */
export function ExplorerSearchResults({ query, results, loading, error, onPick }: Props) {
  const grouped = useMemo(() => groupByPath(results), [results]);

  return (
    <div className="explorer-search-results">
      {loading && <div className="explorer-empty">検索中…</div>}
      {error && <div className="explorer-error">{error}</div>}
      {!loading && !error && results.length === 0 && (
        <div className="explorer-empty">「{query}」にヒットするファイルがありません</div>
      )}
      {grouped.map((g) => (
        <div key={g.path} className="explorer-search-group">
          <div className="explorer-search-group-path" title={g.path}>
            {g.path}
            <span className="explorer-search-group-count">({g.hits.length})</span>
          </div>
          {g.hits.map((h, i) => (
            <button
              key={`${g.path}:${h.line_number}:${i}`}
              className="explorer-search-hit"
              onClick={() => onPick(g.path, h.line_number)}
              title={`${g.path}:${h.line_number}`}
            >
              <span className="explorer-search-line-no">{h.line_number}</span>
              <span className="explorer-search-line-text">
                {renderHitLine(h.line, h.match_start, h.match_end)}
              </span>
            </button>
          ))}
        </div>
      ))}
    </div>
  );
}

function groupByPath(hits: SearchHit[]): { path: string; hits: SearchHit[] }[] {
  const map = new Map<string, SearchHit[]>();
  for (const h of hits) {
    const arr = map.get(h.path) ?? [];
    arr.push(h);
    map.set(h.path, arr);
  }
  return Array.from(map.entries()).map(([path, hits]) => ({ path, hits }));
}

/**
 * 行の中でマッチ部分を `<mark>` で強調する。
 * match_start/end は rg --json の submatch（UTF-8 バイトオフセット）だが、
 * UI 用に大まかに切り出すため、`String#slice` の文字オフセットでも動く範囲で扱う。
 * （ascii 行では一致、マルチバイト行ではズレうるが見栄えの問題に留まる。）
 */
function renderHitLine(line: string, start: number, end: number) {
  if (end <= start || start < 0 || end > line.length) {
    return <>{line}</>;
  }
  return (
    <>
      {line.slice(0, start)}
      <mark className="explorer-search-mark">{line.slice(start, end)}</mark>
      {line.slice(end)}
    </>
  );
}
