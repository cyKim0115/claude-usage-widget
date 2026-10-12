//! claude.ai 웹 세션 소스입니다.
//!
//! 위젯 전용 Chrome 프로필로 사용자가 로그인해 받은 쿠키(`sessionKey`)로
//! `GET /api/organizations` → `GET /api/organizations/{uuid}/usage`를 평문 HTTP로 부릅니다.
//! 비공식 엔드포인트라 언제든 깨질 수 있습니다. 쿠키 값·이메일은 로그·오류 문구에 넣지 않습니다.

use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::browser_login::{self, Account, Captured, Cookie, LoginError, LoginSpec};
use crate::install::install_dir;
use crate::secure_store;
use crate::usage::{
    fetch_error, need_login, plan_label_from, tracks_from_usage, UsageSnapshot, SOURCE_WEB,
};

const ORIGIN: &str = "https://claude.ai";
const SESSION_COOKIE: &str = "sessionKey";
/// claude.ai가 마지막으로 연 조직 uuid를 담아 둡니다. 조직이 여러 개일 때 고르는 기준입니다.
const LAST_ORG_COOKIE: &str = "lastActiveOrg";
const CLOUDFLARE_COOKIES: &[&str] = &["cf_clearance", "__cf_bm", "_cfuvid", "__cflb"];

pub const SPEC: LoginSpec = LoginSpec {
    login_url: "https://claude.ai/login",
    home_url: "https://claude.ai/",
    cookie_domain: "claude.ai",
    required_cookies: &[SESSION_COOKIE],
    validate,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebSession {
    pub cookies: Vec<Cookie>,
    /// 유닉스 밀리초.
    pub saved_at: i64,
    /// 표시용 이메일.
    pub account: Option<String>,
}

impl WebSession {
    fn from_captured(captured: Captured) -> Self {
        Self {
            cookies: captured.cookies,
            saved_at: now_ms(),
            account: captured.account.label,
        }
    }

    /// `sessionKey` 만료(유닉스 밀리초). 세션 쿠키면 없음.
    fn expires_at_ms(&self) -> Option<i64> {
        self.cookies
            .iter()
            .find(|c| c.name == SESSION_COOKIE && !c.session && c.expires > 0.0)
            .map(|c| (c.expires * 1000.0) as i64)
    }
}

#[derive(Debug, Error)]
pub enum WebError {
    #[error("웹 세션이 거부됐습니다 (HTTP {0})")]
    Unauthorized(u16),
    #[error("Cloudflare가 요청을 막았습니다")]
    Challenge,
    #[error("claude.ai 응답 오류 (HTTP {0})")]
    Status(u16),
    #[error("claude.ai에 연결하지 못했습니다 ({0})")]
    Transport(String),
    #[error("응답 해석 실패")]
    Parse,
    #[error("사용량을 볼 수 있는 조직이 없습니다")]
    NoOrg,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// --- 경로·저장 ---------------------------------------------------------------

pub fn profile_dir() -> Result<PathBuf, String> {
    Ok(install_dir()?.join("login-profile"))
}

fn session_path() -> Result<PathBuf, String> {
    Ok(install_dir()?.join("session.dat"))
}

enum Stored {
    Missing,
    /// 다른 Windows 계정이거나 손상. 지우지 않고 "로그인 필요"로만 둡니다.
    Unreadable,
    Present(WebSession),
}

fn load() -> Stored {
    let Ok(path) = session_path() else {
        return Stored::Unreadable;
    };
    match secure_store::read(&path) {
        Ok(None) => Stored::Missing,
        Ok(Some(bytes)) => serde_json::from_slice(&bytes)
            .map(Stored::Present)
            .unwrap_or(Stored::Unreadable),
        Err(_) => Stored::Unreadable,
    }
}

fn save(session: &WebSession) -> Result<(), String> {
    let bytes = serde_json::to_vec(session).map_err(|e| e.to_string())?;
    secure_store::write(&session_path()?, &bytes).map_err(|e| format!("세션 저장 실패: {e}"))
}

pub fn has_session() -> bool {
    session_path().map(|p| p.is_file()).unwrap_or(false)
}

/// 연결 해제: 저장한 세션만 지웁니다. 로그인 프로필은 남아 있어 다시 로그인이 빠릅니다.
pub fn disconnect() -> Result<(), String> {
    secure_store::remove(&session_path()?).map_err(|e| format!("세션 삭제 실패: {e}"))
}

/// 로그인 프로필 초기화: 프로필 안에도 살아 있는 쿠키가 있어 프로필과 세션을 함께 지웁니다.
pub fn reset_profile() -> Result<(), String> {
    browser_login::wipe_profile(&profile_dir()?).map_err(|e| e.to_string())?;
    disconnect()
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebStatus {
    /// `connected` · `none` · `unreadable`
    pub state: &'static str,
    pub account: Option<String>,
    pub saved_at: Option<i64>,
    pub expires_at: Option<i64>,
}

pub fn status() -> WebStatus {
    match load() {
        Stored::Present(s) => WebStatus {
            state: "connected",
            expires_at: s.expires_at_ms(),
            saved_at: Some(s.saved_at),
            account: s.account,
        },
        Stored::Missing => WebStatus {
            state: "none",
            account: None,
            saved_at: None,
            expires_at: None,
        },
        Stored::Unreadable => WebStatus {
            state: "unreadable",
            account: None,
            saved_at: None,
            expires_at: None,
        },
    }
}

// --- HTTP --------------------------------------------------------------------

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(30))
        // 쿠키가 다른 호스트로 따라가지 않게 리디렉션을 끕니다.
        .redirects(0)
        .build()
}

/// claude.ai는 Cloudflare 뒤라 일반 Chrome UA를 씁니다. 설치된 Chrome 주 버전을 따릅니다.
fn user_agent() -> &'static str {
    static UA: OnceLock<String> = OnceLock::new();
    UA.get_or_init(|| {
        let major = browser_login::find_browser()
            .and_then(|exe| browser_login::browser_major_version(&exe))
            .unwrap_or(150);
        browser_login::normal_user_agent(major)
    })
}

fn now_secs() -> f64 {
    now_ms() as f64 / 1000.0
}

fn cookie_header(cookies: &[Cookie], now_s: f64) -> String {
    cookies
        .iter()
        .filter(|c| !c.value.is_empty())
        .filter(|c| c.session || c.expires <= 0.0 || c.expires > now_s)
        .map(|c| format!("{}={}", c.name, c.value))
        .collect::<Vec<_>>()
        .join("; ")
}

struct Reply {
    body: serde_json::Value,
    set_cookies: Vec<String>,
}

fn get(path: &str, cookies: &[Cookie]) -> Result<Reply, WebError> {
    let result = agent()
        .get(&format!("{ORIGIN}{path}"))
        .set("Cookie", &cookie_header(cookies, now_secs()))
        .set("User-Agent", user_agent())
        .set("Accept", "application/json")
        .call();
    let resp = match result {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        // 표시 문구에 URL이 섞이지 않게 종류만 남깁니다.
        Err(ureq::Error::Transport(t)) => return Err(WebError::Transport(t.kind().to_string())),
    };
    let status = resp.status();
    if resp.header("cf-mitigated") == Some("challenge") {
        return Err(WebError::Challenge);
    }
    match status {
        200 => {}
        // Cloudflare 차단 페이지(HTML 403)도 여기로 옵니다. 본문은 보여주지 않습니다.
        401 | 403 if resp.content_type() == "text/html" => return Err(WebError::Challenge),
        401 | 403 => return Err(WebError::Unauthorized(status)),
        other => return Err(WebError::Status(other)),
    }
    let set_cookies = resp
        .all("set-cookie")
        .into_iter()
        .map(str::to_string)
        .collect();
    let body = resp.into_json().map_err(|_| WebError::Parse)?;
    Ok(Reply { body, set_cookies })
}

// --- 조직 --------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct OrgEntry {
    uuid: String,
    #[serde(default)]
    capabilities: Vec<String>,
    rate_limit_tier: Option<String>,
}

/// 채팅 조직(`capabilities`에 `chat`) 중 `lastActiveOrg`를 먼저, 없으면 첫 채팅 조직.
/// API 전용 조직은 `/usage`가 403이라 채팅 조직이 하나라도 있으면 고르지 않습니다.
fn pick_org<'a>(orgs: &'a [OrgEntry], last_active: Option<&str>) -> Option<&'a OrgEntry> {
    let chat: Vec<&OrgEntry> = orgs
        .iter()
        .filter(|o| o.capabilities.iter().any(|c| c == "chat"))
        .collect();
    let pool: Vec<&OrgEntry> = if chat.is_empty() {
        orgs.iter().collect()
    } else {
        chat
    };
    last_active
        .and_then(|id| pool.iter().find(|o| o.uuid == id).copied())
        .or_else(|| pool.first().copied())
}

/// `capabilities`에 `claude_max`·`claude_pro` 같은 플랜 표식이 들어 있습니다.
fn org_plan(org: &OrgEntry) -> Option<String> {
    let org_type = org
        .capabilities
        .iter()
        .find(|c| c.starts_with("claude_"))
        .map(String::as_str);
    plan_label_from(org_type, org.rate_limit_tier.as_deref())
}

fn last_active(cookies: &[Cookie]) -> Option<&str> {
    cookies
        .iter()
        .find(|c| c.name == LAST_ORG_COOKIE)
        .map(|c| c.value.as_str())
}

fn organizations(cookies: &[Cookie]) -> Result<(Vec<OrgEntry>, Vec<String>), WebError> {
    let reply = get("/api/organizations", cookies)?;
    let orgs = serde_json::from_value(reply.body).map_err(|_| WebError::Parse)?;
    Ok((orgs, reply.set_cookies))
}

fn account_email(cookies: &[Cookie]) -> Option<String> {
    let reply = get("/api/account", cookies).ok()?;
    reply
        .body
        .get("email_address")?
        .as_str()
        .map(str::to_string)
}

/// 로그인 확인: organizations 200 + 사용량을 볼 조직이 있어야 합니다. 이메일은 부가 정보입니다.
fn validate(cookies: &[Cookie]) -> Result<Account, LoginError> {
    let (orgs, _) = organizations(cookies).map_err(|e| match e {
        WebError::Unauthorized(_) => LoginError::Unauthorized,
        other => LoginError::Validate(other.to_string()),
    })?;
    if pick_org(&orgs, last_active(cookies)).is_none() {
        return Err(LoginError::Validate(WebError::NoOrg.to_string()));
    }
    Ok(Account {
        label: account_email(cookies),
    })
}

// --- Set-Cookie --------------------------------------------------------------

const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];

/// 1970-01-01부터 날짜 수 (proleptic Gregorian).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `Mon, 09 Nov 2026 03:41:10 GMT` / `Mon, 09-Nov-2026 03:41:10 GMT` → 유닉스 초.
fn parse_http_date(raw: &str) -> Option<f64> {
    let rest = raw.split_once(',').map_or(raw, |(_, r)| r);
    let parts: Vec<&str> = rest.split([' ', '-']).filter(|p| !p.is_empty()).collect();
    let day: i64 = parts.first()?.parse().ok()?;
    let mon = parts.get(1)?.to_ascii_lowercase();
    let month = MONTHS.iter().position(|m| mon.starts_with(m))? as i64 + 1;
    let mut year: i64 = parts.get(2)?.parse().ok()?;
    if year < 100 {
        year += if year < 70 { 2000 } else { 1900 };
    }
    let hms: Vec<i64> = parts
        .get(3)?
        .split(':')
        .map(|x| x.parse().ok())
        .collect::<Option<_>>()?;
    let [h, mi, s] = hms[..] else { return None };
    let secs = days_from_civil(year, month, day) * 86_400 + h * 3600 + mi * 60 + s;
    Some(secs as f64)
}

struct SetCookie {
    name: String,
    value: String,
    expires: Option<f64>,
    deleted: bool,
}

fn parse_set_cookie(raw: &str, now_s: f64) -> Option<SetCookie> {
    let mut parts = raw.split(';');
    let (name, value) = parts.next()?.split_once('=')?;
    let (name, value) = (name.trim().to_string(), value.trim().to_string());
    if name.is_empty() {
        return None;
    }
    let mut expires = None;
    let mut max_age = None;
    for attr in parts {
        let (k, v) = attr.split_once('=').unwrap_or((attr, ""));
        match k.trim().to_ascii_lowercase().as_str() {
            "max-age" => max_age = v.trim().parse::<i64>().ok(),
            "expires" => expires = parse_http_date(v.trim()),
            _ => {}
        }
    }
    // Max-Age가 Expires보다 우선합니다 (RFC 6265).
    let expires = max_age.map(|s| now_s + s as f64).or(expires);
    let deleted =
        value.is_empty() || max_age.is_some_and(|s| s <= 0) || expires.is_some_and(|e| e <= now_s);
    Some(SetCookie {
        name,
        value,
        expires,
        deleted,
    })
}

/// 응답이 준 Set-Cookie로 저장본을 갱신합니다. 이미 가진 쿠키만 고칩니다.
/// 삭제·로그아웃 쿠키로는 좋은 세션을 덮지 않습니다. 바뀐 게 있으면 true.
fn merge_set_cookies<'a>(
    jar: &mut [Cookie],
    headers: impl IntoIterator<Item = &'a String>,
    now_s: f64,
) -> bool {
    let mut changed = false;
    for raw in headers {
        let Some(sc) = parse_set_cookie(raw, now_s) else {
            continue;
        };
        if sc.deleted || CLOUDFLARE_COOKIES.contains(&sc.name.as_str()) {
            continue;
        }
        let Some(cookie) = jar.iter_mut().find(|c| c.name == sc.name) else {
            continue;
        };
        let value_changed = cookie.value != sc.value;
        let expiry_changed = sc.name == SESSION_COOKIE
            && sc
                .expires
                .is_some_and(|e| (e - cookie.expires).abs() >= 1.0);
        if value_changed || expiry_changed {
            cookie.value = sc.value;
            if let Some(e) = sc.expires {
                cookie.expires = e;
                cookie.session = false;
            }
            changed = true;
        }
    }
    changed
}

// --- 조회 --------------------------------------------------------------------

fn fetch(session: &mut WebSession) -> Result<UsageSnapshot, WebError> {
    let (orgs, org_cookies) = organizations(&session.cookies)?;
    let org = pick_org(&orgs, last_active(&session.cookies)).ok_or(WebError::NoOrg)?;
    let usage = get(
        &format!("/api/organizations/{}/usage", org.uuid),
        &session.cookies,
    )
    .map_err(|e| match e {
        // 조직 목록은 받았으니 세션은 살아 있습니다. 조직 권한 문제를 로그인 문제로 돌리지 않습니다.
        WebError::Unauthorized(code) => WebError::Status(code),
        other => other,
    })?;
    let tracks = tracks_from_usage(usage.body).map_err(|_| WebError::Parse)?;

    if merge_set_cookies(
        &mut session.cookies,
        org_cookies.iter().chain(&usage.set_cookies),
        now_secs(),
    ) {
        let _ = save(session);
    }

    Ok(UsageSnapshot {
        state: "OK".into(),
        source: Some(SOURCE_WEB.into()),
        account_email: session.account.clone(),
        plan_label: org_plan(org),
        tracks,
        error: None,
    })
}

/// 무인 갱신: 같은 프로필을 headless로 열어 쿠키를 다시 읽습니다. 30분에 한 번 이하.
fn refresh(old: &WebSession) -> Result<WebSession, String> {
    let captured =
        browser_login::headless_refresh(&SPEC, &profile_dir()?).map_err(|e| e.to_string())?;
    let mut fresh = WebSession::from_captured(captured);
    if fresh.account.is_none() {
        fresh.account = old.account.clone();
    }
    save(&fresh)?;
    Ok(fresh)
}

const NEED_WEB_LOGIN: &str = "NeedLogin: 설정에서 「브라우저로 로그인」을 눌러 주세요";

pub fn usage() -> UsageSnapshot {
    let mut session = match load() {
        Stored::Present(s) => s,
        Stored::Missing => return need_login(SOURCE_WEB, NEED_WEB_LOGIN.into()),
        Stored::Unreadable => {
            return need_login(
                SOURCE_WEB,
                "NeedLogin: 저장된 웹 세션을 읽지 못했습니다. 다시 로그인해 주세요".into(),
            )
        }
    };
    match fetch(&mut session) {
        Ok(snap) => snap,
        // 401/403 → 무인 갱신 1회 → 실패면 로그인 필요.
        Err(WebError::Unauthorized(_)) => match refresh(&session) {
            Ok(mut fresh) => match fetch(&mut fresh) {
                Ok(snap) => snap,
                Err(WebError::Unauthorized(_)) => need_login(SOURCE_WEB, NEED_WEB_LOGIN.into()),
                Err(e) => fetch_error(SOURCE_WEB, format!("FetchError: {e}")),
            },
            Err(_) => need_login(SOURCE_WEB, NEED_WEB_LOGIN.into()),
        },
        Err(e) => fetch_error(SOURCE_WEB, format!("FetchError: {e}")),
    }
}

/// 로그인 창을 띄우고, 사용자가 로그인 후 창을 닫으면 세션을 저장합니다.
pub fn login(on_status: &dyn Fn(&str)) -> Result<WebStatus, String> {
    let profile = profile_dir()?;
    let captured =
        browser_login::interactive_login(&SPEC, &profile, on_status).map_err(|e| e.to_string())?;
    save(&WebSession::from_captured(captured))?;
    Ok(status())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cookie(name: &str, value: &str, expires: f64) -> Cookie {
        Cookie {
            name: name.into(),
            value: value.into(),
            domain: ".claude.ai".into(),
            path: "/".into(),
            expires,
            http_only: true,
            secure: true,
            session: expires <= 0.0,
        }
    }

    fn org(uuid: &str, caps: &[&str]) -> OrgEntry {
        OrgEntry {
            uuid: uuid.into(),
            capabilities: caps.iter().map(|s| s.to_string()).collect(),
            rate_limit_tier: Some("default_claude_max_20x".into()),
        }
    }

    #[test]
    fn org_choice_prefers_last_active_chat_org() {
        let orgs = [
            org("api", &["api", "api_individual"]),
            org("a", &["chat", "claude_max"]),
            org("b", &["chat"]),
        ];
        assert_eq!(pick_org(&orgs, Some("b")).unwrap().uuid, "b");
        assert_eq!(pick_org(&orgs, None).unwrap().uuid, "a");
        // lastActiveOrg가 API 전용 조직을 가리켜도 채팅 조직을 고릅니다.
        assert_eq!(pick_org(&orgs, Some("api")).unwrap().uuid, "a");
        assert_eq!(pick_org(&orgs[..1], Some("api")).unwrap().uuid, "api");
        assert!(pick_org(&[], None).is_none());
    }

    #[test]
    fn plan_from_capabilities() {
        assert_eq!(
            org_plan(&org("a", &["chat", "claude_max"])).as_deref(),
            Some("Max 20x")
        );
        assert_eq!(org_plan(&org("a", &["chat"])), None);
    }

    #[test]
    fn http_dates() {
        assert_eq!(
            parse_http_date("Mon, 09 Nov 2026 03:41:10 GMT"),
            Some(1_794_195_670.0)
        );
        assert_eq!(
            parse_http_date("Mon, 09-Nov-2026 03:41:10 GMT"),
            Some(1_794_195_670.0)
        );
        assert_eq!(parse_http_date("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0.0));
        assert_eq!(parse_http_date("garbage"), None);
    }

    #[test]
    fn set_cookie_updates_session_but_never_with_logout() {
        let now = 1_790_000_000.0;
        let mut jar = vec![
            cookie("sessionKey", "old", now + 1000.0),
            cookie("lastActiveOrg", "o1", now + 1000.0),
        ];

        let deletions = [
            "sessionKey=; Path=/; Max-Age=0".to_string(),
            "sessionKey=gone; Expires=Thu, 01 Jan 1970 00:00:00 GMT".to_string(),
            "__cf_bm=abc; Path=/; HttpOnly".to_string(),
            "unknown=1; Path=/".to_string(),
        ];
        assert!(!merge_set_cookies(&mut jar, &deletions, now));
        assert_eq!(jar[0].value, "old");

        let rotate = ["sessionKey=new; Path=/; Max-Age=2419200; HttpOnly; Secure".to_string()];
        assert!(merge_set_cookies(&mut jar, &rotate, now));
        assert_eq!(jar[0].value, "new");
        assert_eq!(jar[0].expires, now + 2_419_200.0);
        // 같은 값이 다시 오면 저장하지 않습니다.
        assert!(!merge_set_cookies(&mut jar, &rotate, now));
    }

    #[test]
    fn cookie_header_skips_expired_and_empty() {
        let now = 1_790_000_000.0;
        let jar = [
            cookie("sessionKey", "s", now + 10.0),
            cookie("old", "x", now - 10.0),
            cookie("empty", "", now + 10.0),
            cookie("ion-vk", "v", -1.0),
        ];
        assert_eq!(cookie_header(&jar, now), "sessionKey=s; ion-vk=v");
    }

    #[test]
    fn session_json_round_trip_and_expiry() {
        let s = WebSession {
            cookies: vec![cookie("sessionKey", "s", 1_794_195_670.9)],
            saved_at: 1,
            account: None,
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"savedAt\"") && json.contains("\"httpOnly\""));
        let back: WebSession = serde_json::from_str(&json).unwrap();
        assert_eq!(back.expires_at_ms(), Some(1_794_195_670_900));
    }

    /// 실측: 로그인된 위젯 프로필을 headless로 열어 쿠키를 읽고 session.dat에 저장합니다.
    #[test]
    #[ignore = "실측: 로그인된 login-profile 필요"]
    fn live_capture_from_profile() {
        let started = std::time::Instant::now();
        let fresh = refresh(&WebSession {
            cookies: Vec::new(),
            saved_at: 0,
            account: None,
        })
        .expect("headless 쿠키 읽기 실패");
        let names: Vec<_> = fresh.cookies.iter().map(|c| c.name.as_str()).collect();
        println!(
            "captured in {:?}: cookies={names:?} account={} expires_at={:?}",
            started.elapsed(),
            fresh.account.is_some(),
            fresh.expires_at_ms()
        );
        assert!(names.contains(&SESSION_COOKIE));
        assert!(names.iter().all(|n| !CLOUDFLARE_COOKIES.contains(n)));
        assert!(matches!(load(), Stored::Present(_)));
    }

    /// 실측: 저장된 세션으로 실제 사용량을 읽습니다. 값은 출력하지 않습니다.
    #[test]
    #[ignore = "실측: 로그인된 session.dat 필요"]
    fn live_usage_from_saved_session() {
        let snap = usage();
        println!(
            "state={} source={:?} plan={:?} tracks={:?}",
            snap.state,
            snap.source,
            snap.plan_label,
            snap.tracks
                .iter()
                .map(|t| (&t.label, t.percent))
                .collect::<Vec<_>>()
        );
        assert_eq!(snap.state, "OK", "{:?}", snap.error);
    }
}
