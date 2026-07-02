import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { Preview } from "../../../types";
import { TextPreview } from "./TextPreview";
import { MarkdownPreview } from "./MarkdownPreview";
import { ImagePreview } from "./ImagePreview";
import { PdfPreview } from "./PdfPreview";
import { UnsupportedPreview } from "./UnsupportedPreview";

interface Props {
  instanceId: string;
  rootDir: string;
  path: string | null;
  highlightLine?: number | null;
}

type Loadable =
  | { state: "idle" }
  | { state: "loading" }
  | { state: "ok"; preview: Preview }
  | { state: "error"; message: string };

/**
 * 右ペインのプレビュー本体。`path` 変更で `explorer_get_preview` を invoke し、
 * 結果の `kind` に応じて各種ビューワへ振り分ける。
 */
export function PreviewPane({ instanceId, rootDir, path, highlightLine }: Props) {
  const [data, setData] = useState<Loadable>({ state: "idle" });

  useEffect(() => {
    if (!path) {
      setData({ state: "idle" });
      return;
    }
    let cancelled = false;
    setData({ state: "loading" });
    invoke<Preview>("explorer_get_preview", { instanceId, root: rootDir, path })
      .then((p) => {
        if (!cancelled) setData({ state: "ok", preview: p });
      })
      .catch((e) => {
        if (!cancelled) setData({ state: "error", message: String(e) });
      });
    return () => {
      cancelled = true;
    };
  }, [instanceId, rootDir, path]);

  if (data.state === "idle") {
    return (
      <div className="preview-empty">
        <p>左のツリーからファイルを選択してください</p>
      </div>
    );
  }
  if (data.state === "loading") {
    return <div className="preview-empty">読み込み中…</div>;
  }
  if (data.state === "error") {
    return <div className="preview-error">{data.message}</div>;
  }

  const p = data.preview;
  return (
    <div className="preview-pane">
      <div className="preview-header">
        <span className="preview-path" title={path ?? undefined}>
          {path}
        </span>
      </div>
      <div className="preview-content">
        {p.kind === "text" && (
          <TextPreview
            content={p.content}
            language={p.language ?? undefined}
            truncated={p.truncated}
            size={p.size}
            highlightLine={highlightLine}
          />
        )}
        {p.kind === "markdown" && (
          <MarkdownPreview
            content={p.content}
            truncated={p.truncated}
            size={p.size}
            instanceId={instanceId}
            rootDir={rootDir}
            mdPath={path ?? ""}
          />
        )}
        {p.kind === "image" && (
          <ImagePreview mime={p.mime} base64={p.base64} size={p.size} />
        )}
        {p.kind === "pdf" && <PdfPreview base64={p.base64} size={p.size} />}
        {p.kind === "binary" && (
          <UnsupportedPreview kind="binary" size={p.size} mime={p.mime} />
        )}
        {p.kind === "too_large" && (
          <UnsupportedPreview kind="too-large" size={p.size} limit={p.limit} />
        )}
      </div>
    </div>
  );
}
