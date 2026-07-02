import { describe, it, expect } from "vitest";
import { reconcileInstances } from "./reconcileInstances";
import { InstanceInfo } from "../types";

function makeInstance(overrides: Partial<InstanceInfo> & { id: string }): InstanceInfo {
  return {
    kind: "local",
    name: "test",
    status: "running",
    instance_hash: "abcd1234",
    instance_dir: "/tmp/test",
    agent_profile: "default",
    ...overrides,
  };
}

describe("reconcileInstances", () => {
  it("スナップショットにのみ存在するインスタンスはそのまま追加される", () => {
    const prev: InstanceInfo[] = [];
    const snapshot = [makeInstance({ id: "a", status: "connecting" })];
    const got = reconcileInstances(prev, snapshot);
    expect(got).toEqual(snapshot);
  });

  it("既存インスタンスの状態系フィールドは現在値を保持する", () => {
    // イベントで agent_busy に更新済みの state を、invoke 時点の古い
    // スナップショット (running) が巻き戻さないこと。
    const prev = [
      makeInstance({
        id: "a",
        status: "agent_busy",
        status_message: "Bash 実行中: cargo test",
        current_session_id: "sess-1",
      }),
    ];
    const snapshot = [makeInstance({ id: "a", status: "running" })];
    const got = reconcileInstances(prev, snapshot);
    expect(got).toHaveLength(1);
    expect(got[0].status).toBe("agent_busy");
    expect(got[0].status_message).toBe("Bash 実行中: cargo test");
    expect(got[0].current_session_id).toBe("sess-1");
  });

  it("非状態フィールドはスナップショットに従う", () => {
    const prev = [makeInstance({ id: "a", name: "old-name" })];
    const snapshot = [makeInstance({ id: "a", name: "new-name" })];
    const got = reconcileInstances(prev, snapshot);
    expect(got[0].name).toBe("new-name");
  });

  it("スナップショットに無いインスタンスは削除される", () => {
    const prev = [makeInstance({ id: "a" }), makeInstance({ id: "temp" })];
    const snapshot = [makeInstance({ id: "a" })];
    const got = reconcileInstances(prev, snapshot);
    expect(got.map((i) => i.id)).toEqual(["a"]);
  });

  it("pending_prompt も現在値を保持する", () => {
    const prev = [
      makeInstance({
        id: "a",
        status: "agent_waiting_input",
        pending_prompt: { kind: "plan" },
      }),
    ];
    const snapshot = [makeInstance({ id: "a", status: "running" })];
    const got = reconcileInstances(prev, snapshot);
    expect(got[0].pending_prompt).toEqual({ kind: "plan" });
  });
});
