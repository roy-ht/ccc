import { useCallback, useRef } from "react";

interface Props {
  onStartResize: (e: React.MouseEvent, containerLeft: number) => void;
}

/**
 * 左ペインと右ペインの間に置くドラッグ可能な縦バー。ホバー / アクティブ時に
 * アクセントカラーで強調表示する。コンテナ左端 X をドラッグ開始時に算出して
 * `useExplorerSplit` へ渡す。
 */
export function ExplorerSplitter({ onStartResize }: Props) {
  const ref = useRef<HTMLDivElement>(null);

  const handleMouseDown = useCallback(
    (e: React.MouseEvent) => {
      const parent = ref.current?.parentElement;
      const left = parent?.getBoundingClientRect().left ?? 0;
      onStartResize(e, left);
    },
    [onStartResize]
  );

  return <div ref={ref} className="explorer-splitter" onMouseDown={handleMouseDown} />;
}
