// 1 メッセージ行（MessageRow）を、表示用の「part」へ展開する純ロジック。
//
// transcript の 1 行（とくに assistant 行）は thinking / text / tool_use を 1 つの
// `message.content[]` にまとめて持つため、そのままでは 1 バブルに混在してしまう。
// ここで `raw`（取り込み時に保存した元 JSON）を解析し、ブロック単位の part に割る。
// これにより「ツールだけ隠す」「thinking だけ出す」などの選択的表示や、
// AskUserQuestion の特別表示ができる。描画は SessionsPanel が担う。

import { MessageRow } from "../types";

/** AskUserQuestion の 1 問。 */
export interface AskQuestion {
  question: string;
  header?: string;
  multiSelect?: boolean;
  options: { label: string; description?: string }[];
}

/** 表示用の最小単位。 */
export type Part =
  | { kind: "user"; text: string }
  | { kind: "text"; text: string } // assistant の通常応答
  | { kind: "thinking"; text: string } // 本文は環境により空（暗号化）のことがある
  | { kind: "tool_use"; name: string; input: unknown }
  | { kind: "ask"; questions: AskQuestion[] } // AskUserQuestion（共同作業）
  | { kind: "ask_answer"; text: string; rejected: boolean } // その回答
  | { kind: "tool_result"; text: string; isError: boolean }
  | { kind: "system"; text: string | null; raw: string | null };

/** 描画に必要なメタを添えた part。 */
export interface DisplayPart {
  key: string;
  part: Part;
  ts?: number | null;
  isSidechain: boolean;
}

/** トグル制御のためのカテゴリ。`main` は常に表示する。 */
export type PartCategory = "main" | "thinking" | "tool" | "system";

export function partCategory(part: Part): PartCategory {
  switch (part.kind) {
    case "thinking":
      return "thinking";
    case "tool_use":
    case "tool_result":
      return "tool";
    case "system":
      return "system";
    default:
      // user / text / ask / ask_answer は人間との本筋なので常時表示。
      return "main";
  }
}

function parseRaw(raw: string | null | undefined): any | null {
  if (!raw) return null;
  try {
    return JSON.parse(raw);
  } catch {
    return null;
  }
}

function contentBlocks(j: any): any[] {
  const c = j?.message?.content;
  return Array.isArray(c) ? c : [];
}

/**
 * 1 ターン（MessageRow 列）を DisplayPart 列に展開する。
 * AskUserQuestion の回答（tool_result）を判別するため、ターン内の AskUserQuestion
 * tool_use id を先に集めてから走査する。
 */
export function expandTurn(turn: MessageRow[]): DisplayPart[] {
  const askIds = new Set<string>();
  for (const m of turn) {
    if (m.role !== "assistant") continue;
    for (const b of contentBlocks(parseRaw(m.raw))) {
      if (b?.type === "tool_use" && b?.name === "AskUserQuestion" && b?.id) {
        askIds.add(b.id);
      }
    }
  }

  const out: DisplayPart[] = [];
  const push = (m: MessageRow, suffix: string, part: Part) =>
    out.push({ key: `${m.id}-${suffix}`, part, ts: m.ts, isSidechain: m.is_sidechain });

  for (const m of turn) {
    const role = m.role ?? "system";
    const j = parseRaw(m.raw);

    if (role === "assistant") {
      const blocks = contentBlocks(j);
      if (blocks.length === 0) {
        const t = (m.text ?? "").trim();
        if (t) push(m, "tx", { kind: "text", text: t });
        continue;
      }
      blocks.forEach((b: any, i: number) => {
        switch (b?.type) {
          case "thinking":
            push(m, `th${i}`, { kind: "thinking", text: String(b.thinking ?? "").trim() });
            break;
          case "text": {
            const t = String(b.text ?? "").trim();
            if (t) push(m, `tx${i}`, { kind: "text", text: t });
            break;
          }
          case "tool_use":
            if (b.name === "AskUserQuestion") {
              const questions = Array.isArray(b.input?.questions) ? b.input.questions : [];
              push(m, `ask${i}`, { kind: "ask", questions });
            } else {
              push(m, `tu${i}`, { kind: "tool_use", name: b.name ?? "(tool)", input: b.input });
            }
            break;
          default: {
            // 未知ブロック type（redacted_thinking 等）。thinking 系はマーカーとして、
            // text を持つものは text として拾い、内容の取りこぼしを防ぐ。
            if (b?.type === "redacted_thinking" || typeof b?.thinking === "string") {
              push(m, `th${i}`, { kind: "thinking", text: String(b?.thinking ?? "").trim() });
            } else if (typeof b?.text === "string" && b.text.trim()) {
              push(m, `tx${i}`, { kind: "text", text: b.text.trim() });
            }
            // それ以外（本文を持たない純メタ）は無視。
          }
        }
      });
      continue;
    }

    if (role === "user") {
      if (m.msg_type === "tool_result") {
        const tr = contentBlocks(j).find((b: any) => b?.type === "tool_result");
        const id: string | undefined = tr?.tool_use_id;
        const isError = !!tr?.is_error;
        const text = (m.text ?? "").trim();
        if (id && askIds.has(id)) {
          push(m, "aa", { kind: "ask_answer", text, rejected: isError });
        } else {
          push(m, "tr", { kind: "tool_result", text, isError });
        }
      } else {
        push(m, "u", { kind: "user", text: (m.text ?? "").trim() });
      }
      continue;
    }

    push(m, "s", { kind: "system", text: m.text ?? null, raw: m.raw ?? null });
  }

  return out;
}

/** tool_use の input を表示用文字列にする（文字列はそのまま、その他は整形 JSON）。 */
export function formatToolInput(input: unknown): string {
  if (input == null) return "(no input)";
  if (typeof input === "string") return input;
  try {
    return JSON.stringify(input, null, 2);
  } catch {
    return String(input);
  }
}
