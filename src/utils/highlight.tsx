import { ReactNode } from "react";

/**
 * `query`（空白区切り）の各語を `text` 中で大文字小文字を無視してハイライトする。
 *
 * セッション本文検索は lindera 形態素トークナイザで索引されるが、ハイライトは
 * ユーザーが入力した語そのものの部分一致で行う（実用上これで十分。完全な
 * トークン境界一致までは追わない）。日本語のように空白の無い語は全体が 1 語になる。
 */
export function highlightText(text: string, query: string | null | undefined): ReactNode {
  if (!query) return text;
  // バックエンドは split_whitespace 後に各 term をフレーズ化（lindera トークンで照合）し、
  // クオート文字自体はトークンとして残らない。フロント側もユーザーが付けた前後の引用符は
  // 落としてから探さないと「ヒットはするのにハイライトが付かない」状態になる。
  const terms = query
    .split(/\s+/)
    .map((t) => t.replace(/^["']+|["']+$/g, ""))
    .filter(Boolean);
  if (terms.length === 0) return text;

  const escaped = terms.map((t) => t.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"));
  const re = new RegExp(`(${escaped.join("|")})`, "gi");
  const lower = new Set(terms.map((t) => t.toLowerCase()));

  // split に捕捉グループを渡すと、マッチ部分が配列に交互に現れる。
  return text.split(re).map((part, i) =>
    part && lower.has(part.toLowerCase()) ? (
      <mark key={i} className="hl">
        {part}
      </mark>
    ) : (
      part
    )
  );
}
