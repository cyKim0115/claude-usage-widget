import { invoke } from "@tauri-apps/api/core";
import type { DataSource } from "./types";

export const ALWAYS_ON_TOP_KEY = "claude-usage-always-on-top";
export const DATA_SOURCE_KEY = "claude-usage-data-source";
export const GLASS_KEY = "claude-usage-glass";
export const GLASS_BLUR_KEY = "claude-usage-glass-blur";

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

/** 지원하지 않는 Windows에서는 Rust가 알아서 끄므로, 비어 있으면 켬으로 봅니다. */
export function loadGlass(): boolean {
  return localStorage.getItem(GLASS_KEY) !== "false";
}

export function saveGlass(enabled: boolean) {
  localStorage.setItem(GLASS_KEY, enabled ? "true" : "false");
}

/** 저장하고 위젯 창에 반영합니다. 실제로 켜졌는지 돌려줍니다. */
export async function applyGlass(enabled: boolean): Promise<boolean> {
  saveGlass(enabled);
  return invoke<boolean>("set_glass", { enabled });
}

/** 흐림 반경(CSS px). glass.rs의 MAX_BLUR_PX, 설정 슬라이더 max와 같아야 합니다. */
export const MAX_GLASS_BLUR = 32;
const DEFAULT_GLASS_BLUR = 12;

export function loadGlassBlur(): number {
  const raw = localStorage.getItem(GLASS_BLUR_KEY);
  const value = raw === null ? NaN : Number(raw);
  return Number.isNaN(value) ? DEFAULT_GLASS_BLUR : Math.min(MAX_GLASS_BLUR, Math.max(0, value));
}

/** 저장하고 위젯 창에 반영합니다. Rust가 캡처 여백을 맞추고 위젯에 반경을 알립니다. */
export async function applyGlassBlur(px: number) {
  localStorage.setItem(GLASS_BLUR_KEY, String(px));
  await invoke("set_glass_blur", { px });
}

export function loadDataSource(): DataSource {
  const value = localStorage.getItem(DATA_SOURCE_KEY);
  return value === "claudeCode" || value === "web" ? value : "auto";
}

export function saveDataSource(source: DataSource) {
  localStorage.setItem(DATA_SOURCE_KEY, source);
}
