/** Explorer のテキストプレビュー（折り返し / JSON 整形 / 行分割）で使う純粋関数。 */

const JSON_INDENT = 2;
/** 先頭 BOM（U+FEFF）。JSON.parse は BOM を受け付けないので落とす。 */
const BOM = String.fromCharCode(0xfeff);

export type JsonFormatResult = { ok: true; text: string } | { ok: false; error: string };

/**
 * JSON らしいファイルかどうかの判定。拡張子優先で、拡張子が違っても
 * 先頭が `{` / `[` なら整形の対象にする（`.txt` に JSON を入れている例など）。
 */
export function looksLikeJson(language: string | null | undefined, content: string): boolean {
  const lang = language?.toLowerCase();
  if (lang === "json" || lang === "jsonl" || lang === "ndjson" || lang === "geojson") return true;
  const head = content.slice(0, 256).replace(BOM, "").trimStart();
  return head.startsWith("{") || head.startsWith("[");
}

/**
 * JSON テキストを 2 スペースインデントに整形する。単一 JSON として読めない
 * 場合は JSON Lines (NDJSON) として 1 行ずつ整形を試みる。
 */
export function formatJsonText(source: string): JsonFormatResult {
  const trimmed = source.replace(BOM, "").trim();
  if (!trimmed) return { ok: false, error: "内容が空のため整形できません" };
  try {
    return { ok: true, text: JSON.stringify(JSON.parse(trimmed), null, JSON_INDENT) };
  } catch {
    // 単一 JSON として読めない → JSON Lines として再挑戦
  }
  const ndjson = formatJsonLines(trimmed);
  if (ndjson !== null) return { ok: true, text: ndjson };
  return {
    ok: false,
    error: "JSON として解釈できませんでした（途中で切れているファイルの可能性があります）",
  };
}

/** 全行が JSON 値なら 1 行ずつ整形して連結する。1 行でも壊れていれば null。 */
function formatJsonLines(source: string): string | null {
  const out: string[] = [];
  for (const line of source.split("\n")) {
    const trimmed = line.trim();
    if (!trimmed) continue;
    try {
      out.push(JSON.stringify(JSON.parse(trimmed), null, JSON_INDENT));
    } catch {
      return null;
    }
  }
  return out.length > 0 ? out.join("\n") : null;
}

/**
 * highlight.js が返す HTML を行単位に分割する。行をまたぐ `<span>`（複数行
 * 文字列やコメント）は行末で閉じ、次の行頭で開き直すことで、行ごとに独立した
 * 断片にする。hljs の出力は `<span class="...">` と `</span>` しか含まない前提。
 */
export function splitHighlightedLines(html: string): string[] {
  const lines: string[] = [];
  const open: string[] = [];
  let cur = "";
  let i = 0;

  while (i < html.length) {
    const lt = html.indexOf("<", i);
    const nl = html.indexOf("\n", i);
    if (lt < 0 && nl < 0) {
      cur += html.slice(i);
      break;
    }
    const next = lt < 0 ? nl : nl < 0 ? lt : Math.min(lt, nl);
    if (next > i) {
      cur += html.slice(i, next);
      i = next;
    }
    if (html[i] === "\n") {
      cur += "</span>".repeat(open.length);
      lines.push(cur);
      cur = open.join("");
      i += 1;
      continue;
    }
    const gt = html.indexOf(">", i);
    if (gt < 0) {
      cur += html.slice(i);
      break;
    }
    const tag = html.slice(i, gt + 1);
    if (tag.startsWith("</")) open.pop();
    else open.push(tag);
    cur += tag;
    i = gt + 1;
  }

  lines.push(cur);
  return lines;
}
