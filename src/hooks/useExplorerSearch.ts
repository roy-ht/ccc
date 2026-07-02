import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { useDebouncedValue } from "./useDebouncedValue";
import type { SearchHit } from "../types";

interface Params {
  instanceId: string;
  rootDir: string;
}

/**
 * Explorer の検索状態。クエリ確定（debounce 300ms）で `explorer_search` を呼ぶ。
 * 同時実行の取り違えを避けるため、cancelled フラグで最新結果以外を捨てる。
 */
export function useExplorerSearch({ instanceId, rootDir }: Params) {
  const [rawQuery, setRawQuery] = useState("");
  const [glob, setGlob] = useState("");
  const query = useDebouncedValue(rawQuery.trim(), 300);
  const active = query !== "";

  const [results, setResults] = useState<SearchHit[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!active) {
      setResults([]);
      setError(null);
      setLoading(false);
      return;
    }
    let cancelled = false;
    setLoading(true);
    setError(null);
    invoke<SearchHit[]>("explorer_search", {
      instanceId,
      root: rootDir,
      query,
      glob: glob.trim() || null,
      maxResults: 500,
    })
      .then((hits) => {
        if (!cancelled) setResults(hits);
      })
      .catch((e) => {
        if (!cancelled) {
          setResults([]);
          setError(String(e));
        }
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [active, query, glob, instanceId, rootDir]);

  return {
    rawQuery,
    setRawQuery,
    glob,
    setGlob,
    query,
    active,
    results,
    loading,
    error,
  };
}
