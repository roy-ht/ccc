import { useMemo } from "react";
import { ExplorerTreeNode } from "./ExplorerTreeNode";
import type { FileNode } from "../../types";

type ChildrenState =
  | { kind: "loading" }
  | { kind: "loaded"; nodes: FileNode[] }
  | { kind: "error"; message: string };

interface Props {
  childrenByPath: Record<string, ChildrenState>;
  expanded: Set<string>;
  selectedPath: string | null;
  onToggle: (path: string) => void;
  onSelectFile: (path: string) => void;
  onContextMenu?: (e: React.MouseEvent, node: FileNode) => void;
}

interface FlatRow {
  node: FileNode;
  depth: number;
}

/**
 * 展開状態に応じて子ノードを再帰的に平らに並べる。仮想スクロールは入れず素直に描画する。
 * 初期は数百行のディレクトリでも十分。仮想化は将来対応の余地として残す。
 */
function flattenTree(
  parentPath: string,
  depth: number,
  expanded: Set<string>,
  childrenByPath: Record<string, ChildrenState>
): FlatRow[] {
  const state = childrenByPath[parentPath];
  if (!state || state.kind !== "loaded") return [];
  const rows: FlatRow[] = [];
  for (const node of state.nodes) {
    rows.push({ node, depth });
    if (node.is_dir && expanded.has(node.path)) {
      rows.push(...flattenTree(node.path, depth + 1, expanded, childrenByPath));
    }
  }
  return rows;
}

export function ExplorerTree({
  childrenByPath,
  expanded,
  selectedPath,
  onToggle,
  onSelectFile,
  onContextMenu,
}: Props) {
  const rows = useMemo(
    () => flattenTree("", 0, expanded, childrenByPath),
    [expanded, childrenByPath]
  );

  // ルートが未ロードの場合のローディング/エラー表示用
  const rootState = childrenByPath[""];

  return (
    <div className="explorer-tree">
      <div className="explorer-tree-body">
        {rootState?.kind === "loading" && <div className="explorer-empty">読み込み中…</div>}
        {rootState?.kind === "error" && (
          <div className="explorer-error">{rootState.message}</div>
        )}
        {rootState?.kind === "loaded" && rows.length === 0 && (
          <div className="explorer-empty">(空のディレクトリ)</div>
        )}
        {rows.map(({ node, depth }) => {
          const childState = childrenByPath[node.path];
          const childCount =
            node.is_dir && childState?.kind === "loaded" ? childState.nodes.length : null;
          const childError =
            node.is_dir && childState?.kind === "error" ? childState.message : null;
          const childLoading = node.is_dir && childState?.kind === "loading";
          return (
            <ExplorerTreeNode
              key={node.path}
              node={node}
              depth={depth}
              isExpanded={expanded.has(node.path)}
              isSelected={selectedPath === node.path}
              isLoading={!!childLoading}
              error={childError}
              childCount={childCount}
              onToggle={onToggle}
              onSelectFile={onSelectFile}
              onContextMenu={onContextMenu}
            />
          );
        })}
      </div>
    </div>
  );
}
