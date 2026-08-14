import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow, LogicalSize } from "@tauri-apps/api/window";

type TrackUsage = {
  label: string;
  kind: string;
  percent: number;
  severity: string;
  resetsAt: string | null;
  isActive: boolean;
};

type UsageSnapshot = {
  state: string;
  accountEmail: string | null;
  planLabel: string | null;
  tracks: TrackUsage[];
  error: string | null;
};

type ContextMenuState = {
  x: number;
  y: number;
  autostartEnabled: boolean;
  isDevBuild: boolean;
};

const WINDOW_WIDTH = 340;

let lastSnapshot: UsageSnapshot | null = null;

function $(id: string): HTMLElement {
  const el = document.getElementById(id);
  if (!el) throw new Error(`missing #${id}`);
  return el;
}

async function getAutostartEnabled(): Promise<boolean> {
  try {
    return await invoke<boolean>("is_autostart_enabled");
  } catch {
    return false;
  }
}

async function getIsDevBuild(): Promise<boolean> {
  try {
    return await invoke<boolean>("is_dev_build");
  } catch {
    return false;
  }
}

function hideContextMenu() {
  $("context-menu").classList.add("hidden");
  $("context-backdrop").classList.add("hidden");
}

function showContextMenu(state: ContextMenuState) {
  const backdrop = $("context-backdrop");
  const menu = $("context-menu");
  const menuAutostart = $("menu-autostart");

  // 위젯 본체는 사용량만 보여주고, 어느 계정인지는 여기서 확인합니다.
  // CLI 로그인 계정과 데스크톱 앱 계정이 다를 수 있어 표시가 필요합니다.
  $("menu-email").textContent = lastSnapshot?.accountEmail ?? "로그인 정보 없음";
  $("menu-plan").textContent = lastSnapshot?.planLabel ?? "";

  backdrop.classList.remove("hidden");
  menu.classList.remove("hidden");

  if (state.isDevBuild) {
    menuAutostart.textContent = "시작프로그램 (시작.bat 사용)";
  } else {
    menuAutostart.textContent = `${state.autostartEnabled ? "✓ " : ""}시작프로그램`;
  }

  const menuRect = menu.getBoundingClientRect();
  const maxX = Math.max(8, window.innerWidth - menuRect.width - 8);
  const maxY = Math.max(8, window.innerHeight - menuRect.height - 8);

  menu.style.left = `${Math.min(state.x, maxX)}px`;
  menu.style.top = `${Math.min(state.y, maxY)}px`;
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
  const menuAutostart = $("menu-autostart") as HTMLButtonElement;
  const menuRefresh = $("menu-refresh") as HTMLButtonElement;
  const menuQuit = $("menu-quit") as HTMLButtonElement;

  window.addEventListener("contextmenu", async (event) => {
    event.preventDefault();
    showContextMenu({
      x: event.clientX,
      y: event.clientY,
      autostartEnabled: await getAutostartEnabled(),
      isDevBuild: await getIsDevBuild(),
    });
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

  menuAutostart.addEventListener("click", async (event) => {
    event.stopPropagation();
    menuAutostart.disabled = true;
    try {
      const isDev = await getIsDevBuild();
      if (isDev) {
        window.alert(
          "개발 모드에서는 시작프로그램을 바꿀 수 없습니다.\n\n「시작.bat」으로 설치·실행한 뒤, 위젯에서 다시 우클릭 → 시작프로그램을 켜 주세요.",
        );
        return;
      }
      const enabled = await getAutostartEnabled();
      if (enabled) {
        await invoke("disable_autostart");
      } else {
        await invoke("enable_autostart");
      }
    } catch (e) {
      window.alert(String(e));
    } finally {
      menuAutostart.disabled = false;
      showContextMenu({
        x: parseFloat($("context-menu").style.left || "0"),
        y: parseFloat($("context-menu").style.top || "0"),
        autostartEnabled: await getAutostartEnabled(),
        isDevBuild: await getIsDevBuild(),
      });
    }
  });

  menuRefresh.addEventListener("click", async (event) => {
    event.stopPropagation();
    hideContextMenu();
    await refresh();
  });

  menuQuit.addEventListener("click", async (event) => {
    event.stopPropagation();
    hideContextMenu();
    await invoke("quit_app");
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
