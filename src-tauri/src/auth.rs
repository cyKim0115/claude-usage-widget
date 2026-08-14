use std::path::PathBuf;

use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("NeedLogin: 자격증명 파일이 없습니다 ({0})")]
    FileMissing(String),
    #[error("NeedLogin: 자격증명을 읽지 못했습니다: {0}")]
    Read(String),
    #[error("NeedLogin: accessToken이 없습니다")]
    TokenMissing,
    #[error("NeedLogin: accessToken이 만료됐습니다")]
    Expired,
}

pub struct Credentials {
    pub access_token: String,
}

#[derive(Debug, Deserialize)]
struct CredentialsFile {
    #[serde(rename = "claudeAiOauth")]
    claude_ai_oauth: Option<OauthEntry>,
}

#[derive(Debug, Deserialize)]
struct OauthEntry {
    #[serde(rename = "accessToken")]
    access_token: Option<String>,
    #[serde(rename = "expiresAt")]
    expires_at: Option<i64>,
}

pub fn default_credentials_path() -> PathBuf {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .unwrap_or_default();
    PathBuf::from(home).join(".claude").join(".credentials.json")
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Claude Code CLI가 저장한 OAuth 토큰을 읽습니다.
///
/// 읽기 전용입니다. 토큰 갱신은 하지 않습니다 — Claude Code와 refresh token
/// 회전이 겹치면 양쪽 로그인이 함께 풀릴 수 있어서, 만료되면 그대로 NeedLogin을
/// 돌려주고 사용자가 Claude Code를 실행해 갱신하도록 둡니다.
/// 반환된 토큰은 절대 로그에 남기지 않습니다.
pub fn read_credentials(path: &PathBuf) -> Result<Credentials, AuthError> {
    if !path.is_file() {
        return Err(AuthError::FileMissing(path.display().to_string()));
    }

    let raw = std::fs::read_to_string(path).map_err(|e| AuthError::Read(e.to_string()))?;
    let parsed: CredentialsFile =
        serde_json::from_str(&raw).map_err(|e| AuthError::Read(e.to_string()))?;

    let entry = parsed.claude_ai_oauth.ok_or(AuthError::TokenMissing)?;
    let token = entry.access_token.filter(|t| !t.is_empty()).ok_or(AuthError::TokenMissing)?;

    // 만료된 토큰이면 네트워크 왕복 없이 바로 NeedLogin으로 끊습니다.
    if let Some(expires_at) = entry.expires_at {
        if expires_at > 0 && expires_at <= now_ms() {
            return Err(AuthError::Expired);
        }
    }

    Ok(Credentials { access_token: token })
}
