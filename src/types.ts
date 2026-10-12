export type TrackUsage = {
  label: string;
  kind: string;
  percent: number;
  severity: string;
  resetsAt: string | null;
  isActive: boolean;
};

/** 사용량을 읽는 곳. auto는 웹 세션이 있으면 웹 세션, 없으면 Claude Code입니다. */
export type DataSource = "auto" | "claudeCode" | "web";

export type UsageSnapshot = {
  state: string;
  /** 실제로 값을 읽은 소스 (claudeCode | web). */
  source: string | null;
  accountEmail: string | null;
  planLabel: string | null;
  tracks: TrackUsage[];
  error: string | null;
};

export type WebStatus = {
  state: "connected" | "none" | "unreadable";
  account: string | null;
  savedAt: number | null;
  expiresAt: number | null;
};

export function stateLabel(state: string): string {
  switch (state) {
    case "OK":
      return "연결됨";
    case "NeedLogin":
      return "로그인 필요";
    default:
      return "갱신 실패";
  }
}

export function sourceLabel(source: string | null): string {
  if (source === "web") return "웹 세션";
  if (source === "claudeCode") return "Claude Code";
  return "";
}

/** 로그인 필요일 때 할 일. 소스마다 로그인 방법이 다릅니다. */
export function needLoginHint(snap: UsageSnapshot, preference: DataSource): string {
  if (snap.source === "web") return "우클릭 → 설정 → 브라우저로 로그인";
  if (preference === "claudeCode") return "터미널에서 claude auth login";
  return "claude auth login 또는 설정 → 브라우저로 로그인";
}
