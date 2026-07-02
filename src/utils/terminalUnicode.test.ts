import { describe, it, expect } from "vitest";
import { Terminal } from "@xterm/xterm";
import { Unicode11Addon } from "@xterm/addon-unicode11";

/**
 * ターミナルの文字幅設定のリグレッションテスト。
 *
 * セル幅は claude code / tmux / vt100（シャドウスクリーン）の計算と一致して
 * いなければならない（UAX #11 準拠 = Unicode 11 テーブル）。レンダラ単独で
 * 幅を変えると、カーソル制御が密な claude の入力欄が崩れる（実績あり）。
 * 曖昧幅文字（①等）は幅1のまま `rescaleOverlappingGlyphs` で縮小描画する。
 */
function makeWcwidth(): (cp: number) => number {
  const term = new Terminal({ allowProposedApi: true });
  term.loadAddon(new Unicode11Addon());
  term.unicode.activeVersion = "11";
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const svc = (term as any)._core.unicodeService;
  return (cp: number) => svc.wcwidth(cp);
}

describe("ターミナルの文字幅（Unicode 11）", () => {
  const wcwidth = makeWcwidth();

  it("絵文字は2セル（tmux / claude の計算と一致）", () => {
    for (const ch of ["⭐", "📁", "🌿", "✅"]) {
      expect(wcwidth(ch.codePointAt(0)!), ch).toBe(2);
    }
  });

  it("異体字セレクタ（FE0F）は0セル", () => {
    expect(wcwidth(0xfe0f)).toBe(0);
  });

  it("曖昧幅文字は1セルのまま（縮小描画で対応、幅は変えない）", () => {
    for (const ch of ["①", "②", "③", "Ⅲ", "※"]) {
      expect(wcwidth(ch.codePointAt(0)!), ch).toBe(1);
    }
  });

  it("CJK は2セル・ASCII / TUI 構造文字は1セル", () => {
    expect(wcwidth("日".codePointAt(0)!)).toBe(2);
    expect(wcwidth("あ".codePointAt(0)!)).toBe(2);
    for (const ch of ["A", "─", "│", "❯", "⏺"]) {
      expect(wcwidth(ch.codePointAt(0)!), ch).toBe(1);
    }
  });
});
