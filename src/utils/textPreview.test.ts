import { describe, it, expect } from "vitest";
import { formatJsonText, looksLikeJson, splitHighlightedLines } from "./textPreview";

describe("looksLikeJson", () => {
  it("拡張子が json なら真", () => {
    expect(looksLikeJson("json", "not json at all")).toBe(true);
  });

  it("jsonl / ndjson も対象", () => {
    expect(looksLikeJson("jsonl", "")).toBe(true);
    expect(looksLikeJson("ndjson", "")).toBe(true);
  });

  it("拡張子が違っても中身が { / [ で始まれば真", () => {
    expect(looksLikeJson("txt", '\n  {"a": 1}')).toBe(true);
    expect(looksLikeJson(null, "[1, 2]")).toBe(true);
  });

  it("JSON に見えなければ偽", () => {
    expect(looksLikeJson("txt", "hello world")).toBe(false);
    expect(looksLikeJson(undefined, "")).toBe(false);
  });
});

describe("formatJsonText", () => {
  it("圧縮された JSON を 2 スペースで整形する", () => {
    const r = formatJsonText('{"a":1,"b":[1,2]}');
    expect(r).toEqual({
      ok: true,
      text: '{\n  "a": 1,\n  "b": [\n    1,\n    2\n  ]\n}',
    });
  });

  it("BOM 付きでも整形できる", () => {
    const r = formatJsonText(`${String.fromCharCode(0xfeff)}{"a":1}`);
    expect(r.ok).toBe(true);
  });

  it("JSON Lines は 1 行ずつ整形する", () => {
    const r = formatJsonText('{"a":1}\n{"b":2}\n');
    expect(r).toEqual({ ok: true, text: '{\n  "a": 1\n}\n{\n  "b": 2\n}' });
  });

  it("壊れた JSON は失敗を返す", () => {
    const r = formatJsonText('{"a":1');
    expect(r.ok).toBe(false);
  });

  it("空文字は失敗を返す", () => {
    expect(formatJsonText("   ").ok).toBe(false);
  });
});

describe("splitHighlightedLines", () => {
  it("装飾のないテキストを行分割する", () => {
    expect(splitHighlightedLines("a\nb\n")).toEqual(["a", "b", ""]);
  });

  it("行内で閉じる span はそのまま保つ", () => {
    expect(splitHighlightedLines('<span class="hljs-x">a</span>\nb')).toEqual([
      '<span class="hljs-x">a</span>',
      "b",
    ]);
  });

  it("行をまたぐ span は行末で閉じ、次行で開き直す", () => {
    const html = '<span class="hljs-string">"a\nb"</span>\nx';
    expect(splitHighlightedLines(html)).toEqual([
      '<span class="hljs-string">"a</span>',
      '<span class="hljs-string">b"</span>',
      "x",
    ]);
  });

  it("入れ子の span も維持する", () => {
    const html = '<span class="a"><span class="b">1\n2</span>3</span>';
    expect(splitHighlightedLines(html)).toEqual([
      '<span class="a"><span class="b">1</span></span>',
      '<span class="a"><span class="b">2</span>3</span>',
    ]);
  });

  it("行数は元テキストの行数と一致する", () => {
    const html = '<span class="hljs-comment">/*\n *\n */</span>\nend';
    expect(splitHighlightedLines(html)).toHaveLength(4);
  });
});
