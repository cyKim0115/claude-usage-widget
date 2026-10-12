//! Phase 0 실측: claude.ai `sessionKey` 쿠키만으로 사용량 엔드포인트가 Rust(ureq, rustls)
//! 평문 HTTP에서 응답하는지 확인합니다. Node fetch와 TLS 지문이 달라 Cloudflare 판정이
//! 갈릴 수 있어서 따로 봅니다.
//!
//! `node scripts/spike_web_session.mjs`가 `CUW_SPIKE_SESSION_KEY`·`CUW_SPIKE_ORG`·
//! `CUW_SPIKE_UA`를 넣어 실행합니다. 쿠키 값은 출력하지 않습니다.

use std::time::Duration;

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(30))
        .redirects(0)
        .build()
}

struct Probe {
    status: u16,
    cf: bool,
    json: Option<serde_json::Value>,
    set_cookie_names: Vec<String>,
}

fn get(url: &str, session_key: &str, ua: &str) -> Probe {
    let result = agent()
        .get(url)
        .set("Cookie", &format!("sessionKey={session_key}"))
        .set("User-Agent", ua)
        .set("Accept", "application/json")
        .call();
    let resp = match result {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(e) => panic!("transport error: {e}"),
    };
    let status = resp.status();
    let cf_header = resp.header("cf-mitigated") == Some("challenge");
    let set_cookie_names = resp
        .all("set-cookie")
        .iter()
        .filter_map(|s| s.split('=').next())
        .map(str::to_string)
        .collect();
    let body = resp.into_string().unwrap_or_default();
    let cf = cf_header || (status == 403 && body.contains("Just a moment"));
    Probe {
        status,
        cf,
        json: serde_json::from_str(&body).ok(),
        set_cookie_names,
    }
}

fn keys(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Object(m) => m.keys().cloned().collect::<Vec<_>>().join(", "),
        serde_json::Value::Array(a) => format!("list[{}]", a.len()),
        other => format!("{other}"),
    }
}

#[test]
#[ignore = "실측: CUW_SPIKE_SESSION_KEY 필요"]
fn claude_ai_usage_over_plain_http() {
    let Ok(session_key) = std::env::var("CUW_SPIKE_SESSION_KEY") else {
        panic!("CUW_SPIKE_SESSION_KEY가 없습니다");
    };
    let ua = std::env::var("CUW_SPIKE_UA").unwrap_or_else(|_| {
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/154.0.0.0 Safari/537.36".into()
    });

    let orgs = get("https://claude.ai/api/organizations", &session_key, &ua);
    println!(
        "  [ureq] GET /api/organizations → {} cf={} setCookie={:?}",
        orgs.status, orgs.cf, orgs.set_cookie_names
    );

    let org = std::env::var("CUW_SPIKE_ORG")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            orgs.json
                .as_ref()
                .and_then(|v| v.as_array())
                .and_then(|a| a.first())
                .and_then(|o| o.get("uuid"))
                .and_then(|u| u.as_str())
                .map(str::to_string)
        })
        .expect("org uuid를 얻지 못했습니다");

    let usage = get(
        &format!("https://claude.ai/api/organizations/{org}/usage"),
        &session_key,
        &ua,
    );
    println!(
        "  [ureq] GET /api/organizations/{{org}}/usage → {} cf={} keys=[{}]",
        usage.status,
        usage.cf,
        usage.json.as_ref().map(keys).unwrap_or_default()
    );

    assert_eq!(orgs.status, 200, "organizations가 200이 아닙니다");
    assert_eq!(usage.status, 200, "usage가 200이 아닙니다");
    assert!(!usage.cf, "Cloudflare 챌린지");
}
