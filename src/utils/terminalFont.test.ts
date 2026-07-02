import { describe, it, expect, vi } from "vitest";
import {
  firstFontFamily,
  resolveXtermFontFamily,
  FontResolveDeps,
  MonoMetrics,
} from "./terminalFont";

const mono = (w: number): MonoMetrics => ({ narrow: w, wide: w });
const proportional: MonoMetrics = { narrow: 18, wide: 60 };

function deps(overrides: Partial<FontResolveDeps>): FontResolveDeps {
  return {
    measureDom: () => mono(30),
    measureCanvas: () => mono(30),
    registerFace: async () => {},
    ...overrides,
  };
}

describe("firstFontFamily", () => {
  it("クォート付きの先頭ファミリーを取り出す", () => {
    expect(firstFontFamily('"HackGen Console NF", monospace')).toBe("HackGen Console NF");
  });

  it("クォートなしでも取り出せる", () => {
    expect(firstFontFamily("HackGen Console NF, monospace")).toBe("HackGen Console NF");
  });

  it("シングルクォートも除去する", () => {
    expect(firstFontFamily("'Fira Code', monospace")).toBe("Fira Code");
  });

  it("空文字列は null", () => {
    expect(firstFontFamily("")).toBeNull();
    expect(firstFontFamily('""')).toBeNull();
  });
});

describe("resolveXtermFontFamily", () => {
  it("canvas が DOM と一致して解決できれば先頭ファミリー単独に変換する", async () => {
    const result = await resolveXtermFontFamily(
      '"HackGen Console NF", monospace',
      deps({}),
    );
    expect(result).toBe('"HackGen Console NF"');
  });

  it("canvas の解決が DOM と食い違えば FontFace 注入して別名を返す", async () => {
    const registered: string[] = [];
    const result = await resolveXtermFontFamily(
      '"HackGen Console NF", monospace',
      deps({
        measureDom: () => mono(30),
        // 注入前は誤った幅、注入後 (alias) は正しい幅を返す
        measureCanvas: (family) =>
          family.includes("ccc-terminal") ? mono(30) : mono(34),
        registerFace: async (family, alias) => {
          registered.push(`${family} -> ${alias}`);
        },
      }),
    );
    expect(registered).toEqual([
      "HackGen Console NF -> HackGen Console NF ccc-terminal",
    ]);
    expect(result).toBe('"HackGen Console NF ccc-terminal"');
  });

  it("canvas が等幅でない（last-resort に落ちた）場合も FontFace 注入する", async () => {
    const result = await resolveXtermFontFamily(
      '"HackGen Console NF", monospace',
      deps({
        measureCanvas: (family) =>
          family.includes("ccc-terminal") ? mono(30) : proportional,
      }),
    );
    expect(result).toBe('"HackGen Console NF ccc-terminal"');
  });

  it("FontFace 注入に失敗したら元の指定を保つ", async () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    const result = await resolveXtermFontFamily(
      '"HackGen Console NF", monospace',
      deps({
        measureCanvas: () => proportional,
        registerFace: async () => {
          throw new Error("font not found");
        },
      }),
    );
    expect(result).toBe('"HackGen Console NF", monospace');
    warn.mockRestore();
  });

  it("注入後も canvas で解決できなければ元の指定を保つ", async () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    const result = await resolveXtermFontFamily(
      '"HackGen Console NF", monospace',
      deps({ measureCanvas: () => proportional }),
    );
    expect(result).toBe('"HackGen Console NF", monospace');
    warn.mockRestore();
  });

  it("DOM (CSS) でも等幅解決できないファミリーは変換しない", async () => {
    const register = vi.fn();
    const result = await resolveXtermFontFamily(
      '"NoSuchFont", monospace',
      deps({ measureDom: () => proportional, registerFace: register }),
    );
    expect(result).toBe('"NoSuchFont", monospace');
    expect(register).not.toHaveBeenCalled();
  });

  it("先頭が総称ファミリーなら変換しない", async () => {
    expect(await resolveXtermFontFamily("monospace", deps({}))).toBe("monospace");
    expect(
      await resolveXtermFontFamily("ui-monospace, monospace", deps({})),
    ).toBe("ui-monospace, monospace");
  });

  it("空文字列はそのまま返す", async () => {
    expect(await resolveXtermFontFamily("", deps({}))).toBe("");
  });
});
