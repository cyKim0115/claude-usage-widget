import { invoke } from "@tauri-apps/api/core";

export const ALWAYS_ON_TOP_KEY = "claude-usage-always-on-top";

/**
 * 위젯과 설정 창은 같은 origin(tauri://localhost)이라 localStorage를 공유합니다.
 * 덕분에 설정 값 자체는 IPC 없이 양쪽에서 읽히고, "이미 열려 있는 창에 알리기"만
 * 이벤트로 처리하면 됩니다.
 */

/** The widget shipped always-on-top, so an unset key must stay on. */
export function loadAlwaysOnTop(): boolean {
  return localStorage.getItem(ALWAYS_ON_TOP_KEY) !== "false";
}

export function saveAlwaysOnTop(enabled: boolean) {
  localStorage.setItem(ALWAYS_ON_TOP_KEY, enabled ? "true" : "false");
}

/** 저장과 창 플래그 반영을 함께 합니다. 설정 창과 부팅 경로가 같이 씁니다. */
export async function applyAlwaysOnTop(enabled: boolean) {
  saveAlwaysOnTop(enabled);
  await invoke("set_always_on_top", { enabled });
}
