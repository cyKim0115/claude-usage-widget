"""Phase 0 스파이크: 브라우저 세션(claude.ai sessionKey) 경로를 검증합니다.

위젯에 넣기 전에 두 층을 먼저 증명합니다.

    1) 브라우저 쿠키 DB → DPAPI로 마스터키 복호화 → AES-GCM으로 sessionKey 복호화
    2) sessionKey 로 사용량을 실제로 조회할 수 있는 엔드포인트 확인

이 스파이크가 통과해야 Rust 쪽 브라우저-쿠키 소스가 의미가 있습니다.
토큰/세션 값은 어떤 경우에도 원문을 출력하지 않습니다 (마스킹만).

필요: cryptography (AES-GCM). DPAPI는 ctypes로 직접 부릅니다.
"""

from __future__ import annotations

import base64
import ctypes
import ctypes.wintypes as wt
import json
import os
import shutil
import sqlite3
import sys
import tempfile
import urllib.error
import urllib.request
from pathlib import Path

from cryptography.hazmat.primitives.ciphers.aead import AESGCM

LOCALAPPDATA = Path(os.environ.get("LOCALAPPDATA", ""))
APPDATA = Path(os.environ.get("APPDATA", ""))

# (표시 이름, User Data 폴더). Claude 데스크톱 앱을 맨 앞에 둡니다 —
# 이쪽만 app-bound(v20)가 아니라 v10이라 승격 없이 DPAPI로 풀립니다.
BROWSERS = [
    ("Claude 데스크톱", APPDATA / "Claude"),
    ("Chrome", LOCALAPPDATA / "Google" / "Chrome" / "User Data"),
    ("Edge", LOCALAPPDATA / "Microsoft" / "Edge" / "User Data"),
    ("Brave", LOCALAPPDATA / "BraveSoftware" / "Brave-Browser" / "User Data"),
]


# --- DPAPI (pywin32 없이 ctypes 로) ---------------------------------------

class DATA_BLOB(ctypes.Structure):
    _fields_ = [("cbData", wt.DWORD), ("pbData", ctypes.POINTER(ctypes.c_char))]


def dpapi_unprotect(blob: bytes) -> bytes:
    """CryptUnprotectData. 현재 사용자 키로만 복호화됩니다."""
    buf_in = DATA_BLOB(len(blob), ctypes.cast(ctypes.create_string_buffer(blob, len(blob)),
                                              ctypes.POINTER(ctypes.c_char)))
    buf_out = DATA_BLOB()
    ok = ctypes.windll.crypt32.CryptUnprotectData(
        ctypes.byref(buf_in), None, None, None, None, 0, ctypes.byref(buf_out)
    )
    if not ok:
        raise OSError("CryptUnprotectData 실패 (다른 사용자/머신 자격일 수 있음)")
    try:
        return ctypes.string_at(buf_out.pbData, buf_out.cbData)
    finally:
        ctypes.windll.kernel32.LocalFree(buf_out.pbData)


def shared_copy(src: Path, dst: Path) -> None:
    """CreateFileW 로 전체 공유(READ|WRITE|DELETE) 열기 후 바이트 복사.

    브라우저가 쿠키 DB를 잡고 있어도, 넓은 공유 플래그로 열면 읽을 수 있습니다.
    파이썬 open()/shutil 은 공유가 좁아 WinError 32 가 납니다.
    """
    GENERIC_READ = 0x80000000
    FILE_SHARE_ALL = 0x1 | 0x2 | 0x4  # READ|WRITE|DELETE
    OPEN_EXISTING = 3
    handle = ctypes.windll.kernel32.CreateFileW(
        str(src), GENERIC_READ, FILE_SHARE_ALL, None, OPEN_EXISTING, 0, None
    )
    if handle == -1 or handle == ctypes.c_void_p(-1).value:
        raise OSError("CreateFileW 실패")
    try:
        chunk = (ctypes.c_char * 65536)()
        read = wt.DWORD(0)
        with open(dst, "wb") as out:
            while True:
                ok = ctypes.windll.kernel32.ReadFile(
                    wt.HANDLE(handle), chunk, 65536, ctypes.byref(read), None
                )
                if not ok or read.value == 0:
                    break
                out.write(bytes(chunk[: read.value]))
    finally:
        ctypes.windll.kernel32.CloseHandle(wt.HANDLE(handle))


def master_key(user_data: Path) -> bytes:
    """Local State 의 os_crypt.encrypted_key → DPAPI 복호화 → AES 마스터키."""
    local_state = json.loads((user_data / "Local State").read_text(encoding="utf-8"))
    enc = base64.b64decode(local_state["os_crypt"]["encrypted_key"])
    if enc[:5] != b"DPAPI":
        raise ValueError("예상치 못한 키 접두사 (app-bound v20 전용일 수 있음)")
    return dpapi_unprotect(enc[5:])


def decrypt_cookie(value: bytes, key: bytes) -> str | None:
    """v10/v11 (AES-GCM). v20(app-bound)은 여기서 처리하지 못합니다."""
    if value[:3] not in (b"v10", b"v11"):
        return None  # 구형 DPAPI-only 또는 v20 app-bound
    nonce, payload = value[3:15], value[15:]
    try:
        plain = AESGCM(key).decrypt(nonce, payload, None)
    except Exception:
        return None
    # 신형 크롬은 복호화 평문 앞에 32바이트 도메인 바인딩을 붙입니다.
    text = plain.decode("utf-8", "ignore")
    if not text.startswith("sk-ant") and len(plain) > 32:
        text = plain[32:].decode("utf-8", "ignore")
    return text


def mask(secret: str) -> str:
    if len(secret) <= 16:
        return "***"
    return f"{secret[:12]}…{secret[-4:]} (len={len(secret)})"


def find_session_keys(user_data: Path) -> list[tuple[str, str]]:
    """(profile, sessionKey) 목록. 여러 프로필을 훑습니다."""
    try:
        key = master_key(user_data)
    except Exception as exc:
        print(f"    마스터키 실패: {exc}")
        return []

    results: list[tuple[str, str]] = []
    # 루트 자신(데스크톱 앱: Network/Cookies가 바로 아래)과 프로필 하위폴더 모두 훑습니다.
    candidates = [user_data] + [p for p in sorted(user_data.glob("*")) if p.is_dir()]
    for profile_dir in candidates:
        for rel in ("Network/Cookies", "Cookies"):
            cookies_db = profile_dir / rel
            if cookies_db.is_file():
                break
        else:
            continue

        # 브라우저가 파일을 잠그고 있어 일반 복사는 WinError 32로 막힙니다.
        # 넓은 공유 플래그로 바이트만 복사한 뒤 그 사본을 엽니다.
        tmp = Path(tempfile.gettempdir()) / f"cuw_spike_{profile_dir.name}.db"
        try:
            shared_copy(cookies_db, tmp)
            con = sqlite3.connect(tmp)
            rows = con.execute(
                "SELECT name, encrypted_value FROM cookies "
                "WHERE host_key LIKE '%claude.ai%' AND name = 'sessionKey'"
            ).fetchall()
            con.close()
        except Exception as exc:
            print(f"    {profile_dir.name}: 쿠키 DB 열기 실패: {exc}")
            continue
        finally:
            tmp.unlink(missing_ok=True)

        for name, enc in rows:
            sk = decrypt_cookie(enc, key)
            if sk and sk.startswith("sk-ant"):
                results.append((profile_dir.name, sk))
    return results


# --- 사용량 엔드포인트 후보 탐침 -------------------------------------------

def _call(url: str, session_key: str) -> tuple[int, str]:
    req = urllib.request.Request(
        url,
        headers={
            "Cookie": f"sessionKey={session_key}",
            "User-Agent": "Mozilla/5.0 claude-usage-widget-spike/0.1",
            "Accept": "application/json",
        },
    )
    try:
        with urllib.request.urlopen(req, timeout=20) as resp:
            return resp.status, resp.read(8000).decode("utf-8", "ignore")
    except urllib.error.HTTPError as exc:
        return exc.code, exc.read(2000).decode("utf-8", "ignore") if exc.fp else ""
    except urllib.error.URLError as exc:
        return -1, str(exc.reason)


def probe(session_key: str) -> None:
    """sessionKey 쿠키로 claude.ai 사용량 경로를 찾습니다.

    1) /api/organizations 로 org uuid 확보
    2) org 범위의 사용량 후보 경로들을 탐침
    상태코드와 최상위 키만 봅니다 (민감정보 미출력).
    """
    base = [
        "https://claude.ai/api/organizations",
        "https://claude.ai/api/bootstrap",
        "https://claude.ai/api/account",
        # 참고: OAuth 전용 엔드포인트. sessionKey 쿠키로는 보통 401이 예상됩니다.
        "https://api.anthropic.com/api/oauth/usage",
    ]

    org_uuid = None
    for url in base:
        status, body = _call(url, session_key)
        print(f"  {status} {url}\n        {_shape(body)}")
        if url.endswith("/organizations") and status == 200:
            try:
                orgs = json.loads(body)
                if isinstance(orgs, list) and orgs:
                    org_uuid = orgs[0].get("uuid")
            except Exception:
                pass

    if not org_uuid:
        print("  org uuid 확보 실패 → org 범위 탐침 생략")
        return

    print(f"\n  org uuid 확보. 사용량 후보 탐침:")
    org_candidates = [
        f"https://claude.ai/api/organizations/{org_uuid}/usage",
        f"https://claude.ai/api/organizations/{org_uuid}/rate_limits",
        f"https://claude.ai/api/organizations/{org_uuid}/usage_limits",
        f"https://claude.ai/api/organizations/{org_uuid}/claude_code_usage",
        f"https://claude.ai/api/organizations/{org_uuid}/billing/usage",
        f"https://claude.ai/api/bootstrap/{org_uuid}/statsig",
    ]
    for url in org_candidates:
        status, body = _call(url, session_key)
        print(f"  {status} {url}\n        {_shape(body)}")


def _shape(body: str) -> str:
    """본문 전체 대신 최상위 키/타입만 요약합니다 (민감정보 유출 방지)."""
    try:
        data = json.loads(body)
    except Exception:
        return f"(non-json, {len(body)}B) {body[:80]!r}"
    if isinstance(data, dict):
        return "keys: " + ", ".join(list(data.keys())[:15])
    if isinstance(data, list):
        head = data[0] if data else None
        keys = ", ".join(head.keys()) if isinstance(head, dict) else type(head).__name__
        return f"list[{len(data)}] first keys: {keys}"
    return type(data).__name__


def main() -> None:
    print("=== 브라우저에서 claude.ai sessionKey 탐색 ===")
    found: list[tuple[str, str, str]] = []  # (browser, profile, key)
    for name, user_data in BROWSERS:
        if not user_data.is_dir():
            continue
        print(f"[{name}] {user_data}")
        for profile, sk in find_session_keys(user_data):
            print(f"    {profile}: sessionKey {mask(sk)}")
            found.append((name, profile, sk))

    if not found:
        print("\n세션을 찾지 못했습니다.")
        print("- claude.ai 에 로그인돼 있어야 합니다.")
        print("- 최신 Chrome/Edge는 app-bound(v20) 암호화라 이 스파이크로는 못 읽을 수 있습니다.")
        sys.exit(1)

    browser, profile, sk = found[0]
    print(f"\n=== 엔드포인트 탐침 ({browser}/{profile}) ===")
    probe(sk)
    print("\nDONE")


if __name__ == "__main__":
    main()
