import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Markdown, type ResolvedImage } from "../../Markdown";
import { TextPreview } from "./TextPreview";
import { formatBytes } from "../../../utils/filetype";
import type { Preview } from "../../../types";

interface Props {
  content: string;
  truncated: boolean;
  size: number;
  /** Markdown ファイルの絶対パス解決に使う explorer ルート / インスタンス情報。 */
  instanceId: string;
  rootDir: string;
  /** explorer ルートからの POSIX 相対パス（このファイル自身の場所）。 */
  mdPath: string;
}

/**
 * POSIX 相対パスを正規化する（`.` を捨て、`..` で 1 段上げる）。
 * ルートを越える `..` は除去のみ行い、最終的なルート外参照は Rust 側 `path_guard` で弾かれる。
 */
function normalizePosix(path: string): string {
  const segs: string[] = [];
  for (const seg of path.split("/")) {
    if (seg === "" || seg === ".") continue;
    if (seg === "..") {
      if (segs.length > 0) segs.pop();
      continue;
    }
    segs.push(seg);
  }
  return segs.join("/");
}

/** Markdown 内の画像 src を explorer ルートからの POSIX 相対パスに解決する。 */
function resolveMdImagePath(src: string, mdPath: string): string {
  let decoded: string;
  try {
    decoded = decodeURI(src);
  } catch {
    decoded = src;
  }
  if (decoded.startsWith("/")) {
    return normalizePosix(decoded);
  }
  const slash = mdPath.lastIndexOf("/");
  const baseDir = slash >= 0 ? mdPath.slice(0, slash) : "";
  return normalizePosix(baseDir ? `${baseDir}/${decoded}` : decoded);
}

/**
 * Markdown のプレビュー / ソース切替。既存 `Markdown` を流用しつつ、
 * Source モード時は `TextPreview`（hljs markdown 言語）で同じ装飾を見せる。
 * 表示モードはコンポーネント内ローカル state。ファイルが変わるたびに preview に戻す。
 *
 * Preview モードでは `resolveImage` を渡し、相対パスのローカル画像を
 * `explorer_get_preview` 経由で取得して `<img>` インライン描画する。
 */
export function MarkdownPreview({
  content,
  truncated,
  size,
  instanceId,
  rootDir,
  mdPath,
}: Props) {
  const [mode, setMode] = useState<"preview" | "source">("preview");
  useEffect(() => {
    setMode("preview");
  }, [content]);

  const resolveImage = useCallback(
    async (src: string): Promise<ResolvedImage> => {
      // 絶対 URL（http/https/data 等）は対象外。alt プレースホルダで表示する。
      try {
        // URL parse に成功＝スキーム付き → 解決しない。
        new URL(src);
        return null;
      } catch {
        // 相対パス。続行。
      }
      const relPath = resolveMdImagePath(src, mdPath);
      if (!relPath) return null;
      try {
        const p = await invoke<Preview>("explorer_get_preview", {
          instanceId,
          root: rootDir,
          path: relPath,
        });
        if (p.kind === "image") return { mime: p.mime, base64: p.base64 };
        return null;
      } catch {
        return null;
      }
    },
    [instanceId, rootDir, mdPath],
  );

  return (
    <div className="md-preview">
      <div className="md-toolbar">
        <button
          className={`md-toolbar-btn ${mode === "preview" ? "active" : ""}`}
          onClick={() => setMode("preview")}
        >
          Preview
        </button>
        <button
          className={`md-toolbar-btn ${mode === "source" ? "active" : ""}`}
          onClick={() => setMode("source")}
        >
          Source
        </button>
        {truncated && (
          <span className="md-toolbar-info">
            先頭 {formatBytes(content.length)} を表示中（全 {formatBytes(size)}）
          </span>
        )}
      </div>
      <div className="md-body">
        {mode === "preview" ? (
          <div className="md-preview-render">
            <Markdown text={content} resolveImage={resolveImage} />
          </div>
        ) : (
          <TextPreview
            content={content}
            language="md"
            truncated={false}
            size={content.length}
          />
        )}
      </div>
    </div>
  );
}
