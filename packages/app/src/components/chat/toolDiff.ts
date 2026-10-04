export interface DiffRow {
  num: number;
  type: string;
  content: string;
}

export function splitLines(text: string): string[] {
  const t = text.endsWith("\n") ? text.slice(0, -1) : text;
  return t === "" ? [] : t.split("\n");
}

// 行级 LCS diff；过大输入退化为整块替换，避免 LCS 表过大
export function buildDiffRows(oldLines: string[], newLines: string[]): DiffRow[] {
  const rows: DiffRow[] = [];
  const n = oldLines.length;
  const m = newLines.length;
  if (n * m > 1_000_000) {
    for (const [i, content] of oldLines.entries())
      rows.push({ num: i + 1, type: "remove", content });
    for (const [i, content] of newLines.entries())
      rows.push({ num: i + 1, type: "add", content });
    return rows;
  }
  const dp: Uint32Array[] = [];
  for (let i = 0; i <= n; i++) dp.push(new Uint32Array(m + 1));
  for (let i = n - 1; i >= 0; i--) {
    for (let j = m - 1; j >= 0; j--) {
      dp[i][j] =
        oldLines[i] === newLines[j]
          ? dp[i + 1][j + 1] + 1
          : Math.max(dp[i + 1][j], dp[i][j + 1]);
    }
  }
  let i = 0;
  let j = 0;
  let oldNo = 1;
  let newNo = 1;
  while (i < n && j < m) {
    if (oldLines[i] === newLines[j]) {
      rows.push({ num: newNo, type: "context", content: oldLines[i] });
      i++;
      j++;
      oldNo++;
      newNo++;
    } else if (dp[i + 1][j] >= dp[i][j + 1]) {
      rows.push({ num: oldNo, type: "remove", content: oldLines[i] });
      i++;
      oldNo++;
    } else {
      rows.push({ num: newNo, type: "add", content: newLines[j] });
      j++;
      newNo++;
    }
  }
  while (i < n) {
    rows.push({ num: oldNo, type: "remove", content: oldLines[i] });
    i++;
    oldNo++;
  }
  while (j < m) {
    rows.push({ num: newNo, type: "add", content: newLines[j] });
    j++;
    newNo++;
  }
  return rows;
}
