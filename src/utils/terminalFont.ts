import { invoke } from "@tauri-apps/api/core";

/**
 * xterm に渡す fontFamily の解決。
 *
 * WKWebView (Tauri) の canvas2d はユーザーインストールフォント
 * (~/Library/Fonts) の解決が信頼できない:
 * - `"HackGen Console NF", monospace` のようなフォールバック付き指定は常に失敗し、
 *   黙って総称 monospace (Courier New / Menlo / Andale Mono 等、ウェイトや
 *   タイミングで変わる) に落ちる
 * - 単独指定 `"HackGen Console NF"` も解決できることがある一方、プロセスや
 *   オリジンによっては last-resort (Times 系) に落ちる
 * - CSS/DOM 経由のフォント解決は常に正常 (v0.7 の DOM レンダラで正しく表示
 *   できていたのはこのため)
 *
 * xterm v6 は文字幅計測 (CharSizeService) もグリフ描画 (WebGL アトラス) も
 * canvas2d で行うため、システムフォントとしての解決には頼れない。そこで
 * Rust 側 (read_font_face) からフォントファイルのバイト列を取得し、FontFace
 * (Web フォント) として document に別名で登録する。document 登録フォントは
 * canvas でも確実に解決される。
 *
 * 判定はすべて実測で行う: DOM 計測 (真実) と canvas 計測を比較し、canvas が
 * 正しい幅で等幅解決できている場合のみ FontFace 注入を省略する。
 */

const GENERIC_FAMILIES = new Set([
  "monospace",
  "sans-serif",
  "serif",
  "system-ui",
  "ui-monospace",
  "ui-sans-serif",
  "ui-serif",
  "cursive",
  "fantasy",
]);

/** FontFace 登録時の別名サフィックス。システムフォント名との衝突を避ける。 */
const FACE_ALIAS_SUFFIX = " ccc-terminal";

/** 等幅判定用の計測結果。narrow='i'、wide='W' の advance 幅。 */
export interface MonoMetrics {
  narrow: number;
  wide: number;
}

export interface FontResolveDeps {
  /** CSS/DOM 経由の計測（常に正しい解決を返す基準値）。 */
  measureDom: (family: string) => MonoMetrics | null;
  /** canvas2d 経由の計測（検証対象）。 */
  measureCanvas: (family: string) => MonoMetrics | null;
  /** family のフォントバイト列を取得し alias で FontFace 登録する。 */
  registerFace: (family: string, alias: string) => Promise<void>;
}

/** フォントスタック文字列から先頭ファミリー名を取り出す（クォート除去済み）。 */
export function firstFontFamily(fontFamily: string): string | null {
  const first = fontFamily.split(",")[0]?.trim() ?? "";
  const unquoted = first.replace(/^["']/, "").replace(/["']$/, "").trim();
  return unquoted.length > 0 ? unquoted : null;
}

function isMono(m: MonoMetrics | null): m is MonoMetrics {
  return m !== null && m.narrow > 0 && m.narrow === m.wide;
}

/** canvas 計測が DOM 計測（真実）と一致しているか。 */
function matchesDom(canvas: MonoMetrics | null, dom: MonoMetrics): boolean {
  return isMono(canvas) && Math.abs(canvas.wide - dom.wide) <= dom.wide * 0.02;
}

/**
 * xterm の Terminal オプションに渡す fontFamily を決定する。
 *
 * 1. 先頭ファミリーが DOM (CSS) で等幅解決できない → 元の指定のまま
 * 2. canvas でも正しく解決できている → 先頭ファミリー単独指定
 * 3. canvas で解決できない → FontFace 注入して別名を返す
 * 4. 注入に失敗 → 元の指定のまま（総称 monospace で描画は継続できる）
 */
export async function resolveXtermFontFamily(
  fontFamily: string,
  deps: FontResolveDeps = defaultDeps,
): Promise<string> {
  const first = firstFontFamily(fontFamily);
  if (!first) return fontFamily;
  // 総称ファミリーをクォートすると「その名前のフォント」探索になるため対象外
  if (GENERIC_FAMILIES.has(first.toLowerCase())) return fontFamily;

  const dom = deps.measureDom(first);
  if (!isMono(dom)) return fontFamily;

  if (matchesDom(deps.measureCanvas(first), dom)) return `"${first}"`;

  const alias = `${first}${FACE_ALIAS_SUFFIX}`;
  try {
    await deps.registerFace(first, alias);
    if (matchesDom(deps.measureCanvas(alias), dom)) return `"${alias}"`;
    console.warn(`[ccc] FontFace 登録後も canvas で解決できません: ${first}`);
  } catch (e) {
    console.warn(`[ccc] ターミナルフォントの FontFace 登録に失敗: ${first}`, e);
  }
  return fontFamily;
}

// ─── 既定実装 ────────────────────────────────────────────────────────────────

function measureDomImpl(family: string): MonoMetrics | null {
  try {
    const span = document.createElement("span");
    span.style.cssText =
      "position:absolute;visibility:hidden;white-space:pre;font-size:64px;";
    span.style.fontFamily = `"${family}"`;
    document.body.appendChild(span);
    try {
      span.textContent = "i".repeat(16);
      const narrow = span.getBoundingClientRect().width / 16;
      span.textContent = "W".repeat(16);
      const wide = span.getBoundingClientRect().width / 16;
      return { narrow, wide };
    } finally {
      span.remove();
    }
  } catch {
    return null;
  }
}

function measureCanvasImpl(family: string): MonoMetrics | null {
  try {
    const ctx = document.createElement("canvas").getContext("2d");
    if (!ctx) return null;
    ctx.font = `64px "${family}"`;
    return {
      narrow: ctx.measureText("i").width,
      wide: ctx.measureText("W").width,
    };
  } catch {
    return null;
  }
}

/** バイト列が同一フォントファイルかの簡易判定（長さ＋先頭 64 バイト）。 */
function sameFontData(a: ArrayBuffer, b: ArrayBuffer): boolean {
  if (a.byteLength !== b.byteLength) return false;
  const va = new Uint8Array(a, 0, Math.min(64, a.byteLength));
  const vb = new Uint8Array(b, 0, Math.min(64, b.byteLength));
  return va.every((v, i) => v === vb[i]);
}

async function registerFaceImpl(family: string, alias: string): Promise<void> {
  const regular = await invoke<ArrayBuffer>("read_font_face", {
    family,
    weight: 400,
  });
  const faces = [new FontFace(alias, regular, { weight: "400" })];
  // Bold face があれば登録する。無い場合 (regular と同一ファイルが返る) は
  // 登録せず、ブラウザの synthetic bold に任せる。
  try {
    const bold = await invoke<ArrayBuffer>("read_font_face", {
      family,
      weight: 700,
    });
    if (!sameFontData(regular, bold)) {
      faces.push(new FontFace(alias, bold, { weight: "700" }));
    }
  } catch {
    // bold 無しは正常系
  }
  await Promise.all(faces.map((f) => f.load()));
  for (const f of faces) document.fonts.add(f);
}

const defaultDeps: FontResolveDeps = {
  measureDom: measureDomImpl,
  measureCanvas: measureCanvasImpl,
  registerFace: registerFaceImpl,
};

/** 同じ設定値での解決結果をセッション内で再利用する。 */
const resolveCache = new Map<string, Promise<string>>();

export function resolveXtermFontFamilyCached(fontFamily: string): Promise<string> {
  let cached = resolveCache.get(fontFamily);
  if (!cached) {
    cached = resolveXtermFontFamily(fontFamily);
    resolveCache.set(fontFamily, cached);
  }
  return cached;
}
