export type TrackUsage = {
  label: string;
  kind: string;
  percent: number;
  severity: string;
  resetsAt: string | null;
  isActive: boolean;
};

export type UsageSnapshot = {
  state: string;
  accountEmail: string | null;
  planLabel: string | null;
  tracks: TrackUsage[];
  error: string | null;
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
