import { useEffect, useRef, useState } from "react";
import { InstanceId, InstanceInfo, InstanceStatus } from "../types";
import { statusDescriptor } from "../utils/instanceStatus";

/**
 * インスタンスの状態遷移を監視し、未読変化マーカーを返す。
 *
 * 非アクティブなインスタンスで「注目すべき変化」（作業完了 busy→idle、
 * または指示待ち化）が起きてから、ユーザーがそのインスタンスを開くまで
 * 保持する（値は表示色）。
 *
 * 遷移の瞬間の演出（行フラッシュ・ボーダービーム等）は試した結果
 * 派手すぎたため廃止し、未読ドットのみに一本化した。
 *
 * 初回マウントや復元で新しい id が現れただけでは発火しない（前回 status が
 * ある id の変化のみを遷移とみなす）。
 */
export function useStatusTransitions(
  instances: InstanceInfo[],
  activeId: InstanceId | null
): { unread: Map<InstanceId, string> } {
  const prevStatusRef = useRef<Map<InstanceId, InstanceStatus>>(new Map());
  const [unread, setUnread] = useState<Map<InstanceId, string>>(new Map());

  useEffect(() => {
    const prev = prevStatusRef.current;
    const newUnread: Array<[InstanceId, string]> = [];

    for (const inst of instances) {
      const before = prev.get(inst.id);
      if (before === undefined || before === inst.status) continue;

      // 未読: 見ていないインスタンスの「完了」と「指示待ち化」だけを残す
      const noteworthy =
        (before === "agent_busy" && inst.status === "agent_idle") ||
        inst.status === "agent_waiting_input";
      if (noteworthy && inst.id !== activeId) {
        newUnread.push([inst.id, statusDescriptor(inst.status).color]);
      }
    }

    // 現在の status を控える（消えた id は掃除）
    const nextPrev = new Map<InstanceId, InstanceStatus>();
    for (const inst of instances) nextPrev.set(inst.id, inst.status);
    prevStatusRef.current = nextPrev;

    if (newUnread.length > 0) {
      setUnread((cur) => {
        const next = new Map(cur);
        for (const [id, color] of newUnread) next.set(id, color);
        for (const id of next.keys()) {
          if (!nextPrev.has(id)) next.delete(id);
        }
        return next;
      });
    }
  }, [instances, activeId]);

  // アクティブになったインスタンスの未読は既読化する
  useEffect(() => {
    if (activeId === null) return;
    setUnread((cur) => {
      if (!cur.has(activeId)) return cur;
      const next = new Map(cur);
      next.delete(activeId);
      return next;
    });
  }, [activeId]);

  return { unread };
}
