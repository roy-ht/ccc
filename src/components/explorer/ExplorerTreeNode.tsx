import type { FileNode } from "../../types";

interface Props {
  node: FileNode;
  depth: number;
  isExpanded: boolean;
  isSelected: boolean;
  isLoading: boolean;
  error?: string | null;
  childCount?: number | null;
  onToggle: (path: string) => void;
  onSelectFile: (path: string) => void;
  onContextMenu?: (e: React.MouseEvent, node: FileNode) => void;
}

/**
 * ツリー 1 ノードの表示。フォルダはクリックで展開トグル、ファイルはクリックでプレビュー選択。
 * 子の表示は親 ExplorerTree が行うので、ここは行 UI のみ。
 */
export function ExplorerTreeNode({
  node,
  depth,
  isExpanded,
  isSelected,
  isLoading,
  error,
  childCount,
  onToggle,
  onSelectFile,
  onContextMenu,
}: Props) {
  const indent = depth * 14;
  const handleClick = () => {
    if (node.is_dir) onToggle(node.path);
    else onSelectFile(node.path);
  };
  const handleContextMenu = (e: React.MouseEvent) => {
    onContextMenu?.(e, node);
  };
  const chevron = node.is_dir ? (isExpanded ? "▾" : "▸") : "·";
  const icon = node.is_dir ? (isExpanded ? "📂" : "📁") : iconForFile(node.name);

  return (
    <button
      className={`explorer-tree-node ${isSelected ? "selected" : ""} ${node.hidden ? "hidden" : ""}`}
      style={{ paddingLeft: indent + 8 }}
      onClick={handleClick}
      onContextMenu={handleContextMenu}
      title={node.path}
    >
      <span className="explorer-tree-chevron">{chevron}</span>
      <span className="explorer-tree-icon">{icon}</span>
      <span className="explorer-tree-name">{node.name}</span>
      {isLoading && <span className="explorer-tree-loading">…</span>}
      {error && <span className="explorer-tree-error" title={error}>!</span>}
      {childCount != null && childCount === 0 && isExpanded && (
        <span className="explorer-tree-empty">(空)</span>
      )}
    </button>
  );
}

function iconForFile(name: string): string {
  const dot = name.lastIndexOf(".");
  if (dot < 0) return "📄";
  const ext = name.slice(dot + 1).toLowerCase();
  if (["md", "mdx", "markdown"].includes(ext)) return "📝";
  if (["png", "jpg", "jpeg", "gif", "webp", "bmp", "svg", "ico"].includes(ext)) return "🖼";
  if (ext === "pdf") return "📕";
  if (["zip", "gz", "tar", "bz2", "xz", "7z", "rar"].includes(ext)) return "📦";
  if (["mp4", "mov", "avi", "mkv"].includes(ext)) return "🎞";
  if (["mp3", "wav", "flac", "ogg"].includes(ext)) return "🎵";
  return "📄";
}
