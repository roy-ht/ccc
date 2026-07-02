import { describe, it, expect } from "vitest";
import { statusDescriptor, statusLabel } from "./instanceStatus";
import { InstanceStatus } from "../types";

const allStatuses: InstanceStatus[] = [
  "agent_busy",
  "agent_idle",
  "agent_waiting_input",
  "running",
  "connecting",
  "disconnected",
  "terminated",
];

describe("statusDescriptor", () => {
  it("全ステータスに対して shape/color を返す", () => {
    for (const status of allStatuses) {
      const result = statusDescriptor(status);
      expect(result.shape).toMatch(/^(dot|ring|spinner|cross|bang)$/);
      expect(result.color).toMatch(/^#[0-9a-fA-F]{6}$/);
    }
  });

  it("agent_busy は緑のspinするspinner（コメット）", () => {
    expect(statusDescriptor("agent_busy")).toEqual({
      shape: "spinner",
      color: "#4ade80",
      animation: "spin",
    });
  });

  it("running は静的な青dot", () => {
    expect(statusDescriptor("running")).toEqual({
      shape: "dot",
      color: "#60a5fa",
    });
  });

  it("agent_idle は静的な黄ring", () => {
    expect(statusDescriptor("agent_idle")).toEqual({
      shape: "ring",
      color: "#facc15",
    });
  });

  it("agent_waiting_input はbounceするbang（注意喚起）", () => {
    const d = statusDescriptor("agent_waiting_input");
    expect(d.shape).toBe("bang");
    expect(d.animation).toBe("bounce");
    expect(d.color).toBe("#fb923c");
  });

  it("agent_busy と connecting は同形spinnerでも色で区別される", () => {
    expect(statusDescriptor("agent_busy").color).not.toBe(
      statusDescriptor("connecting").color
    );
  });

  it("connecting はspinするspinner", () => {
    expect(statusDescriptor("connecting")).toEqual({
      shape: "spinner",
      color: "#94a3b8",
      animation: "spin",
    });
  });

  it("disconnected は赤cross", () => {
    expect(statusDescriptor("disconnected")).toEqual({
      shape: "cross",
      color: "#f87171",
    });
  });

  it("terminated は暗灰ring（runningのdotと区別される）", () => {
    const t = statusDescriptor("terminated");
    const r = statusDescriptor("running");
    expect(t.shape).not.toBe(r.shape);
  });

  it("agent_busy と running は色が異なる（同じdotでも区別される）", () => {
    expect(statusDescriptor("agent_busy").color).not.toBe(
      statusDescriptor("running").color
    );
  });
});

describe("statusLabel", () => {
  it("全ステータスに対して日本語ラベルを返す", () => {
    for (const status of allStatuses) {
      const label = statusLabel(status);
      expect(typeof label).toBe("string");
      expect(label.length).toBeGreaterThan(0);
    }
  });

  it("各ステータスのラベルが正しい", () => {
    expect(statusLabel("agent_busy")).toBe("作業中");
    expect(statusLabel("agent_idle")).toBe("待機中");
    expect(statusLabel("agent_waiting_input")).toBe("指示待ち");
    expect(statusLabel("running")).toBe("アクティブ");
    expect(statusLabel("connecting")).toBe("接続中...");
    expect(statusLabel("disconnected")).toBe("切断");
    expect(statusLabel("terminated")).toBe("終了");
  });
});
