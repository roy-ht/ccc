import { useEffect, useMemo, useRef, useState } from "react";
import { formatBytes } from "../../../utils/filetype";
import { formatJsonText, looksLikeJson, splitHighlightedLines } from "../../../utils/textPreview";

interface Props {
  content: string;
  language?: string | null;
  truncated: boolean;
  size: number;
  highlightLine?: number | null;
}

const WRAP_STORAGE_KEY = "ccc.explorer-preview-wrap";

function loadWrapPref(): boolean {
  try {
    return localStorage.getItem(WRAP_STORAGE_KEY) === "1";
  } catch {
    // localStorage 不可なら折り返しなし
    return false;
  }
}

/** JSON 整形の結果。`source` は整形元の content（ファイル切替で無効化するため）。 */
interface JsonView {
  source: string;
  text: string | null;
  error: string | null;
}

/**
 * テキストファイルのプレビュー。highlight.js を必要な言語だけ動的 import して
 * 適用する。失敗時はプレーン表示にフォールバックする。`highlightLine` が指定
 * された場合、その行をハイライトし、ビュー内にスクロールする。
 *
 * ツールバーで以下を切り替えられる:
 * - 折り返し: 長い行を折り返す（設定は localStorage に永続化）
 * - JSON 整形: JSON / JSON Lines をインデント付きに整形して表示
 */
export function TextPreview({ content, language, truncated, size, highlightLine }: Props) {
  const [highlighted, setHighlighted] = useState<string | null>(null);
  const [wrap, setWrap] = useState<boolean>(loadWrapPref);
  const [jsonView, setJsonView] = useState<JsonView | null>(null);
  const lineRef = useRef<HTMLDivElement>(null);

  // ファイルが切り替わったら（content が変われば）整形状態は自動的に無効化する。
  const activeJson = jsonView && jsonView.source === content ? jsonView : null;
  const formatted = activeJson?.text ?? null;
  const displayContent = formatted ?? content;
  const jsonCapable = useMemo(() => looksLikeJson(language, content), [language, content]);

  useEffect(() => {
    try {
      localStorage.setItem(WRAP_STORAGE_KEY, wrap ? "1" : "0");
    } catch {
      // 保存失敗は無視
    }
  }, [wrap]);

  useEffect(() => {
    let cancelled = false;
    const lang = mapLanguage(language);
    // 前のファイル / 整形前の結果が残ると行がズレるので、いったん捨てる。
    setHighlighted(null);
    if (!lang) return;
    applyHighlight(displayContent, lang).then((html) => {
      if (!cancelled) setHighlighted(html);
    });
    return () => {
      cancelled = true;
    };
  }, [displayContent, language]);

  // highlightLine が指定されたら、ビューポートに収まるようスクロール。
  useEffect(() => {
    if (highlightLine && lineRef.current) {
      lineRef.current.scrollIntoView({ block: "center" });
    }
  }, [highlightLine, highlighted, wrap]);

  const lines = useMemo(() => displayContent.split("\n"), [displayContent]);
  // 行をまたぐ span を閉じ直して行単位に分割する。折り返し時も行番号と本文が
  // 1 行ずつ対応するので、行の高さが伸びてもズレない。
  const htmlLines = useMemo(
    () => (highlighted ? splitHighlightedLines(highlighted) : null),
    [highlighted],
  );
  // ハイライト計算は非同期なので、行数が合わない間はプレーン表示にフォールバック。
  const renderedHtml = htmlLines && htmlLines.length === lines.length ? htmlLines : null;

  // 整形表示中は元ファイルの行番号と対応しないため、行ハイライトは無効化する。
  const focusIdx = highlightLine != null && !formatted ? Math.max(0, highlightLine - 1) : -1;

  const toggleJson = () => {
    if (activeJson) {
      setJsonView(null);
      return;
    }
    const result = formatJsonText(content);
    setJsonView(
      result.ok
        ? { source: content, text: result.text, error: null }
        : { source: content, text: null, error: result.error },
    );
  };

  return (
    <div className="text-preview">
      <div className="text-preview-toolbar">
        <button
          className={`text-preview-btn ${wrap ? "active" : ""}`}
          onClick={() => setWrap((v) => !v)}
          title="長い行を折り返して表示する"
        >
          折り返し
        </button>
        {jsonCapable && (
          <button
            className={`text-preview-btn ${formatted ? "active" : ""}`}
            onClick={toggleJson}
            title="JSON / JSON Lines をインデント付きで整形して表示する"
          >
            JSON 整形
          </button>
        )}
      </div>
      {truncated && (
        <div className="text-preview-banner">
          先頭 {formatBytes(content.length)} を表示中（全 {formatBytes(size)}）
        </div>
      )}
      {activeJson?.error && <div className="text-preview-banner error">{activeJson.error}</div>}
      <div className={`text-preview-body ${wrap ? "wrap" : ""}`}>
        <div className="text-preview-lines">
          {lines.map((line, i) => (
            <div
              key={i}
              ref={i === focusIdx ? lineRef : undefined}
              className={`text-preview-row ${i === focusIdx ? "focus" : ""}`}
            >
              <span className="text-preview-lineno">{i + 1}</span>
              {renderedHtml ? (
                <span
                  className="text-preview-code"
                  dangerouslySetInnerHTML={{ __html: renderedHtml[i] }}
                />
              ) : (
                <span className="text-preview-code">{line}</span>
              )}
            </div>
          ))}
        </div>
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
    case "jsonl":
    case "ndjson":
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
