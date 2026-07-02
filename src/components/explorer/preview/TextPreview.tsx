import { useEffect, useRef, useState } from "react";
import { formatBytes } from "../../../utils/filetype";

interface Props {
  content: string;
  language?: string | null;
  truncated: boolean;
  size: number;
  highlightLine?: number | null;
}

/**
 * テキストファイルのプレビュー。highlight.js を必要な言語だけ動的 import して
 * 適用する。失敗時はプレーン表示にフォールバックする。`highlightLine` が指定
 * された場合、その行をハイライトし、ビュー内にスクロールする。
 */
export function TextPreview({ content, language, truncated, size, highlightLine }: Props) {
  const [highlighted, setHighlighted] = useState<string | null>(null);
  const codeRef = useRef<HTMLElement>(null);
  const lineRef = useRef<HTMLSpanElement>(null);

  useEffect(() => {
    let cancelled = false;
    const lang = mapLanguage(language);
    if (!lang) {
      setHighlighted(null);
      return;
    }
    applyHighlight(content, lang).then((html) => {
      if (!cancelled) setHighlighted(html);
    });
    return () => {
      cancelled = true;
    };
  }, [content, language]);

  // highlightLine が指定されたら、ビューポートに収まるようスクロール。
  useEffect(() => {
    if (highlightLine && lineRef.current) {
      lineRef.current.scrollIntoView({ block: "center" });
    }
  }, [highlightLine, highlighted]);

  // 行ハイライトは行番号オーバーレイで表現する。
  const lines = content.split("\n");
  const focusIdx = highlightLine != null ? Math.max(0, highlightLine - 1) : -1;

  return (
    <div className="text-preview">
      {truncated && (
        <div className="text-preview-banner">
          先頭 {formatBytes(content.length)} を表示中（全 {formatBytes(size)}）
        </div>
      )}
      <div className="text-preview-body">
        <div className="text-preview-gutter">
          {lines.map((_, i) => (
            <span
              key={i}
              ref={i === focusIdx ? lineRef : undefined}
              className={`text-preview-lineno ${i === focusIdx ? "focus" : ""}`}
            >
              {i + 1}
            </span>
          ))}
        </div>
        <pre className="text-preview-pre">
          {highlighted ? (
            <code
              ref={codeRef}
              className={`hljs language-${mapLanguage(language) ?? "plaintext"}`}
              dangerouslySetInnerHTML={{ __html: highlighted }}
            />
          ) : (
            <code ref={codeRef} className="text-preview-plain">
              {content}
            </code>
          )}
        </pre>
      </div>
    </div>
  );
}

// ─── 言語マッピング & 動的 import ───────────────────────────────────────────

/** 拡張子（lower）→ highlight.js 言語名。未対応は null。 */
function mapLanguage(ext?: string | null): string | null {
  if (!ext) return null;
  switch (ext) {
    case "rs":
      return "rust";
    case "ts":
    case "tsx":
      return "typescript";
    case "js":
    case "jsx":
    case "mjs":
    case "cjs":
      return "javascript";
    case "py":
      return "python";
    case "json":
      return "json";
    case "yml":
    case "yaml":
      return "yaml";
    case "toml":
      return "ini"; // ini で近似
    case "sh":
    case "bash":
    case "zsh":
      return "bash";
    case "md":
    case "mdx":
    case "markdown":
      return "markdown";
    case "html":
    case "htm":
      return "xml";
    case "css":
      return "css";
    case "scss":
      return "scss";
    case "go":
      return "go";
    case "java":
      return "java";
    case "kt":
      return "kotlin";
    case "swift":
      return "swift";
    case "rb":
      return "ruby";
    case "php":
      return "php";
    case "c":
      return "c";
    case "h":
    case "hpp":
    case "cpp":
    case "cc":
    case "cxx":
      return "cpp";
    case "sql":
      return "sql";
    case "xml":
      return "xml";
    case "diff":
    case "patch":
      return "diff";
    default:
      return null;
  }
}

let hljsCorePromise: Promise<typeof import("highlight.js/lib/core")> | null = null;
const langLoaded = new Map<string, Promise<unknown>>();

function loadCore() {
  if (!hljsCorePromise) {
    hljsCorePromise = import("highlight.js/lib/core");
  }
  return hljsCorePromise;
}

/** 必要な言語のみ動的 register（バンドル肥大化を避けるため）。 */
function loadLanguage(name: string): Promise<unknown> {
  let p = langLoaded.get(name);
  if (p) return p;
  p = (async () => {
    const core = await loadCore();
    const mod: any = await dynamicImportLanguage(name);
    const def = mod?.default ?? mod;
    core.default.registerLanguage(name, def);
  })().catch(() => {
    langLoaded.delete(name);
  });
  langLoaded.set(name, p);
  return p;
}

function dynamicImportLanguage(name: string): Promise<any> {
  // Vite は動的 import の文字列を静的解析するため、列挙して switch する必要がある。
  switch (name) {
    case "rust":
      return import("highlight.js/lib/languages/rust");
    case "typescript":
      return import("highlight.js/lib/languages/typescript");
    case "javascript":
      return import("highlight.js/lib/languages/javascript");
    case "python":
      return import("highlight.js/lib/languages/python");
    case "json":
      return import("highlight.js/lib/languages/json");
    case "yaml":
      return import("highlight.js/lib/languages/yaml");
    case "ini":
      return import("highlight.js/lib/languages/ini");
    case "bash":
      return import("highlight.js/lib/languages/bash");
    case "markdown":
      return import("highlight.js/lib/languages/markdown");
    case "xml":
      return import("highlight.js/lib/languages/xml");
    case "css":
      return import("highlight.js/lib/languages/css");
    case "scss":
      return import("highlight.js/lib/languages/scss");
    case "go":
      return import("highlight.js/lib/languages/go");
    case "java":
      return import("highlight.js/lib/languages/java");
    case "kotlin":
      return import("highlight.js/lib/languages/kotlin");
    case "swift":
      return import("highlight.js/lib/languages/swift");
    case "ruby":
      return import("highlight.js/lib/languages/ruby");
    case "php":
      return import("highlight.js/lib/languages/php");
    case "c":
      return import("highlight.js/lib/languages/c");
    case "cpp":
      return import("highlight.js/lib/languages/cpp");
    case "sql":
      return import("highlight.js/lib/languages/sql");
    case "diff":
      return import("highlight.js/lib/languages/diff");
    default:
      return Promise.reject(new Error(`unknown language: ${name}`));
  }
}

async function applyHighlight(content: string, lang: string): Promise<string | null> {
  try {
    await loadLanguage(lang);
    const core = await loadCore();
    const result = core.default.highlight(content, { language: lang, ignoreIllegals: true });
    return result.value;
  } catch {
    return null;
  }
}
