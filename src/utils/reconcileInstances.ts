import { InstanceInfo } from "../types";

/**
 * create / recreate 後に取得した `list_instances` スナップショットを
 * 現在の state に反映する。
 *
 * 追加・削除と非状態フィールド（name / directory 等）はスナップショットに
 * 従い、既存インスタンスの状態系フィールド（status / status_message /
 * pending_prompt / current_session_id）は現在値を保持する。
 *
 * スナップショットは invoke 時点の値であり、応答が届くまでの間に
 * `instance-status-changed` イベントで更新された新しい状態を古い値で
 * 巻き戻し得るため（バックエンドは状態変更を必ず emit するので、
 * イベント駆動の現在値が常に最新）。
 */
export function reconcileInstances(
  prev: InstanceInfo[],
  snapshot: InstanceInfo[]
): InstanceInfo[] {
  const prevById = new Map(prev.map((p) => [p.id, p]));
  return snapshot.map((s) => {
    const p = prevById.get(s.id);
    if (!p) return s;
    return {
      ...s,
      status: p.status,
      status_message: p.status_message,
      pending_prompt: p.pending_prompt,
      current_session_id: p.current_session_id,
    };
  });
}
