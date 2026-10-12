//! 위젯 전용 Chrome 프로필로 사용자가 직접 로그인하고, 쿠키만 받아 옵니다.
//!
//! 평소 Chrome 프로필은 읽지도 건드리지도 않습니다. 위젯 전용 user-data-dir를 쓰고,
//! 브라우저 수준 CDP 명령(`Storage.getCookies`, `Browser.close`)만 씁니다. 페이지 타깃에는
//! 붙지 않습니다.
//!
//! 로그인 흐름 (claude.ai Phase 0 실측으로 정함):
//! 1. 디버깅 포트 **없이** 평범한 창을 띄운다. 포트를 열고 CDP를 붙인 창에서는 claude.ai의
//!    Cloudflare 체크가 끝없이 반복돼 로그인할 수 없었다.
//! 2. 사용자가 로그인을 마치고 창을 닫으면(프로세스 종료) 같은 프로필을 headless + 포트로
//!    다시 띄워 쿠키를 읽고 `validate`로 확인한다.
//!
//! 무인 갱신은 같은 프로필을 headless(일반 Chrome UA)로 `home_url`과 함께 띄워 쿠키를 다시
//! 읽습니다. UA에 `HeadlessChrome`이 남으면 Cloudflare가 무한 챌린지합니다.
//! 쿠키 값은 로그·오류 문구에 넣지 않습니다.

use std::ffi::OsString;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

/// 시험용으로 브라우저 경로를 직접 지정합니다.
pub const BROWSER_OVERRIDE_ENV: &str = "WIDGET_LOGIN_BROWSER";

/// Cloudflare 쿠키는 브라우저 지문에 묶여 있어 옮기면 오히려 막힙니다.
const CLOUDFLARE_COOKIES: &[&str] = &["cf_clearance", "__cf_bm", "_cfuvid", "__cflb"];

const LOGIN_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const STATUS_EVERY: Duration = Duration::from_secs(15);
const DEVTOOLS_PORT_TIMEOUT: Duration = Duration::from_secs(15);
/// 로그인 창을 닫은 직후에는 쿠키가 이미 디스크에 있어 곧바로 읽힙니다.
const AFTER_LOGIN_READ_TIMEOUT: Duration = Duration::from_secs(4);
const REFRESH_READ_TIMEOUT: Duration = Duration::from_secs(20);
const REFRESH_MIN_INTERVAL: Duration = Duration::from_secs(30 * 60);
const POLL_EVERY: Duration = Duration::from_millis(1500);
/// 이보다 빨리 끝난 실행은 이미 떠 있던 같은 프로필 창에 넘겨진 것으로 봅니다.
const HANDOFF_EXIT: Duration = Duration::from_secs(3);
/// 버전 폴더를 못 찾았을 때 UA에 쓸 주 버전입니다.
const FALLBACK_MAJOR: u32 = 150;

/// 로그인 창과 무인 갱신은 같은 프로필을 쓰므로 동시에 띄울 수 없습니다.
static BROWSER_LOCK: Mutex<()> = Mutex::new(());
static LAST_REFRESH: Mutex<Option<Instant>> = Mutex::new(None);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    #[serde(default = "root_path")]
    pub path: String,
    /// 유닉스 초. 세션 쿠키는 0 이하입니다.
    #[serde(default)]
    pub expires: f64,
    #[serde(default)]
    pub http_only: bool,
    #[serde(default)]
    pub secure: bool,
    #[serde(default)]
    pub session: bool,
}

fn root_path() -> String {
    "/".into()
}

/// 표시용 계정 정보입니다.
#[derive(Debug, Clone, Default)]
pub struct Account {
    pub label: Option<String>,
}

/// 서비스별 차이는 이것 하나로 넘깁니다.
pub struct LoginSpec {
    pub login_url: &'static str,
    pub home_url: &'static str,
    pub cookie_domain: &'static str,
    /// 이 중 하나라도 있으면 로그인 쿠키가 생긴 것으로 보고 `validate`를 부릅니다.
    pub required_cookies: &'static [&'static str],
    /// 쿠키가 쓸 만한지 평문 HTTP로 확인합니다. 거부면 `LoginError::Unauthorized`.
    pub validate: fn(&[Cookie]) -> Result<Account, LoginError>,
}

pub struct Captured {
    pub cookies: Vec<Cookie>,
    pub account: Account,
}

#[derive(Debug, Error)]
pub enum LoginError {
    #[error("Chrome 또는 Edge를 찾지 못했습니다")]
    BrowserNotFound,
    #[error("위젯 로그인 창이 이미 열려 있습니다. 그 창을 닫은 뒤 다시 시도하세요")]
    ProfileInUse,
    #[error("다른 로그인이나 갱신이 진행 중입니다")]
    Busy,
    #[error("최근에 이미 갱신을 시도했습니다")]
    RateLimited,
    #[error("브라우저를 실행하지 못했습니다: {0}")]
    Launch(String),
    #[error("브라우저 연결에 실패했습니다: {0}")]
    Devtools(String),
    #[error("로그인이 확인되지 않았습니다. 창에서 로그인을 마친 뒤 닫아 주세요")]
    NotLoggedIn,
    #[error("{0}분 안에 창이 닫히지 않아 중단했습니다")]
    Timeout(u64),
    #[error("세션이 거부됐습니다")]
    Unauthorized,
    #[error("세션 확인 실패: {0}")]
    Validate(String),
    #[error("파일 작업 실패: {0}")]
    Io(String),
}

// --- 브라우저 찾기 -----------------------------------------------------------

fn app_path_from_registry(exe_name: &str) -> Vec<PathBuf> {
    use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
    use winreg::RegKey;

    let sub = format!(r"SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths\{exe_name}");
    [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE]
        .into_iter()
        .filter_map(|hive| RegKey::predef(hive).open_subkey(&sub).ok())
        .filter_map(|key| key.get_value::<String, _>("").ok())
        .map(|raw| PathBuf::from(raw.trim_matches('"')))
        .collect()
}

/// 찾는 순서: 환경변수 override → App Paths(chrome) → 설치 폴더(chrome) → App Paths(edge) → 설치 폴더(edge).
fn browser_candidates(
    env: impl Fn(&str) -> Option<OsString>,
    app_paths: impl Fn(&str) -> Vec<PathBuf>,
) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(p) = env(BROWSER_OVERRIDE_ENV).filter(|p| !p.is_empty()) {
        out.push(PathBuf::from(p));
    }
    let under = |var: &str, rel: &[&str]| {
        env(var).map(|base| rel.iter().fold(PathBuf::from(base), |p, seg| p.join(seg)))
    };

    out.extend(app_paths("chrome.exe"));
    let chrome = ["Google", "Chrome", "Application", "chrome.exe"];
    for var in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
        out.extend(under(var, &chrome));
    }

    out.extend(app_paths("msedge.exe"));
    let edge = ["Microsoft", "Edge", "Application", "msedge.exe"];
    for var in ["ProgramFiles(x86)", "ProgramFiles"] {
        out.extend(under(var, &edge));
    }
    out
}

pub fn find_browser() -> Option<PathBuf> {
    browser_candidates(|k| std::env::var_os(k), app_path_from_registry)
        .into_iter()
        .find(|p| p.is_file())
}

/// Chrome·Edge는 exe 옆에 `154.0.8037.98` 같은 버전 폴더를 둡니다. 가장 높은 것의 주 버전.
pub fn browser_major_version(exe: &Path) -> Option<u32> {
    let dir = exe.parent()?;
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let parts: Vec<u32> = name
                .split('.')
                .map(|s| s.parse().ok())
                .collect::<Option<_>>()?;
            (parts.len() == 4).then_some(parts)
        })
        .max()
        .map(|parts| parts[0])
}

/// headless 기본 UA에는 `HeadlessChrome`이 들어가 Cloudflare가 막습니다. 일반 Chrome UA로 바꿉니다.
pub fn normal_user_agent(major: u32) -> String {
    format!(
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/{major}.0.0.0 Safari/537.36"
    )
}

// --- 프로필 ------------------------------------------------------------------

/// Chrome은 실행 중에만 프로필의 `lockfile`을 공유 없이 열어 둡니다(종료 시 삭제).
pub fn profile_in_use(profile: &Path) -> bool {
    let lock = profile.join("lockfile");
    if !lock.exists() {
        return false;
    }
    match std::fs::OpenOptions::new().write(true).open(&lock) {
        Ok(_) => false,
        // ERROR_SHARING_VIOLATION
        Err(e) => e.raw_os_error() == Some(32),
    }
}

/// 로그인 프로필을 통째로 지웁니다. 프로필 안에도 살아 있는 쿠키가 있어서 연결 해제와 별개입니다.
pub fn wipe_profile(profile: &Path) -> Result<(), LoginError> {
    let _guard = BROWSER_LOCK.try_lock().map_err(|_| LoginError::Busy)?;
    if profile_in_use(profile) {
        return Err(LoginError::ProfileInUse);
    }
    match std::fs::remove_dir_all(profile) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(LoginError::Io(e.to_string())),
        _ => Ok(()),
    }
}

// --- CDP ---------------------------------------------------------------------

/// `DevToolsActivePort` 첫 줄은 포트, 둘째 줄은 브라우저 웹소켓 경로입니다.
pub fn parse_devtools_active_port(text: &str) -> Option<String> {
    let mut lines = text.lines();
    let port: u16 = lines.next()?.trim().parse().ok()?;
    let path = lines.next()?.trim();
    (port != 0 && path.starts_with("/devtools/browser/"))
        .then(|| format!("ws://127.0.0.1:{port}{path}"))
}

fn wait_devtools_url(profile: &Path, child: &mut Child) -> Result<String, LoginError> {
    let file = profile.join("DevToolsActivePort");
    let deadline = Instant::now() + DEVTOOLS_PORT_TIMEOUT;
    while Instant::now() < deadline {
        if let Some(url) = std::fs::read_to_string(&file)
            .ok()
            .as_deref()
            .and_then(parse_devtools_active_port)
        {
            return Ok(url);
        }
        if let Ok(Some(_)) = child.try_wait() {
            // 같은 프로필 Chrome이 이미 떠 있으면 새 실행이 그쪽에 넘기고 바로 끝납니다.
            return Err(LoginError::ProfileInUse);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(LoginError::Devtools(
        "DevToolsActivePort가 생기지 않았습니다".into(),
    ))
}

struct Cdp {
    ws: WebSocket<MaybeTlsStream<TcpStream>>,
    next_id: u64,
}

impl Cdp {
    fn connect(url: &str) -> Result<Self, LoginError> {
        let (ws, _) = tungstenite::connect(url).map_err(|e| LoginError::Devtools(e.to_string()))?;
        if let MaybeTlsStream::Plain(stream) = ws.get_ref() {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
        }
        Ok(Self { ws, next_id: 0 })
    }

    fn call(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, LoginError> {
        self.next_id += 1;
        let id = self.next_id;
        let msg = serde_json::json!({ "id": id, "method": method, "params": params });
        self.ws
            .send(Message::text(msg.to_string()))
            .map_err(|e| LoginError::Devtools(e.to_string()))?;
        loop {
            let text = match self
                .ws
                .read()
                .map_err(|e| LoginError::Devtools(e.to_string()))?
            {
                Message::Text(t) => t,
                Message::Close(_) => return Err(LoginError::Devtools("연결이 닫혔습니다".into())),
                _ => continue,
            };
            let Ok(reply) = serde_json::from_str::<serde_json::Value>(text.as_str()) else {
                continue;
            };
            if reply.get("id").and_then(|v| v.as_u64()) != Some(id) {
                continue;
            }
            if let Some(err) = reply.get("error") {
                let message = err
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("CDP 오류");
                return Err(LoginError::Devtools(message.to_string()));
            }
            return Ok(reply.get("result").cloned().unwrap_or_default());
        }
    }

    fn cookies(&mut self) -> Result<Vec<Cookie>, LoginError> {
        let result = self.call("Storage.getCookies", serde_json::json!({}))?;
        let raw = result.get("cookies").cloned().unwrap_or_default();
        serde_json::from_value(raw).map_err(|e| LoginError::Devtools(e.to_string()))
    }

    fn close_browser(&mut self) {
        let _ = self.call("Browser.close", serde_json::json!({}));
    }
}

/// 위젯이 띄운 프로세스만 정리합니다. 정상 종료를 잠깐 기다리고 남아 있으면 끝냅니다.
fn reap(child: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Ok(Some(_)) = child.try_wait() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
}

// --- 쿠키 --------------------------------------------------------------------

/// 해당 도메인(하위 도메인 포함) 쿠키만 남기고 Cloudflare 쿠키는 버립니다.
pub fn filter_cookies(cookies: Vec<Cookie>, domain: &str) -> Vec<Cookie> {
    let suffix = format!(".{domain}");
    cookies
        .into_iter()
        .filter(|c| {
            let d = c.domain.trim_start_matches('.');
            d == domain || d.ends_with(&suffix)
        })
        .filter(|c| !CLOUDFLARE_COOKIES.contains(&c.name.as_str()))
        .collect()
}

fn has_required(cookies: &[Cookie], required: &[&str]) -> bool {
    cookies
        .iter()
        .any(|c| !c.value.is_empty() && required.contains(&c.name.as_str()))
}

/// 같은 프로필을 headless + 디버깅 포트로 띄워 쿠키를 읽고 `validate`가 통과하면 돌려줍니다.
fn read_cookies_headless(
    spec: &LoginSpec,
    profile: &Path,
    exe: &Path,
    url: &str,
    timeout: Duration,
) -> Result<Captured, LoginError> {
    let ua = normal_user_agent(browser_major_version(exe).unwrap_or(FALLBACK_MAJOR));
    let _ = std::fs::remove_file(profile.join("DevToolsActivePort"));

    let mut child = Command::new(exe)
        .arg(format!("--user-data-dir={}", profile.display()))
        .args([
            "--headless=new",
            "--remote-debugging-port=0",
            "--no-first-run",
            "--no-default-browser-check",
        ])
        .arg(format!("--user-agent={ua}"))
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| LoginError::Launch(e.to_string()))?;

    let result = wait_devtools_url(profile, &mut child).and_then(|ws_url| {
        let mut cdp = Cdp::connect(&ws_url)?;
        let outcome = poll_until_valid(spec, &mut cdp, timeout);
        cdp.close_browser();
        outcome
    });
    reap(&mut child);
    result
}

fn poll_until_valid(
    spec: &LoginSpec,
    cdp: &mut Cdp,
    timeout: Duration,
) -> Result<Captured, LoginError> {
    let deadline = Instant::now() + timeout;
    let mut last = LoginError::NotLoggedIn;
    loop {
        let cookies = filter_cookies(cdp.cookies()?, spec.cookie_domain);
        if has_required(&cookies, spec.required_cookies) {
            match (spec.validate)(&cookies) {
                Ok(account) => return Ok(Captured { cookies, account }),
                // 페이지가 쿠키를 고칠 수 있으니 시간이 남았으면 다시 봅니다.
                Err(LoginError::Unauthorized) => last = LoginError::Unauthorized,
                Err(other) => return Err(other),
            }
        }
        if Instant::now() + POLL_EVERY > deadline {
            return Err(last);
        }
        std::thread::sleep(POLL_EVERY);
    }
}

// --- 공개 흐름 ---------------------------------------------------------------

/// 창을 띄워 사용자가 직접 로그인하게 하고, 창이 닫히면 쿠키를 읽어 확인합니다.
/// 비밀번호·인증 코드 입력은 전부 사용자가 합니다.
pub fn interactive_login(
    spec: &LoginSpec,
    profile: &Path,
    on_status: &dyn Fn(&str),
) -> Result<Captured, LoginError> {
    let _guard = BROWSER_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let exe = find_browser().ok_or(LoginError::BrowserNotFound)?;
    if profile_in_use(profile) {
        return Err(LoginError::ProfileInUse);
    }
    std::fs::create_dir_all(profile).map_err(|e| LoginError::Io(e.to_string()))?;

    let mut child = Command::new(&exe)
        .arg(format!("--user-data-dir={}", profile.display()))
        .args([
            "--no-first-run",
            "--no-default-browser-check",
            "--new-window",
        ])
        .arg(spec.login_url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| LoginError::Launch(e.to_string()))?;

    on_status("브라우저 창에서 로그인한 뒤, 창을 직접 닫아 주세요.");
    let started = Instant::now();
    let mut last_status = started;
    loop {
        if let Ok(Some(_)) = child.try_wait() {
            break;
        }
        let elapsed = started.elapsed();
        if elapsed >= LOGIN_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            return Err(LoginError::Timeout(LOGIN_TIMEOUT.as_secs() / 60));
        }
        if last_status.elapsed() >= STATUS_EVERY {
            last_status = Instant::now();
            let left = (LOGIN_TIMEOUT - elapsed).as_secs();
            on_status(&format!(
                "로그인을 마치면 창을 닫아 주세요. ({}분 {}초 남음)",
                left / 60,
                left % 60
            ));
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    if started.elapsed() < HANDOFF_EXIT {
        return Err(LoginError::ProfileInUse);
    }

    on_status("로그인 정보를 확인하는 중…");
    read_cookies_headless(spec, profile, &exe, "about:blank", AFTER_LOGIN_READ_TIMEOUT).map_err(
        |e| match e {
            LoginError::Unauthorized => LoginError::NotLoggedIn,
            other => other,
        },
    )
}

/// 무인 갱신. 로그인 창이 떠 있거나 30분 안에 시도했으면 하지 않습니다.
pub fn headless_refresh(spec: &LoginSpec, profile: &Path) -> Result<Captured, LoginError> {
    let _guard = BROWSER_LOCK.try_lock().map_err(|_| LoginError::Busy)?;
    {
        let mut last = LAST_REFRESH.lock().unwrap_or_else(|e| e.into_inner());
        if last.is_some_and(|t| t.elapsed() < REFRESH_MIN_INTERVAL) {
            return Err(LoginError::RateLimited);
        }
        *last = Some(Instant::now());
    }
    if !profile.is_dir() {
        return Err(LoginError::NotLoggedIn);
    }
    if profile_in_use(profile) {
        return Err(LoginError::ProfileInUse);
    }
    let exe = find_browser().ok_or(LoginError::BrowserNotFound)?;
    read_cookies_headless(spec, profile, &exe, spec.home_url, REFRESH_READ_TIMEOUT)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cookie(name: &str, domain: &str) -> Cookie {
        Cookie {
            name: name.into(),
            value: "v".into(),
            domain: domain.into(),
            path: "/".into(),
            expires: 0.0,
            http_only: false,
            secure: true,
            session: true,
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("cuw-browser-login-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn filter_keeps_domain_and_drops_cloudflare() {
        let input = vec![
            cookie("sessionKey", ".claude.ai"),
            cookie("lastActiveOrg", "claude.ai"),
            cookie("x", "api.claude.ai"),
            cookie("cf_clearance", ".claude.ai"),
            cookie("__cf_bm", ".claude.ai"),
            cookie("_cfuvid", ".claude.ai"),
            cookie("__cflb", "claude.ai"),
            cookie("sessionKey", "notclaude.ai"),
            cookie("NID", ".google.com"),
        ];
        let names: Vec<_> = filter_cookies(input, "claude.ai")
            .into_iter()
            .map(|c| format!("{}@{}", c.name, c.domain))
            .collect();
        assert_eq!(
            names,
            [
                "sessionKey@.claude.ai",
                "lastActiveOrg@claude.ai",
                "x@api.claude.ai"
            ]
        );
    }

    #[test]
    fn required_cookie_needs_a_value() {
        let mut empty = cookie("sessionKey", "claude.ai");
        empty.value.clear();
        assert!(!has_required(&[empty], &["sessionKey"]));
        assert!(has_required(
            &[cookie("V2_WS_AID", "flex.team")],
            &["AID", "V2_WS_AID"]
        ));
        assert!(!has_required(
            &[cookie("other", "claude.ai")],
            &["sessionKey"]
        ));
    }

    #[test]
    fn cdp_cookie_json_parses() {
        let raw = serde_json::json!([{
            "name": "sessionKey", "value": "secret", "domain": ".claude.ai", "path": "/",
            "expires": 1794195670.9, "size": 141, "httpOnly": true, "secure": true,
            "session": false, "sameSite": "Lax", "priority": "Medium"
        }]);
        let parsed: Vec<Cookie> = serde_json::from_value(raw).unwrap();
        assert!(parsed[0].http_only && !parsed[0].session);
        assert_eq!(parsed[0].expires as i64, 1_794_195_670);
    }

    #[test]
    fn devtools_active_port_parsing() {
        assert_eq!(
            parse_devtools_active_port("51234\n/devtools/browser/abc-123\n").as_deref(),
            Some("ws://127.0.0.1:51234/devtools/browser/abc-123")
        );
        assert_eq!(
            parse_devtools_active_port("51234\r\n/devtools/browser/x").as_deref(),
            Some("ws://127.0.0.1:51234/devtools/browser/x")
        );
        assert!(parse_devtools_active_port("").is_none());
        assert!(parse_devtools_active_port("0\n/devtools/browser/x").is_none());
        assert!(parse_devtools_active_port("abc\n/devtools/browser/x").is_none());
        assert!(parse_devtools_active_port("51234\n/devtools/page/x").is_none());
        assert!(parse_devtools_active_port("51234").is_none());
    }

    #[test]
    fn user_agent_is_plain_chrome() {
        let ua = normal_user_agent(154);
        assert!(ua.contains("Chrome/154.0.0.0"));
        assert!(!ua.contains("Headless"));
        assert!(ua.starts_with("Mozilla/5.0 (Windows NT 10.0; Win64; x64)"));
    }

    #[test]
    fn major_version_from_sibling_folders() {
        let dir = temp_dir("version");
        for d in ["153.0.7000.1", "154.0.8037.98", "SetupMetrics", "99.1"] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
        }
        std::fs::write(dir.join("chrome.exe"), b"").unwrap();
        assert_eq!(browser_major_version(&dir.join("chrome.exe")), Some(154));

        let empty = temp_dir("version-empty");
        std::fs::write(empty.join("chrome.exe"), b"").unwrap();
        assert_eq!(browser_major_version(&empty.join("chrome.exe")), None);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&empty);
    }

    #[test]
    fn browser_search_order() {
        let env = |k: &str| -> Option<OsString> {
            match k {
                BROWSER_OVERRIDE_ENV => Some(r"D:\portable\chrome.exe".into()),
                "ProgramFiles" => Some(r"C:\PF".into()),
                "ProgramFiles(x86)" => Some(r"C:\PF86".into()),
                "LOCALAPPDATA" => Some(r"C:\LAD".into()),
                _ => None,
            }
        };
        let app_paths = |exe: &str| vec![PathBuf::from(format!(r"C:\Reg\{exe}"))];
        let got: Vec<String> = browser_candidates(env, app_paths)
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        assert_eq!(
            got,
            [
                r"D:\portable\chrome.exe",
                r"C:\Reg\chrome.exe",
                r"C:\PF\Google\Chrome\Application\chrome.exe",
                r"C:\PF86\Google\Chrome\Application\chrome.exe",
                r"C:\LAD\Google\Chrome\Application\chrome.exe",
                r"C:\Reg\msedge.exe",
                r"C:\PF86\Microsoft\Edge\Application\msedge.exe",
                r"C:\PF\Microsoft\Edge\Application\msedge.exe",
            ]
        );
    }

    #[test]
    fn browser_search_skips_missing_env() {
        let got = browser_candidates(|_| None, |_| Vec::new());
        assert!(got.is_empty());
    }

    #[test]
    fn unlocked_or_missing_lockfile_is_not_in_use() {
        let dir = temp_dir("lock");
        assert!(!profile_in_use(&dir));
        std::fs::write(dir.join("lockfile"), b"").unwrap();
        assert!(!profile_in_use(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
