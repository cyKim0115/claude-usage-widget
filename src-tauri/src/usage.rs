use serde::{Deserialize, Serialize};
use thiserror::Error;

const API_BASE: &str = "https://api.anthropic.com";
const USAGE_PATH: &str = "/api/oauth/usage";
const PROFILE_PATH: &str = "/api/oauth/profile";
const OAUTH_BETA: &str = "oauth-2025-04-20";

pub const SOURCE_CLAUDE_CODE: &str = "claudeCode";
pub const SOURCE_WEB: &str = "web";

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
    /// `claudeCode`(CLI OAuth) 또는 `web`(위젯 전용 브라우저 로그인).
    pub source: Option<String>,
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

/// `limits[]`가 없을 때 보는 기간별 창. 응답에 실제로 값이 있는 창만 트랙이 됩니다.
#[derive(Debug, Deserialize)]
struct WindowEntry {
    utilization: Option<f64>,
    resets_at: Option<String>,
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
    plan_label_from(
        org.organization_type.as_deref(),
        org.rate_limit_tier.as_deref(),
    )
}

/// `organization_type`(예: `claude_max`)과 `rate_limit_tier`로 헤더에 쓸 플랜 이름을 만듭니다.
pub fn plan_label_from(
    organization_type: Option<&str>,
    rate_limit_tier: Option<&str>,
) -> Option<String> {
    let base = match organization_type? {
        "claude_max" => "Max",
        "claude_pro" => "Pro",
        "claude_team" => "Team",
        other => other,
    };

    // "default_claude_max_20x" 처럼 tier 끝에 배수가 붙어 옵니다.
    let multiplier = rate_limit_tier
        .and_then(|tier| tier.rsplit('_').next())
        .filter(|seg| seg.ends_with('x') && seg.len() <= 4);

    Some(match multiplier {
        Some(m) => format!("{base} {m}"),
        None => base.to_string(),
    })
}

/// 사용량 응답을 트랙으로 바꿉니다. OAuth와 claude.ai 웹 응답이 같은 `limits[]`를 줍니다.
///
/// `limits[]`가 없으면 기간별 창(`five_hour` 등) 중 값이 있는 것만 씁니다. 응답에 없는 창을
/// 0%로 만들어 넣지 않습니다. 사용률이 빠진 항목도 0%로 그리지 않고 건너뜁니다.
pub fn tracks_from_usage(value: serde_json::Value) -> Result<Vec<TrackUsage>, UsageError> {
    let windows = window_tracks(&value);
    let usage: UsageResponse =
        serde_json::from_value(value).map_err(|e| UsageError::Parse(e.to_string()))?;

    if usage.limits.is_empty() {
        return Ok(windows);
    }
    Ok(usage
        .limits
        .iter()
        .filter_map(|entry| {
            Some(TrackUsage {
                label: label_for(entry),
                kind: entry.kind.clone().unwrap_or_else(|| "unknown".into()),
                percent: entry.percent?,
                severity: entry.severity.clone().unwrap_or_else(|| "normal".into()),
                resets_at: entry.resets_at.clone(),
                is_active: entry.is_active.unwrap_or(false),
            })
        })
        .collect())
}

fn window_tracks(value: &serde_json::Value) -> Vec<TrackUsage> {
    const WINDOWS: [(&str, &str, &str); 4] = [
        ("five_hour", "session", "5시간"),
        ("seven_day", "weekly_all", "주간"),
        ("seven_day_opus", "weekly_scoped", "주간 · Opus"),
        ("seven_day_sonnet", "weekly_scoped", "주간 · Sonnet"),
    ];
    WINDOWS
        .iter()
        .filter_map(|(key, kind, label)| {
            let entry: WindowEntry = serde_json::from_value(value.get(*key)?.clone()).ok()?;
            Some(TrackUsage {
                label: (*label).into(),
                kind: (*kind).into(),
                percent: entry.utilization?,
                severity: "normal".into(),
                resets_at: entry.resets_at,
                is_active: false,
            })
        })
        .collect()
}

pub fn fetch_usage(token: &str) -> Result<UsageSnapshot, UsageError> {
    let usage_val = get_json(USAGE_PATH, token)?;
    // 프로필은 부가 정보(계정·플랜)라 실패해도 사용량 표시를 막지 않습니다.
    let profile_val = get_json(PROFILE_PATH, token).unwrap_or_else(|_| serde_json::json!({}));

    let tracks = tracks_from_usage(usage_val)?;
    let profile: ProfileResponse = serde_json::from_value(profile_val).unwrap_or(ProfileResponse {
        account: None,
        organization: None,
    });

    Ok(UsageSnapshot {
        state: "OK".into(),
        source: Some(SOURCE_CLAUDE_CODE.into()),
        account_email: profile.account.and_then(|a| a.email),
        plan_label: profile.organization.as_ref().and_then(plan_label),
        tracks,
        error: None,
    })
}

fn empty_snapshot(state: &str, source: &str, message: String) -> UsageSnapshot {
    UsageSnapshot {
        state: state.into(),
        source: Some(source.into()),
        account_email: None,
        plan_label: None,
        tracks: Vec::new(),
        error: Some(message),
    }
}

pub fn need_login(source: &str, message: String) -> UsageSnapshot {
    empty_snapshot("NeedLogin", source, message)
}

pub fn fetch_error(source: &str, message: String) -> UsageSnapshot {
    empty_snapshot("FetchError", source, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_become_tracks_and_missing_percent_is_skipped() {
        let value = serde_json::json!({
            "five_hour": { "utilization": 20, "resets_at": "2026-10-12T05:30:00.063513+00:00" },
            "limits": [
                { "kind": "session", "percent": 20, "severity": "normal",
                  "resets_at": "2026-10-12T05:30:00.063513+00:00", "is_active": false },
                { "kind": "weekly_all", "percent": 66, "is_active": true },
                { "kind": "weekly_scoped", "percent": 0,
                  "scope": { "model": { "id": null, "display_name": "Fable" } } },
                { "kind": "mystery" }
            ]
        });
        let tracks = tracks_from_usage(value).unwrap();
        let labels: Vec<_> = tracks.iter().map(|t| t.label.as_str()).collect();
        assert_eq!(labels, ["5시간", "주간", "주간 · Fable"]);
        assert_eq!(tracks[1].percent, 66.0);
        assert!(tracks[1].is_active);
        assert_eq!(
            tracks[0].resets_at.as_deref(),
            Some("2026-10-12T05:30:00.063513+00:00")
        );
    }

    #[test]
    fn windows_fallback_only_uses_present_windows() {
        let value = serde_json::json!({
            "five_hour": { "utilization": 12.5, "resets_at": "2026-10-12T05:30:00+00:00" },
            "seven_day": { "utilization": 40, "resets_at": null },
            "seven_day_opus": null,
            "seven_day_sonnet": { "utilization": null, "resets_at": null }
        });
        let tracks = tracks_from_usage(value).unwrap();
        let kinds: Vec<_> = tracks
            .iter()
            .map(|t| (t.kind.as_str(), t.percent))
            .collect();
        assert_eq!(kinds, [("session", 12.5), ("weekly_all", 40.0)]);
    }

    #[test]
    fn plan_label_with_multiplier() {
        assert_eq!(
            plan_label_from(Some("claude_max"), Some("default_claude_max_20x")).as_deref(),
            Some("Max 20x")
        );
        assert_eq!(
            plan_label_from(Some("claude_pro"), None).as_deref(),
            Some("Pro")
        );
        assert_eq!(plan_label_from(None, Some("default_claude_max_20x")), None);
    }
}
