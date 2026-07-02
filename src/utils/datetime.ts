/** unix ms を「M/D HH:mm」形式に整形する（同年は年を省く）。null は空文字。 */
export function formatTimestamp(ms: number | null | undefined): string {
  if (ms == null) return "";
  const d = new Date(ms);
  if (Number.isNaN(d.getTime())) return "";
  const now = new Date();
  const sameYear = d.getFullYear() === now.getFullYear();
  const pad = (n: number) => String(n).padStart(2, "0");
  const date = sameYear
    ? `${d.getMonth() + 1}/${d.getDate()}`
    : `${d.getFullYear()}/${d.getMonth() + 1}/${d.getDate()}`;
  return `${date} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

/** セッション活動時刻（ended_at 優先、無ければ started_at）を整形する。 */
export function sessionTime(started?: number | null, ended?: number | null): string {
  return formatTimestamp(ended ?? started ?? null);
}
