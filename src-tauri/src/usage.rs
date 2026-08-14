use serde::{Deserialize, Serialize};
use thiserror::Error;

const API_BASE: &str = "https://api.anthropic.com";
const USAGE_PATH: &str = "/api/oauth/usage";
const PROFILE_PATH: &str = "/api/oauth/profile";
const OAUTH_BETA: &str = "oauth-2025-04-20";

#[derive(Debug, Error)]
pub enum UsageError {
    #[error("NeedLogin: 토큰이 거부됐습니다 (401)")]
    Unauthorized,
    #[error("FetchError: {0}")]
    Fetch(String),
    #[error("FetchError: 응답 해석 실패: {0}")]
    Parse(String),
}

/// 위젯 바 하나에 해당합니다. 플랜에 따라 개수가 달라집니다.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackUsage {
    pub label: String,
    pub kind: String,
    pub percent: f64,
    pub severity: String,
    pub resets_at: Option<String>,
    pub is_active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageSnapshot {
    pub state: String,
    pub account_email: Option<String>,
    pub plan_label: Option<String>,
    pub tracks: Vec<TrackUsage>,
    pub error: Option<String>,
}

// --- 응답 스키마 ---
//
// 응답에는 아직 공개되지 않은 필드가 여럿 섞여 나옵니다. 필요한 것만 Option으로
// 뽑아 두면 상대가 필드를 늘리거나 줄여도 파싱이 깨지지 않습니다.

#[derive(Debug, Deserialize)]
struct UsageResponse {
    #[serde(default)]
    limits: Vec<LimitEntry>,
}

#[derive(Debug, Deserialize)]
struct LimitEntry {
    kind: Option<String>,
    percent: Option<f64>,
    severity: Option<String>,
    #[serde(rename = "resets_at")]
    resets_at: Option<String>,
    #[serde(rename = "is_active")]
    is_active: Option<bool>,
    scope: Option<LimitScope>,
}

#[derive(Debug, Deserialize)]
struct LimitScope {
    model: Option<ScopeModel>,
}

#[derive(Debug, Deserialize)]
struct ScopeModel {
    #[serde(rename = "display_name")]
    display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ProfileResponse {
    account: Option<ProfileAccount>,
    organization: Option<ProfileOrg>,
}

#[derive(Debug, Deserialize)]
struct ProfileAccount {
    email: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ProfileOrg {
    #[serde(rename = "organization_type")]
    organization_type: Option<String>,
    #[serde(rename = "rate_limit_tier")]
    rate_limit_tier: Option<String>,
}

fn get_json(path: &str, token: &str) -> Result<serde_json::Value, UsageError> {
    let url = format!("{API_BASE}{path}");
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(10))
        .timeout_read(std::time::Duration::from_secs(30))
        .build();

    let resp = agent
        .get(&url)
        .set("Authorization", &format!("Bearer {token}"))
        .set("anthropic-beta", OAUTH_BETA)
        .set("User-Agent", "claude-usage-widget/0.1")
        .call()
        .map_err(|e| match e {
            ureq::Error::Status(401, _) => UsageError::Unauthorized,
            other => UsageError::Fetch(other.to_string()),
        })?;

    resp.into_json::<serde_json::Value>()
        .map_err(|e| UsageError::Parse(e.to_string()))
}

/// 트랙 라벨. `weekly_scoped`는 kind만으로는 무엇의 한도인지 알 수 없어
/// scope의 모델 이름까지 봐야 합니다.
fn label_for(entry: &LimitEntry) -> String {
    let kind = entry.kind.as_deref().unwrap_or("unknown");
    match kind {
        "session" => "5시간".to_string(),
        "weekly_all" => "주간".to_string(),
        "weekly_scoped" => entry
            .scope
            .as_ref()
            .and_then(|s| s.model.as_ref())
            .and_then(|m| m.display_name.as_deref())
            .map(|name| format!("주간 · {name}"))
            .unwrap_or_else(|| "주간 · 모델별".to_string()),
        // 모르는 kind는 숨기지 않고 원문 그대로 노출합니다. 새 한도가 생겼을 때
        // 조용히 사라지는 것보다 낯선 이름이라도 보이는 편이 낫습니다.
        other => other.to_string(),
    }
}

fn plan_label(org: &ProfileOrg) -> Option<String> {
    let base = match org.organization_type.as_deref()? {
        "claude_max" => "Max",
        "claude_pro" => "Pro",
        "claude_team" => "Team",
        other => other,
    };

    // "default_claude_max_20x" 처럼 tier 끝에 배수가 붙어 옵니다.
    let multiplier = org
        .rate_limit_tier
        .as_deref()
        .and_then(|tier| tier.rsplit('_').next())
        .filter(|seg| seg.ends_with('x') && seg.len() <= 4);

    Some(match multiplier {
        Some(m) => format!("{base} {m}"),
        None => base.to_string(),
    })
}

pub fn fetch_usage(token: &str) -> Result<UsageSnapshot, UsageError> {
    let usage_val = get_json(USAGE_PATH, token)?;
    // 프로필은 부가 정보(계정·플랜)라 실패해도 사용량 표시를 막지 않습니다.
    let profile_val = get_json(PROFILE_PATH, token).unwrap_or_else(|_| serde_json::json!({}));

    let usage: UsageResponse =
        serde_json::from_value(usage_val).map_err(|e| UsageError::Parse(e.to_string()))?;
    let profile: ProfileResponse = serde_json::from_value(profile_val).unwrap_or(ProfileResponse {
        account: None,
        organization: None,
    });

    let tracks = usage
        .limits
        .iter()
        .map(|entry| TrackUsage {
            label: label_for(entry),
            kind: entry.kind.clone().unwrap_or_else(|| "unknown".into()),
            percent: entry.percent.unwrap_or(0.0),
            severity: entry.severity.clone().unwrap_or_else(|| "normal".into()),
            resets_at: entry.resets_at.clone(),
            is_active: entry.is_active.unwrap_or(false),
        })
        .collect();

    Ok(UsageSnapshot {
        state: "OK".into(),
        account_email: profile.account.and_then(|a| a.email),
        plan_label: profile.organization.as_ref().and_then(plan_label),
        tracks,
        error: None,
    })
}

fn empty_snapshot(state: &str, message: String) -> UsageSnapshot {
    UsageSnapshot {
        state: state.into(),
        account_email: None,
        plan_label: None,
        tracks: Vec::new(),
        error: Some(message),
    }
}

pub fn need_login(message: String) -> UsageSnapshot {
    empty_snapshot("NeedLogin", message)
}

pub fn fetch_error(message: String) -> UsageSnapshot {
    empty_snapshot("FetchError", message)
}
