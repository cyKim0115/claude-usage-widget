import { invoke } from "@tauri-apps/api/core";
import { emit, listen } from "@tauri-apps/api/event";
import { getCurrentWindow, LogicalSize } from "@tauri-apps/api/window";
import { loadAlwaysOnTop } from "./preferences";
import type { TrackUsage, UsageSnapshot } from "./types";

const WINDOW_WIDTH = 340;

let lastSnapshot: UsageSnapshot | null = null;

function $(id: string): HTMLElement {
  const el = document.getElementById(id);
  if (!el) throw new Error(`missing #${id}`);
  return el;
}

function hideContextMenu() {
  $("context-menu").classList.add("hidden");
  $("context-backdrop").classList.add("hidden");
}

/**
 * 메뉴에는 설정과 종료만 둡니다. 창이 트랙 수에 맞춰 160px 안팎까지 줄어들어서,
 * 항목이 늘어나면 메뉴가 창 밖으로 잘려 종료를 누를 수 없게 됩니다. 나머지
 * 항목은 별도 설정 창(settings.html)으로 옮겼습니다.
 */
function showContextMenu(x: number, y: number) {
  const backdrop = $("context-backdrop");
  const menu = $("context-menu");

  backdrop.classList.remove("hidden");
  menu.classList.remove("hidden");

  const menuRect = menu.getBoundingClientRect();
  const maxX = Math.max(8, window.innerWidth - menuRect.width - 8);
  const maxY = Math.max(8, window.innerHeight - menuRect.height - 8);

  menu.style.left = `${Math.min(x, maxX)}px`;
  menu.style.top = `${Math.min(y, maxY)}px`;
}

function formatResetRemaining(resetsAt: string | null): string {
  if (!resetsAt) return "";
  const end = Date.parse(resetsAt);
  if (Number.isNaN(end)) return "";

  const diff = end - Date.now();
  if (diff <= 0) return "곧 초기화";

  const dayMs = 24 * 60 * 60 * 1000;
  const hourMs = 60 * 60 * 1000;
  const minuteMs = 60 * 1000;

  const days = Math.floor(diff / dayMs);
  if (days >= 1) return `${days}일 남음`;

  const hours = Math.floor(diff / hourMs);
  if (hours >= 1) return `${hours}시간 남음`;

  const minutes = Math.max(1, Math.floor(diff / minuteMs));
  return `${minutes}분 남음`;
}

/**
 * 바 색상 단계. cursor-usage-widget과 같은 기준(70% / 90%)을 씁니다.
 *
 * 응답의 severity 필드는 쓰지 않습니다. 지금까지 "normal" 외의 값을 관측하지
 * 못해 다른 단계의 이름을 알 수 없고, 추측한 문자열에 색을 걸면 실제 값이
 * 다를 때 조용히 경고가 사라집니다.
 */
function severityClass(track: TrackUsage): "" | "warn" | "hot" {
  if (track.percent >= 90) return "hot";
  if (track.percent >= 70) return "warn";
  return "";
}

function buildTrack(track: TrackUsage): HTMLElement {
  const section = document.createElement("section");
  section.className = "track";
  if (track.isActive) section.classList.add("active");

  const row = document.createElement("div");
  row.className = "row";

  const label = document.createElement("span");
  label.className = "label";
  label.textContent = track.label;

  const caption = document.createElement("span");
  caption.className = "caption";
  const remaining = formatResetRemaining(track.resetsAt);
  caption.textContent = remaining
    ? `${Math.round(track.percent)}% · ${remaining}`
    : `${Math.round(track.percent)}%`;

  row.append(label, caption);

  const bar = document.createElement("div");
  bar.className = "bar";
  bar.setAttribute("aria-hidden", "true");

  const fill = document.createElement("div");
  fill.className = "fill";
  const level = severityClass(track);
  if (level) fill.classList.add(level);
  fill.style.width = `${Math.max(0, Math.min(100, track.percent))}%`;

  bar.append(fill);
  section.append(row, bar);
  return section;
}

function renderEmpty(message: string): HTMLElement {
  const empty = document.createElement("div");
  empty.className = "empty";
  empty.textContent = message;
  return empty;
}

/** 트랙 수가 플랜마다 달라서, 그린 다음 실제 높이에 창을 맞춥니다. */
async function syncWindowHeight() {
  await new Promise((resolve) => requestAnimationFrame(resolve));
  const height = Math.ceil($("widget").getBoundingClientRect().height);
  if (height <= 0) return;
  try {
    await getCurrentWindow().setSize(new LogicalSize(WINDOW_WIDTH, height));
  } catch {
    /* 크기 조절 실패는 표시를 막지 않습니다 */
  }
}

function render(snap: UsageSnapshot) {
  lastSnapshot = snap;

  const widget = $("widget");
  widget.classList.toggle("error", snap.state !== "OK");

  // 플랜에 따라 트랙 구성이 달라져서, 지금 보는 값이 어느 플랜 기준인지 함께 둡니다.
  // 로그인 전이나 갱신 실패면 빈 문자열이라 헤더가 제목만 남습니다.
  $("plan").textContent = snap.planLabel ?? "";

  const tracks = $("tracks");
  tracks.replaceChildren();

  if (snap.tracks.length > 0) {
    tracks.append(...snap.tracks.map(buildTrack));
  } else if (snap.state === "NeedLogin") {
    tracks.append(renderEmpty("터미널에서 claude auth login"));
  } else {
    tracks.append(renderEmpty("사용량을 불러오지 못했습니다"));
  }

  const now = new Date();
  const hhmm = now.toLocaleTimeString([], {
    hour: "2-digit",
    minute: "2-digit",
    hour12: false,
  });
  const status = $("status");
  if (snap.state === "OK") {
    status.textContent = `${hhmm} 갱신`;
  } else if (snap.state === "NeedLogin") {
    status.textContent = "Claude 로그인 필요";
  } else {
    status.textContent = `갱신 실패 · ${hhmm}`;
  }

  void syncWindowHeight();

  // 설정 창이 계정/플랜을 직접 조회하지 않고 이 값을 받아 그립니다.
  void emit("usage-updated", snap).catch(() => {
    /* 설정 창이 없을 수도 있어 실패는 무시합니다 */
  });
}

async function refresh() {
  try {
    const snap = await invoke<UsageSnapshot>("get_usage");
    render(snap);
  } catch (e) {
    render({
      state: "FetchError",
      accountEmail: null,
      planLabel: null,
      tracks: [],
      error: String(e),
    });
  }
}

async function boot() {
  const backdrop = $("context-backdrop");
  const menuSettings = $("menu-settings") as HTMLButtonElement;
  const menuQuit = $("menu-quit") as HTMLButtonElement;

  // tauri.conf.json pins the window to always-on-top, so a user who turned it
  // off gets it restored as soon as the webview boots.
  try {
    await invoke("set_always_on_top", { enabled: loadAlwaysOnTop() });
  } catch {
    /* browser preview */
  }

  window.addEventListener("contextmenu", (event) => {
    event.preventDefault();
    showContextMenu(event.clientX, event.clientY);
  });

  backdrop.addEventListener("pointerdown", (event) => {
    if (event.target === backdrop) hideContextMenu();
  });

  $("context-menu").addEventListener("pointerdown", (event) => {
    event.stopPropagation();
  });

  window.addEventListener("blur", () => {
    hideContextMenu();
  });

  window.addEventListener("resize", () => {
    hideContextMenu();
  });

  window.addEventListener("click", () => {
    hideContextMenu();
  });

  window.addEventListener("keydown", (event) => {
    if (event.key === "Escape") hideContextMenu();
  });

  menuSettings.addEventListener("click", async (event) => {
    event.stopPropagation();
    hideContextMenu();
    try {
      await invoke("open_settings_window");
    } catch (e) {
      window.alert(String(e));
    }
  });

  menuQuit.addEventListener("click", async (event) => {
    event.stopPropagation();
    hideContextMenu();
    await invoke("quit_app");
  });

  // 설정 창이 열릴 때 계정 표시에 쓸 스냅샷을 요구합니다. 아직 한 번도 못
  // 받아왔다면 새로 조회해서, 설정 창이 빈 값으로 남지 않게 합니다.
  await listen("usage-requested", () => {
    if (lastSnapshot) {
      void emit("usage-updated", lastSnapshot);
    } else {
      void refresh();
    }
  });

  // 새로고침은 위젯이 수행합니다. 두 창이 각자 조회하면 폴링 간격이 어긋납니다.
  await listen("refresh-requested", () => {
    void refresh();
  });

  await refresh();
  let interval = 300_000;
  try {
    interval = await invoke<number>("get_poll_interval_ms");
  } catch {
    /* keep default */
  }
  window.setInterval(() => {
    void refresh();
  }, interval);
}

void boot();
