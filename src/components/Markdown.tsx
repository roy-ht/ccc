import { useEffect, useState } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { openUrl } from "@tauri-apps/plugin-opener";

/**
 * OS ブラウザに渡して安全なスキームか判定する（http/https/mailto のみ許可）。
 * react-markdown は `javascript:`/`data:`/`file:` 等をサニタイズしないため、
 * アーカイブ本文（他ホスト由来を含む半信頼入力）のリンクをここで検査する。
 */
function isSafeHref(href: string): boolean {
  try {
    const proto = new URL(href).protocol;
    return proto === "http:" || proto === "https:" || proto === "mailto:";
  } catch {
    return false; // 相対URL・不正URL・javascript: 等
  }
}

/** resolver の戻り値。null は「解決不能（プレースホルダにフォールバック）」を意味する。 */
export type ResolvedImage = { mime: string; base64: string } | null;

/** 同一 src の多重 invoke を避けるためのモジュールスコープキャッシュ。 */
const imageCache = new Map<string, ResolvedImage>();

interface MdImageProps {
  alt?: string;
  src?: string;
  resolveImage: (src: string) => Promise<ResolvedImage>;
}

/**
 * resolver 経由でローカル画像を data URL に解決して `<img>` 表示する。
 * 解決中・解決失敗時は既存と同じプレースホルダにフォールバック。
 */
function MdImage({ alt, src, resolveImage }: MdImageProps) {
  const [resolved, setResolved] = useState<ResolvedImage>(() =>
    src && imageCache.has(src) ? imageCache.get(src)! : null,
  );
  const [done, setDone] = useState<boolean>(() => !!src && imageCache.has(src));

  useEffect(() => {
    if (!src) {
      setDone(true);
      return;
    }
    if (imageCache.has(src)) {
      setResolved(imageCache.get(src)!);
      setDone(true);
      return;
    }
    let cancelled = false;
    setDone(false);
    resolveImage(src)
      .then((r) => {
        imageCache.set(src, r);
        if (!cancelled) {
          setResolved(r);
          setDone(true);
        }
      })
      .catch(() => {
        imageCache.set(src, null);
        if (!cancelled) {
          setResolved(null);
          setDone(true);
        }
      });
    return () => {
      cancelled = true;
    };
  }, [src, resolveImage]);

  if (done && resolved) {
    return (
      <img
        className="md-img-inline"
        src={`data:${resolved.mime};base64,${resolved.base64}`}
        alt={alt ?? ""}
        title={src}
      />
    );
  }
  return (
    <span className="md-img" title={src}>
      🖼 {alt || "image"}
    </span>
  );
}

interface Props {
  text: string;
  /**
   * ローカル画像の解決関数。指定された場合のみ `![](path)` をインライン `<img>` として描画する。
   * 未指定（アーカイブ本文など）では従来通りプレースホルダのみ。
   * `null` を返した src はプレースホルダにフォールバック（外部 URL / 解決不能 / 範囲外など）。
   */
  resolveImage?: (src: string) => Promise<ResolvedImage>;
}

/**
 * メッセージ本文を Markdown（GFM）として描画する。
 *
 * react-markdown は既定で生 HTML を描画しない（XSS 安全）。リンクは webview を
 * 遷移させず OS のブラウザで開き、画像は既定で alt テキストのみ表示してリモート取得を避ける。
 * `resolveImage` を渡すコンテキスト（explorer の md preview 等）に限り、ローカル画像を
 * data URL でインライン描画する。
 * 検索ハイライトとは併用しない（呼び出し側が検索中はプレーン描画に出し分ける）。
 */
export function Markdown({ text, resolveImage }: Props) {
  return (
    <div className="md">
      <ReactMarkdown
        remarkPlugins={[remarkGfm]}
        components={{
          a: ({ href, children }) => {
            const safe = typeof href === "string" && isSafeHref(href);
            return (
              <a
                className="md-link"
                href={safe ? href : undefined}
                title={href}
                onClick={(e) => {
                  e.preventDefault();
                  if (safe && href) openUrl(href).catch(() => {});
                }}
              >
                {children}
              </a>
            );
          },
          img: ({ alt, src }) =>
            resolveImage ? (
              <MdImage
                alt={typeof alt === "string" ? alt : undefined}
                src={typeof src === "string" ? src : undefined}
                resolveImage={resolveImage}
              />
            ) : (
              <span className="md-img" title={typeof src === "string" ? src : undefined}>
                🖼 {alt || "image"}
              </span>
            ),
        }}
      >
        {text}
      </ReactMarkdown>
    </div>
  );
}
