interface Props {
  rawQuery: string;
  setRawQuery: (s: string) => void;
  glob: string;
  setGlob: (s: string) => void;
}

/**
 * Explorer 上部の検索バー。
 * 左: クエリ（全文検索）、右: glob パターン（対象ファイル絞り込み）。
 */
export function ExplorerSearchBar({ rawQuery, setRawQuery, glob, setGlob }: Props) {
  return (
    <div className="explorer-search-row">
      <input
        className="explorer-search"
        type="text"
        value={rawQuery}
        placeholder="ファイル内容を全文検索（ripgrep）"
        onChange={(e) => setRawQuery(e.target.value)}
      />
      {rawQuery && (
        <button
          className="explorer-search-clear"
          onClick={() => setRawQuery("")}
          title="クリア"
        >
          ✕
        </button>
      )}
      <input
        className="explorer-search-glob"
        type="text"
        value={glob}
        placeholder="glob: 例) *.ts, src/**/*.rs"
        onChange={(e) => setGlob(e.target.value)}
      />
    </div>
  );
}
