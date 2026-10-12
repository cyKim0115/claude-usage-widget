// Phase 0 스파이크: 위젯 전용 Chrome 프로필로 claude.ai에 로그인받고 sessionKey만으로
// 사용량을 평문 HTTP로 가져올 수 있는지 확인합니다. (Node 22+, 의존성 없음)
//
//   node scripts/spike_web_session.mjs login     포트 없이 창을 띄우고, 사용자가 로그인 후 창을 닫으면 headless로 쿠키를 읽음
//   node scripts/spike_web_session.mjs headless  같은 프로필을 headless(일반 UA)로 claude.ai/와 함께 열어 쿠키 재확인
//
// 공통: 쿠키를 받은 뒤 Node fetch로 엔드포인트를 탐침하고, 같은 sessionKey로
// Rust(ureq) #[ignore] 실측 테스트(src-tauri/tests/live_web_session.rs)를 돌립니다.
// 쿠키 값·이메일·조직 이름은 어떤 경우에도 출력하지 않습니다. 상태 코드와 키 이름만 봅니다.
//
// 2026-10-12 실측: --remote-debugging-port를 연 창에 CDP를 붙여 기다리면 claude.ai의 Cloudflare
// 체크가 끝없이 반복돼 로그인할 수 없었습니다. 그래서 로그인 창은 포트 없이 띄웁니다.

import { spawn, spawnSync } from "node:child_process";
import { existsSync, mkdirSync, readdirSync, readFileSync, rmSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const MODE = process.argv[2] ?? "login";
const ORIGIN = "https://claude.ai";
const PROFILE = join(process.env.LOCALAPPDATA ?? "", "ClaudeUsageWidget", "login-profile");
const CF_COOKIES = new Set(["cf_clearance", "__cf_bm", "_cfuvid", "__cflb"]);
const LOGIN_TIMEOUT_MS = 10 * 60_000;
const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const log = (...a) => console.log(...a);

function findBrowser() {
  if (process.env.CUW_BROWSER && existsSync(process.env.CUW_BROWSER)) return process.env.CUW_BROWSER;
  const pf = process.env.ProgramFiles ?? "C:\\Program Files";
  const pf86 = process.env["ProgramFiles(x86)"] ?? "C:\\Program Files (x86)";
  const lad = process.env.LOCALAPPDATA ?? "";
  const candidates = [
    join(pf, "Google", "Chrome", "Application", "chrome.exe"),
    join(pf86, "Google", "Chrome", "Application", "chrome.exe"),
    join(lad, "Google", "Chrome", "Application", "chrome.exe"),
    join(pf86, "Microsoft", "Edge", "Application", "msedge.exe"),
    join(pf, "Microsoft", "Edge", "Application", "msedge.exe"),
  ];
  const hit = candidates.find((p) => existsSync(p));
  if (!hit) throw new Error("Chrome/Edge를 찾지 못했습니다 (CUW_BROWSER로 지정 가능)");
  return hit;
}

/** Chrome은 exe 옆에 버전 이름 폴더를 둡니다. headless UA에서 HeadlessChrome을 지우는 데 씁니다. */
function browserMajor(exe) {
  const dirs = readdirSync(dirname(exe)).filter((d) => /^\d+\.\d+\.\d+\.\d+$/.test(d));
  dirs.sort((a, b) => Number(b.split(".")[0]) - Number(a.split(".")[0]));
  return dirs[0]?.split(".")[0] ?? "140";
}

function chromeUa(major) {
  return `Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/${major}.0.0.0 Safari/537.36`;
}

async function readDevToolsPort(timeoutMs, child) {
  const file = join(PROFILE, "DevToolsActivePort");
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (existsSync(file)) {
      const [port, path] = readFileSync(file, "utf8").split(/\r?\n/);
      if (port && path) return `ws://127.0.0.1:${port.trim()}${path.trim()}`;
    }
    if (child.exitCode !== null) {
      throw new Error("브라우저가 바로 종료됐습니다 (같은 프로필 창이 이미 떠 있을 수 있음)");
    }
    await sleep(100);
  }
  throw new Error("DevToolsActivePort가 생기지 않았습니다");
}

class Cdp {
  constructor(url) {
    this.ws = new WebSocket(url);
    this.seq = 0;
    this.pending = new Map();
    this.closed = false;
    this.ws.addEventListener("message", (ev) => {
      const msg = JSON.parse(ev.data);
      const p = msg.id && this.pending.get(msg.id);
      if (!p) return;
      this.pending.delete(msg.id);
      msg.error ? p.reject(new Error(msg.error.message)) : p.resolve(msg.result);
    });
    this.ws.addEventListener("close", () => {
      this.closed = true;
      for (const p of this.pending.values()) p.reject(new Error("closed"));
      this.pending.clear();
    });
  }
  open() {
    return new Promise((resolve, reject) => {
      this.ws.addEventListener("open", resolve, { once: true });
      this.ws.addEventListener("error", () => reject(new Error("ws error")), { once: true });
    });
  }
  send(method, params = {}) {
    const id = ++this.seq;
    this.ws.send(JSON.stringify({ id, method, params }));
    return new Promise((resolve, reject) => this.pending.set(id, { resolve, reject }));
  }
}

const isClaudeCookie = (c) => c.domain === "claude.ai" || c.domain.endsWith(".claude.ai");

async function claudeCookies(cdp) {
  const { cookies } = await cdp.send("Storage.getCookies");
  return cookies.filter((c) => isClaudeCookie(c) && !CF_COOKIES.has(c.name));
}

function describeCookie(c) {
  const exp = c.session || c.expires <= 0 ? "session" : new Date(c.expires * 1000).toISOString();
  return `${c.name} (session=${c.session}, expires=${exp}, httpOnly=${c.httpOnly}, secure=${c.secure}, len=${c.value.length})`;
}

// --- 응답 요약 (값 대신 모양만) -----------------------------------------------

const SENSITIVE_KEY = /(email|phone|token|secret|password|^name$|full_name|account_name|^uuid$|_uuid$|^id$)/i;
const ISO = /^\d{4}-\d{2}-\d{2}T[\d:.]+(Z|[+-]\d{2}:?\d{2})?$/;

function shape(value, key = "", depth = 0) {
  if (value === null) return "null";
  if (typeof value === "number" || typeof value === "boolean") return JSON.stringify(value);
  if (typeof value === "string") {
    if (SENSITIVE_KEY.test(key) || value.includes("@")) return `<str len=${value.length}>`;
    if (ISO.test(value) || /^[a-z0-9_.\- ]{1,40}$/i.test(value)) return JSON.stringify(value);
    return `<str len=${value.length}>`;
  }
  if (Array.isArray(value)) {
    if (depth >= 3) return `[…${value.length}]`;
    return `[${value.length}] ${value.length ? shape(value[0], key, depth + 1) : ""}`;
  }
  if (depth >= 3) return `{${Object.keys(value).join(",")}}`;
  const parts = Object.entries(value).map(([k, v]) => `${k}: ${shape(v, k, depth + 1)}`);
  return `{ ${parts.join(", ")} }`;
}

function cfBlocked(status, headers, body) {
  return (
    headers.get("cf-mitigated") === "challenge" ||
    (status === 403 && /Just a moment|challenge-platform|cf-chl/i.test(body))
  );
}

async function nodeGet(url, cookieHeader, ua) {
  const res = await fetch(url, {
    headers: { Cookie: cookieHeader, "User-Agent": ua, Accept: "application/json" },
    redirect: "manual",
  });
  const body = await res.text();
  let json = null;
  try {
    json = JSON.parse(body);
  } catch {}
  const setCookies = (res.headers.getSetCookie?.() ?? []).map((s) => s.split("=")[0]);
  return { status: res.status, ct: res.headers.get("content-type") ?? "", cf: cfBlocked(res.status, res.headers, body), json, setCookies, len: body.length };
}

function pickOrg(orgs, cookies) {
  const last = cookies.find((c) => c.name === "lastActiveOrg")?.value;
  const byCookie = last && orgs.find((o) => o.uuid === last);
  if (byCookie) return { org: byCookie, rule: "lastActiveOrg 쿠키" };
  const chat = orgs.find((o) => Array.isArray(o.capabilities) && o.capabilities.includes("chat"));
  if (chat) return { org: chat, rule: "capabilities에 chat" };
  return { org: orgs[0], rule: "첫 번째" };
}

async function probeNode(cookies, ua) {
  const sk = cookies.find((c) => c.name === "sessionKey");
  const variants = [
    ["sessionKey만", `sessionKey=${sk.value}`],
    ["claude.ai 쿠키 전체(CF 제외)", cookies.map((c) => `${c.name}=${c.value}`).join("; ")],
  ];
  let orgs = null;
  for (const [label, header] of variants) {
    const r = await nodeGet(`${ORIGIN}/api/organizations`, header, ua);
    log(`  [Node fetch · ${label}] GET /api/organizations → ${r.status} ${r.ct.split(";")[0]} cf=${r.cf} setCookie=[${r.setCookies}]`);
    if (r.status === 200 && Array.isArray(r.json) && !orgs) orgs = r.json;
  }
  if (!orgs) return null;

  log(`  조직 ${orgs.length}개`);
  orgs.forEach((o, i) =>
    log(`    #${i} caps=${JSON.stringify(o.capabilities ?? null)} raven_type=${o.raven_type ?? "-"} billing_type=${o.billing_type ?? "-"} rate_limit_tier=${o.rate_limit_tier ?? "-"} keys=${Object.keys(o).length}`),
  );
  const { org, rule } = pickOrg(orgs, cookies);
  log(`  선택: #${orgs.indexOf(org)} (${rule})`);

  const header = `sessionKey=${sk.value}`;
  const paths = ["usage", "rate_limits", "usage_limits", "claude_code_usage", "billing/usage"];
  for (const p of paths) {
    const r = await nodeGet(`${ORIGIN}/api/organizations/${org.uuid}/${p}`, header, ua);
    log(`  [Node fetch] GET /api/organizations/{org}/${p} → ${r.status} cf=${r.cf} len=${r.len}`);
    if (r.status === 200 && r.json) log(`      ${shape(r.json)}`);
    if (p === "usage" && Array.isArray(r.json?.limits)) {
      r.json.limits.forEach((l, i) => log(`      limits[${i}] ${shape(l)}`));
    }
  }
  // 설정 창 계정 표시용 후보. 키 이름만 봅니다.
  for (const p of ["/api/account", "/api/bootstrap"]) {
    const r = await nodeGet(`${ORIGIN}${p}`, header, ua);
    const top = r.json && typeof r.json === "object" ? Object.keys(r.json).join(", ") : "-";
    log(`  [Node fetch] GET ${p} → ${r.status} keys=[${top}]`);
    const acct = r.json?.account ?? (p === "/api/account" ? r.json : null);
    if (acct && typeof acct === "object") log(`      account keys=[${Object.keys(acct).join(", ")}]`);
  }
  // 조직이 여러 개면 나머지에서도 usage가 되는지 봅니다(선택 규칙 판단용).
  for (const o of orgs) {
    if (o === org) continue;
    const r = await nodeGet(`${ORIGIN}/api/organizations/${o.uuid}/usage`, header, ua);
    log(`  [Node fetch] 다른 조직 #${orgs.indexOf(o)} /usage → ${r.status}`);
  }
  return org.uuid;
}

function runRust(sessionKey, orgUuid, ua) {
  log("\n=== Rust(ureq) 실측 테스트 ===");
  const r = spawnSync(
    "cargo",
    ["test", "--manifest-path", join(ROOT, "src-tauri", "Cargo.toml"), "--test", "live_web_session", "--", "--ignored", "--nocapture"],
    {
      stdio: "inherit",
      env: { ...process.env, CUW_SPIKE_SESSION_KEY: sessionKey, CUW_SPIKE_ORG: orgUuid ?? "", CUW_SPIKE_UA: ua },
      shell: false,
    },
  );
  log(`  cargo test 종료 코드: ${r.status}`);
}

async function launch(exe, extraArgs) {
  mkdirSync(PROFILE, { recursive: true });
  rmSync(join(PROFILE, "DevToolsActivePort"), { force: true });
  const args = [
    `--user-data-dir=${PROFILE}`,
    "--remote-debugging-port=0",
    "--no-first-run",
    "--no-default-browser-check",
    ...extraArgs,
  ];
  const child = spawn(exe, args, { stdio: "ignore" });
  const wsUrl = await readDevToolsPort(15_000, child);
  const cdp = new Cdp(wsUrl);
  await cdp.open();
  return { child, cdp };
}

async function closeBrowser(cdp, child) {
  if (!cdp.closed) await cdp.send("Browser.close").catch(() => {});
  for (let i = 0; i < 50 && child.exitCode === null; i++) await sleep(100);
}

/**
 * 디버깅 포트 없이 평범한 창을 띄워 사용자가 로그인하고 창을 닫으면,
 * 같은 프로필을 headless + 포트로 다시 열어 쿠키만 읽습니다. 로그인 동안 자동화 흔적이 전혀 없습니다.
 */
async function loginMode(exe, ua) {
  mkdirSync(PROFILE, { recursive: true });
  log(`프로필: ${PROFILE}`);
  const child = spawn(
    exe,
    [`--user-data-dir=${PROFILE}`, "--no-first-run", "--no-default-browser-check", "--new-window", `${ORIGIN}/login`],
    { stdio: "ignore" },
  );
  log("포트 없는 로그인 창을 띄웠습니다. 로그인을 마친 뒤 창을 닫으면 쿠키를 읽습니다 (최대 10분).");
  const started = Date.now();
  let lastNote = Date.now();
  while (child.exitCode === null && Date.now() - started < LOGIN_TIMEOUT_MS) {
    if (Date.now() - lastNote > 30_000) {
      lastNote = Date.now();
      log(`  창이 닫히길 기다리는 중… (${Math.round((LOGIN_TIMEOUT_MS - (Date.now() - started)) / 1000)}초 남음)`);
    }
    await sleep(500);
  }
  if (child.exitCode === null) {
    log("시간 초과. 창을 닫지 않았습니다.");
    child.kill();
    return null;
  }
  log(`창 닫힘 감지(${Math.round((Date.now() - started) / 1000)}초). headless로 쿠키를 읽습니다.`);
  return headlessMode(exe, ua, "about:blank");
}

async function headlessMode(exe, ua, url = `${ORIGIN}/`) {
  const { child, cdp } = await launch(exe, ["--headless=new", `--user-agent=${ua}`, url]);
  const ver = await cdp.send("Browser.getVersion");
  log(`브라우저: ${ver.product} (UA 덮어씀: ${!/Headless/.test(ua)})`);
  let cookies = [];
  const before = await claudeCookies(cdp).catch(() => []);
  const beforeExp = before.find((c) => c.name === "sessionKey")?.expires;
  // 홈 로드가 쿠키를 갱신하는지 보려고 몇 초 기다립니다.
  for (let i = 0; i < 10; i++) {
    await sleep(2000);
    cookies = await claudeCookies(cdp).catch(() => cookies);
  }
  const after = cookies.find((c) => c.name === "sessionKey");
  log(`  sessionKey 만료: 시작 ${beforeExp ? new Date(beforeExp * 1000).toISOString() : "-"} → 20초 후 ${after ? new Date(after.expires * 1000).toISOString() : "-"}`);
  await closeBrowser(cdp, child);
  return cookies;
}

async function main() {
  const exe = findBrowser();
  const ua = chromeUa(browserMajor(exe));
  log(`브라우저 경로: ${exe}`);
  log(`일반 UA 주 버전: ${browserMajor(exe)}`);

  const modes = { login: loginMode, headless: headlessMode };
  if (!modes[MODE]) throw new Error(`모드: ${Object.keys(modes).join(" | ")}`);
  const cookies = await modes[MODE](exe, ua);
  if (!cookies) process.exit(2);

  log("\n=== claude.ai 쿠키 (CF 제외, 값 미출력) ===");
  for (const c of cookies) log(`  ${describeCookie(c)}`);
  const sk = cookies.find((c) => c.name === "sessionKey");
  if (!sk) {
    log("sessionKey 없음 → 로그인 상태 아님");
    process.exit(3);
  }

  log("\n=== Node fetch 탐침 (일반 Chrome UA, CF 쿠키 없이) ===");
  const orgUuid = await probeNode(cookies, ua);

  runRust(sk.value, orgUuid, ua);
  log("\nDONE");
}

main().catch((err) => {
  log(`실패: ${err.message}`);
  process.exit(1);
});
