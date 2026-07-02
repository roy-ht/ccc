import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

/**
 * `list_claude_profiles` の結果を取得し、選択肢として返すフック。
 * `currentProfile` を渡すと、サーバーから返ってきた一覧に含まれていない場合に
 * 末尾へ追加し、既存設定が UI から消えないようにする。
 */
export function useClaudeProfiles(currentProfile?: string): string[] {
  const fallback = currentProfile && currentProfile.length > 0 ? currentProfile : "default";
  const [choices, setChoices] = useState<string[]>([fallback]);

  useEffect(() => {
    invoke<string[]>("list_claude_profiles")
      .then((profiles) => {
        if (currentProfile && !profiles.includes(currentProfile)) {
          setChoices([...profiles, currentProfile]);
        } else if (profiles.length === 0) {
          setChoices([fallback]);
        } else {
          setChoices(profiles);
        }
      })
      .catch(() => setChoices([fallback]));
  }, [currentProfile, fallback]);

  return choices;
}
