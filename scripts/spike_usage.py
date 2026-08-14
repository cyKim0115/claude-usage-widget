"""Phase 0 spike: Claude 사용량 조회 경로를 검증합니다.

위젯 Rust 코드가 그대로 따라갈 흐름을 파이썬으로 먼저 확인합니다.

    %USERPROFILE%\\.claude\\.credentials.json  →  claudeAiOauth.accessToken
    GET https://api.anthropic.com/api/oauth/usage
    GET https://api.anthropic.com/api/oauth/profile

토큰 값은 어떤 경우에도 출력하지 않습니다.
"""

from __future__ import annotations

import json
import os
import sys
import urllib.error
import urllib.request
from datetime import datetime, timezone
from pathlib import Path

API_BASE = "https://api.anthropic.com"
USAGE_PATH = "/api/oauth/usage"
PROFILE_PATH = "/api/oauth/profile"
OAUTH_BETA = "oauth-2025-04-20"


def credentials_path() -> Path:
    return Path(os.path.expanduser("~")) / ".claude" / ".credentials.json"


def read_token() -> tuple[str, int]:
    """(accessToken, expiresAt ms)를 돌려줍니다. 실패하면 종료합니다."""
    path = credentials_path()
    if not path.is_file():
        sys.exit(f"NeedLogin: {path} 없음. `claude auth login` 후 다시 실행하세요.")

    try:
        oauth = json.loads(path.read_text(encoding="utf-8"))["claudeAiOauth"]
    except (json.JSONDecodeError, KeyError) as exc:
        sys.exit(f"NeedLogin: 자격증명 파싱 실패: {exc}")

    token = oauth.get("accessToken")
    if not token:
        sys.exit("NeedLogin: claudeAiOauth.accessToken 없음.")
    return token, int(oauth.get("expiresAt", 0))


def get(path: str, token: str) -> dict:
    req = urllib.request.Request(
        API_BASE + path,
        headers={
            "Authorization": f"Bearer {token}",
            "anthropic-beta": OAUTH_BETA,
            "User-Agent": "claude-usage-widget-spike/0.1",
        },
    )
    with urllib.request.urlopen(req, timeout=30) as resp:
        return json.loads(resp.read().decode("utf-8"))


def label_for(limit: dict) -> str:
    """위젯이 쓸 트랙 라벨. kind만으로는 부족하고 scope까지 봐야 합니다."""
    kind = limit.get("kind")
    if kind == "session":
        return "5시간"
    if kind == "weekly_all":
        return "주간"
    if kind == "weekly_scoped":
        model = (limit.get("scope") or {}).get("model") or {}
        name = model.get("display_name")
        return f"주간 · {name}" if name else "주간 · 모델별"
    return str(kind)


def fmt_reset(raw: str | None) -> str:
    if not raw:
        return "리셋 시각 미정"
    when = datetime.fromisoformat(raw)
    diff = when - datetime.now(timezone.utc)
    hours = diff.total_seconds() / 3600
    if hours < 0:
        return "만료됨"
    if hours < 24:
        return f"{hours:.1f}시간 남음"
    return f"{hours / 24:.1f}일 남음"


def main() -> None:
    token, expires_at = read_token()

    if expires_at:
        left = (expires_at / 1000) - datetime.now(timezone.utc).timestamp()
        print(f"token expires in: {left / 3600:.1f}h")
        if left <= 0:
            sys.exit("NeedLogin: accessToken 만료. Claude Code를 한 번 실행하세요.")

    try:
        profile = get(PROFILE_PATH, token)
        usage = get(USAGE_PATH, token)
    except urllib.error.HTTPError as exc:
        if exc.code == 401:
            sys.exit("NeedLogin: 401. 토큰이 더 이상 유효하지 않습니다.")
        sys.exit(f"FetchError: HTTP {exc.code}")
    except urllib.error.URLError as exc:
        sys.exit(f"FetchError: {exc.reason}")

    account = profile.get("account", {})
    org = profile.get("organization", {})
    print()
    print("=== account ===")
    print(f"  email : {account.get('email')}")
    print(f"  plan  : {org.get('organization_type')} ({org.get('rate_limit_tier')})")

    print()
    print("=== limits[] → 위젯 트랙 ===")
    for limit in usage.get("limits", []):
        print(
            f"  {label_for(limit):<14} {limit.get('percent'):>3}%"
            f"  severity={limit.get('severity'):<8}"
            f"  active={str(limit.get('is_active')):<5}"
            f"  {fmt_reset(limit.get('resets_at'))}"
        )

    if not usage.get("limits"):
        print("  (없음) — limits[] 가 비었습니다. 스키마 변경 가능성.")

    print()
    print("PASS")


if __name__ == "__main__":
    main()
