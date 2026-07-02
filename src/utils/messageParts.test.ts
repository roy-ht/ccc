import { describe, it, expect } from "vitest";
import { expandTurn, partCategory } from "./messageParts";
import { MessageRow } from "../types";

function row(partial: Partial<MessageRow> & { id: number }): MessageRow {
  return {
    seq: partial.id,
    ts: 1000 + partial.id,
    role: null,
    msg_type: null,
    tool_name: null,
    is_sidechain: false,
    agent_id: null,
    text: null,
    raw: null,
    ...partial,
  };
}

function assistant(id: number, content: unknown, text: string | null = null): MessageRow {
  return row({
    id,
    role: "assistant",
    text,
    raw: JSON.stringify({ type: "assistant", message: { content } }),
  });
}

describe("expandTurn", () => {
  it("assistant の thinking / text / tool_use を別 part に割る", () => {
    const m = assistant(
      1,
      [
        { type: "thinking", thinking: "考え中" },
        { type: "text", text: "やります" },
        { type: "tool_use", name: "Edit", input: { file_path: "a.rs" } },
      ],
      "やります"
    );
    const parts = expandTurn([m]).map((d) => d.part);
    expect(parts).toEqual([
      { kind: "thinking", text: "考え中" },
      { kind: "text", text: "やります" },
      { kind: "tool_use", name: "Edit", input: { file_path: "a.rs" } },
    ]);
  });

  it("AskUserQuestion は ask part になる", () => {
    const m = assistant(2, [
      {
        type: "tool_use",
        id: "toolu_1",
        name: "AskUserQuestion",
        input: { questions: [{ question: "Q", header: "H", options: [{ label: "A" }] }] },
      },
    ]);
    const parts = expandTurn([m]).map((d) => d.part);
    expect(parts).toEqual([
      { kind: "ask", questions: [{ question: "Q", header: "H", options: [{ label: "A" }] }] },
    ]);
  });

  it("AskUserQuestion への tool_result は ask_answer に分類する", () => {
    const ask = assistant(3, [
      { type: "tool_use", id: "toolu_x", name: "AskUserQuestion", input: { questions: [] } },
    ]);
    const answer = row({
      id: 4,
      role: "user",
      msg_type: "tool_result",
      text: "Aを選びました",
      raw: JSON.stringify({
        type: "user",
        message: { content: [{ type: "tool_result", tool_use_id: "toolu_x", content: "Aを選びました" }] },
      }),
    });
    const parts = expandTurn([ask, answer]).map((d) => d.part);
    expect(parts[1]).toEqual({ kind: "ask_answer", text: "Aを選びました", rejected: false });
  });

  it("AskUserQuestion 以外の tool_result は tool_result のまま", () => {
    const answer = row({
      id: 5,
      role: "user",
      msg_type: "tool_result",
      text: "出力",
      raw: JSON.stringify({
        type: "user",
        message: { content: [{ type: "tool_result", tool_use_id: "toolu_other", content: "出力", is_error: false }] },
      }),
    });
    expect(expandTurn([answer]).map((d) => d.part)).toEqual([
      { kind: "tool_result", text: "出力", isError: false },
    ]);
  });

  it("ユーザー入力と system 行をそれぞれ分類する", () => {
    const user = row({ id: 6, role: "user", msg_type: "text", text: "お願い" });
    const sys = row({ id: 7, role: "system", text: null, raw: '{"type":"system","subtype":"turn_duration"}' });
    const parts = expandTurn([user, sys]).map((d) => d.part);
    expect(parts[0]).toEqual({ kind: "user", text: "お願い" });
    expect(parts[1]).toEqual({ kind: "system", text: null, raw: '{"type":"system","subtype":"turn_duration"}' });
  });
});

describe("partCategory", () => {
  it("トグル対象のカテゴリを返す", () => {
    expect(partCategory({ kind: "thinking", text: "" })).toBe("thinking");
    expect(partCategory({ kind: "tool_use", name: "x", input: {} })).toBe("tool");
    expect(partCategory({ kind: "tool_result", text: "", isError: false })).toBe("tool");
    expect(partCategory({ kind: "system", text: null, raw: null })).toBe("system");
    expect(partCategory({ kind: "user", text: "" })).toBe("main");
    expect(partCategory({ kind: "ask", questions: [] })).toBe("main");
  });
});
