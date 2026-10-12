import { invoke } from "@tauri-apps/api/core";
import { emit, listen } from "@tauri-apps/api/event";
import { getCurrentWindow, LogicalSize } from "@tauri-apps/api/window";
import { applyAlwaysOnTop, loadAlwaysOnTop, loadDataSource, saveDataSource } from "./preferences";
import {
  needLoginHint,
  sourceLabel,
  stateLabel,
  type DataSource,
  type UsageSnapshot,
  type WebStatus,
} from "./types";

/** 제목 표시줄과 화면 위아래 여유. 이만큼 빼야 창 아래 끝이 작업 표시줄에 걸리지 않습니다. */
const SCREEN_MARGIN = 80;
/** tauri.conf.json의 minHeight와 같습니다. */
const MIN_HEIGHT = 440;

let autostartEnabled = false;
let isDevBuild = false;
/** 로그인 창은 최대 10분 열려 있습니다. 설정 창은 숨겨 두고 재사용하므로 상태가 유지됩니다. */
let webLoginRunning = false;
let webStatus: WebStatus | null = null;

function $(id: string): HTMLElement {
  const el = document.getElementById(id);
  if (!el) throw new Error(`missing #${id}`);
  return el;
}

/**
 * 에러 문자열에는 Rust가 옮겨 온 API 응답 본문이 섞일 수 있어서, 마크업을
 * 조립하지 않고 textContent로만 넣습니다.
 */
function showBootError(message: string) {
  const wrap = document.createElement("div");
  wrap.className = "settings-window";
  wrap.style.padding = "24px";

  const title = document.createElement("h1");
  title.textContent = "설정 로드 실패";

  const detail = document.createElement("p");
  detail.className = "settings-hint settings-hint--error";
  detail.textContent = message;

  const close = document.createElement("button");
  close.type = "button";
  close.className = "btn secondary wide";
  close.textContent = "닫기";
  close.addEventListener("click", () => {
    void closeWindow();
  });

  wrap.append(title, detail, close);
  document.body.replaceChildren(wrap);
}

function contentHeight(): number {
  const el = document.querySelector(".settings-window");
  return el ? Math.ceil(el.getBoundingClientRect().height) : 0;
}

/**
 * 화면에 들어가는 만큼만 내용 높이에 창을 맞추고 가운데로 옮깁니다. 내용이 더 길면
 * 스크롤로 넘기고 닫기 버튼은 아래에 붙어 있습니다. 고정 높이로 두면 FHD처럼 세로가
 * 짧은 화면에서 창 아래가 화면 밖으로 나가, 늘리지도 닫지도 못하게 됩니다.
 */
async function fitToScreen() {
  try {
    const win = getCurrentWindow();
    const inner = (await win.innerSize()).toLogical(await win.scaleFactor());
    const maxHeight = Math.max(MIN_HEIGHT, window.screen.availHeight - SCREEN_MARGIN);
    const target = Math.max(MIN_HEIGHT, Math.min(contentHeight(), maxHeight));
    if (Math.abs(target - inner.height) > 1) {
      await win.setSize(new LogicalSize(inner.width, target));
    }
    await win.center();
  } catch {
    /* 크기 조절 실패는 스크롤로 버팁니다 */
  }
}

/**
 * 위에 알림이 떠서 내용이 길어지면, 지금 자리에서 화면 아래 끝까지 남은 만큼만 늘립니다.
 * 사용자가 옮겨 둔 창을 움직이거나 줄이지는 않습니다. 모자라는 만큼은 스크롤입니다.
 */
async function growToContent() {
  try {
    const win = getCurrentWindow();
    const scale = await win.scaleFactor();
    const inner = (await win.innerSize()).toLogical(scale);
    const needed = contentHeight();
    if (needed <= inner.height + 1) return;

    const top = (await win.innerPosition()).toLogical(scale).y;
    const availTop = (window.screen as Screen & { availTop?: number }).availTop ?? 0;
    const room = Math.floor(availTop + window.screen.availHeight - top - 8);
    const target = Math.min(needed, room);
    if (target > inner.height + 1) {
      await win.setSize(new LogicalSize(inner.width, target));
    }
  } catch {
    /* 크기 조절 실패는 스크롤로 버팁니다 */
  }
}

function setActionStatus(message: string, tone: "info" | "ok" | "error" = "info") {
  const el = $("action-status");
  el.textContent = message;
  el.classList.remove("hidden", "settings-status--ok", "settings-status--error");
  if (tone === "ok") el.classList.add("settings-status--ok");
  else if (tone === "error") el.classList.add("settings-status--error");
  void growToContent();
}

function clearActionStatus() {
  $("action-status").classList.add("hidden");
  $("action-status").textContent = "";
}

function badgeClass(state: string): string {
  if (state === "OK") return "settings-badge--ok";
  if (state === "NeedLogin") return "settings-badge--muted";
  return "settings-badge--error";
}

function renderAccount(snap: UsageSnapshot) {
  const badge = $("connection-badge");
  badge.textContent = stateLabel(snap.state);
  badge.className = `settings-badge ${badgeClass(snap.state)}`;

  $("account-email").textContent = snap.accountEmail ?? "로그인 정보 없음";

  // 두 소스의 계정이 다를 수 있어 어느 소스에서 읽었는지 같이 보여 줍니다.
  const source = sourceLabel(snap.source);
  const prefix = source ? `${source} · ` : "";
  const plan = $("account-plan");
  if (snap.state === "NeedLogin") {
    plan.textContent = `${prefix}${needLoginHint(snap, loadDataSource())}`;
    return;
  }
  if (snap.state !== "OK") {
    plan.textContent = `${prefix}${snap.error ?? "사용량을 불러오지 못했습니다."}`;
    return;
  }
  plan.textContent = `${prefix}${snap.planLabel ?? "플랜 정보 없음"}`;
}

function formatTime(ms: number | null): string {
  if (!ms) return "—";
  return new Date(ms).toLocaleString("ko-KR", {
    month: "numeric",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
    hour12: false,
  });
}

function renderWebStatus() {
  const status = $("web-status");
  const detail = $("web-detail");
  const state = webStatus?.state ?? "none";

  if (state === "connected") {
    status.textContent = `연결됨 · ${webStatus?.account ?? "계정 정보 없음"}`;
    detail.textContent = `저장 ${formatTime(webStatus?.savedAt ?? null)} · 만료 예상 ${formatTime(webStatus?.expiresAt ?? null)}`;
  } else if (state === "unreadable") {
    status.textContent = "저장된 세션을 읽지 못했습니다";
    detail.textContent = "다른 Windows 계정에서 저장됐거나 손상됐습니다. 다시 로그인하세요.";
  } else {
    status.textContent = "연결 안 됨";
    detail.textContent = "";
  }

  ($("btn-web-login") as HTMLButtonElement).disabled = webLoginRunning;
  ($("btn-web-disconnect") as HTMLButtonElement).disabled = webLoginRunning || state === "none";
  ($("btn-web-reset") as HTMLButtonElement).disabled = webLoginRunning;
}

async function refreshWebStatus() {
  webStatus = await invoke<WebStatus>("get_web_status");
  renderWebStatus();
}

function setWebProgress(message: string | null) {
  const el = $("web-progress");
  el.textContent = message ?? "";
  el.classList.toggle("hidden", !message);
  if (message) void growToContent();
}

async function onWebLogin() {
  clearActionStatus();
  webLoginRunning = true;
  renderWebStatus();
  setWebProgress("로그인 창을 여는 중…");
  try {
    webStatus = await invoke<WebStatus>("web_login");
    setActionStatus("웹 세션을 연결했습니다.", "ok");
    await emit("refresh-requested");
  } catch (e) {
    setActionStatus(String(e), "error");
  } finally {
    webLoginRunning = false;
    setWebProgress(null);
    renderWebStatus();
  }
}

async function onWebDisconnect() {
  clearActionStatus();
  try {
    webStatus = await invoke<WebStatus>("web_disconnect");
    setActionStatus("웹 세션 연결을 해제했습니다.", "ok");
    await emit("refresh-requested");
  } catch (e) {
    setActionStatus(String(e), "error");
  }
  renderWebStatus();
}

async function onWebReset() {
  clearActionStatus();
  const ok = window.confirm(
    "위젯 전용 로그인 프로필과 저장된 세션을 모두 지웁니다. 다음 로그인 때 처음부터 로그인해야 합니다. 계속할까요?",
  );
  if (!ok) return;
  try {
    webStatus = await invoke<WebStatus>("web_reset_profile");
    setActionStatus("로그인 프로필을 초기화했습니다.", "ok");
    await emit("refresh-requested");
  } catch (e) {
    setActionStatus(String(e), "error");
  }
  renderWebStatus();
}

function renderDataSource() {
  ($("source-select") as HTMLSelectElement).value = loadDataSource();
}

function renderAlwaysOnTop() {
  ($("always-on-top-toggle") as HTMLInputElement).checked = loadAlwaysOnTop();
}

function syncAutostartUi() {
  const toggle = $("autostart-toggle") as HTMLInputElement;
  const hint = $("autostart-hint");

  if (isDevBuild) {
    toggle.checked = false;
    toggle.disabled = true;
    hint.textContent =
      "개발 모드에서는 시작프로그램을 바꿀 수 없습니다. 「시작.bat」으로 설치·실행한 뒤 다시 시도하세요.";
    hint.classList.remove("hidden");
    return;
  }

  toggle.disabled = false;
  toggle.checked = autostartEnabled;
  hint.classList.add("hidden");
}

async function refreshAutostart() {
  isDevBuild = await invoke<boolean>("is_dev_build");
  autostartEnabled = await invoke<boolean>("is_autostart_enabled");
  syncAutostartUi();
}

/**
 * 사용량은 위젯이 혼자 가져옵니다. 설정 창이 직접 get_usage를 부르면 같은 API를
 * 두 창이 각자 두드려 폴링 간격이 사실상 절반이 되므로, 필요할 때 위젯에 요청해
 * 그 결과(usage-updated)를 받아 그립니다.
 */
async function requestUsage() {
  await emit("usage-requested");
}

async function closeWindow() {
  await invoke("close_settings_window");
}

async function refreshView() {
  clearActionStatus();
  renderAlwaysOnTop();
  renderDataSource();
  // 위젯이 주기마다 보내는 사용량을 숨긴 채로도 받아 그려 두어서, 내용 높이는 이미 맞습니다.
  // 값을 다시 읽기 전에 먼저 맞춰야 창이 뜬 뒤 크기가 튀는 시간이 짧습니다.
  await fitToScreen();
  await refreshAutostart();
  await refreshWebStatus();
  await requestUsage();
}

async function onRefresh() {
  clearActionStatus();
  const btn = $("btn-refresh") as HTMLButtonElement;
  btn.disabled = true;
  try {
    await emit("refresh-requested");
    setActionStatus("사용량을 갱신하고 있습니다…");
  } catch (e) {
    setActionStatus(String(e), "error");
  } finally {
    btn.disabled = false;
  }
}

async function boot() {
  try {
    renderAlwaysOnTop();
    renderDataSource();
    await refreshAutostart();
    await refreshWebStatus();

    $("btn-close").addEventListener("click", () => {
      void closeWindow();
    });
    $("btn-refresh").addEventListener("click", () => {
      void onRefresh();
    });

    $("btn-web-login").addEventListener("click", () => {
      void onWebLogin();
    });
    $("btn-web-disconnect").addEventListener("click", () => {
      void onWebDisconnect();
    });
    $("btn-web-reset").addEventListener("click", () => {
      void onWebReset();
    });

    // 위젯이 다음 조회에서 읽도록 저장만 하고 바로 새로고침을 요청합니다.
    const sourceSelect = $("source-select") as HTMLSelectElement;
    sourceSelect.addEventListener("change", async () => {
      saveDataSource(sourceSelect.value as DataSource);
      setActionStatus("데이터 소스를 바꿨습니다. 사용량을 다시 읽고 있습니다…", "ok");
      await emit("refresh-requested");
    });

    const alwaysOnTopToggle = $("always-on-top-toggle") as HTMLInputElement;
    alwaysOnTopToggle.addEventListener("change", async () => {
      const enabled = alwaysOnTopToggle.checked;
      try {
        await applyAlwaysOnTop(enabled);
        setActionStatus(
          enabled ? "위젯을 항상 위에 표시합니다." : "위젯을 일반 창처럼 표시합니다.",
          "ok",
        );
      } catch (e) {
        alwaysOnTopToggle.checked = loadAlwaysOnTop();
        setActionStatus(String(e), "error");
      }
    });

    const autostartToggle = $("autostart-toggle") as HTMLInputElement;
    autostartToggle.addEventListener("change", async () => {
      if (isDevBuild) {
        syncAutostartUi();
        return;
      }
      autostartToggle.disabled = true;
      try {
        if (autostartToggle.checked) {
          await invoke("enable_autostart");
        } else {
          await invoke("disable_autostart");
        }
        autostartEnabled = await invoke<boolean>("is_autostart_enabled");
        syncAutostartUi();
        setActionStatus(
          autostartEnabled ? "시작프로그램을 켰습니다." : "시작프로그램을 껐습니다.",
          "ok",
        );
      } catch (e) {
        autostartToggle.checked = autostartEnabled;
        setActionStatus(String(e), "error");
      } finally {
        autostartToggle.disabled = isDevBuild;
      }
    });

    // 숨겨져 있을 때는 다음에 열릴 크기를 미리 맞춰 두고, 떠 있을 때는 내용이
    // 늘어난 만큼만 키웁니다.
    await listen<UsageSnapshot>("usage-updated", async (event) => {
      renderAccount(event.payload);
      // 무인 갱신이 세션을 새로 저장했을 수 있습니다.
      await refreshWebStatus().catch(() => {});
      const visible = await getCurrentWindow()
        .isVisible()
        .catch(() => true);
      void (visible ? growToContent() : fitToScreen());
    });

    await listen<string>("web-login-status", (event) => {
      if (webLoginRunning) setWebProgress(event.payload);
    });

    // 창은 숨겨둔 채 재사용하므로, 다시 열릴 때마다 값을 새로 읽습니다.
    await listen("settings-open", () => {
      void refreshView();
    });

    window.addEventListener("keydown", (event) => {
      if (event.key === "Escape") void closeWindow();
    });

    await requestUsage();
  } catch (e) {
    showBootError(String(e));
  }
}

void boot();
