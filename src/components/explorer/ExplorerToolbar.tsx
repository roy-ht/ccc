import { useEffect, useState } from "react";

interface Props {
  cwd: string;
  homeDir: string;
  onGoTo: (path: string) => void;
  onRefresh: () => void;
}

/**
 * ツリー上部のツールバー。ルートディレクトリを表示・編集できるパスバー、
 * 「親へ」「ホーム（インスタンス CWD へ）」「更新」のボタンを並べる。
 */
export function ExplorerToolbar({ cwd, homeDir, onGoTo, onRefresh }: Props) {
  const [draft, setDraft] = useState(cwd);
  useEffect(() => setDraft(cwd), [cwd]);

  const apply = () => {
    const trimmed = draft.trim();
    if (!trimmed || trimmed === cwd) return;
    onGoTo(trimmed);
  };

  const goUp = () => {
    const trimmed = cwd.replace(/\/+$/, "");
    if (!trimmed || trimmed === "/") return;
    const idx = trimmed.lastIndexOf("/");
    const parent = idx <= 0 ? "/" : trimmed.slice(0, idx);
    onGoTo(parent);
  };

  const goHome = () => {
    if (homeDir && homeDir !== cwd) onGoTo(homeDir);
  };

  const isHome = cwd === homeDir;

  return (
    <div className="explorer-toolbar">
      <button
        className="explorer-toolbar-btn"
        onClick={goHome}
        disabled={isHome}
        title={`インスタンス起動ディレクトリへ戻る (${homeDir})`}
      >
        ⌂
      </button>
      <button
        className="explorer-toolbar-btn"
        onClick={goUp}
        title="親ディレクトリへ"
      >
        ↑
      </button>
      <button
        className="explorer-toolbar-btn"
        onClick={onRefresh}
        title="ツリーを再読み込み"
      >
        ⟳
      </button>
      <input
        className="explorer-toolbar-path"
        value={draft}
        onChange={(e) => setDraft(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter") apply();
          else if (e.key === "Escape") setDraft(cwd);
        }}
        spellCheck={false}
        title="絶対パスを入力して Enter で移動"
      />
      <button
        className="explorer-toolbar-btn explorer-toolbar-go"
        onClick={apply}
        disabled={draft.trim() === cwd}
        title="入力したディレクトリへ移動"
      >
        移動
      </button>
    </div>
  );
}
