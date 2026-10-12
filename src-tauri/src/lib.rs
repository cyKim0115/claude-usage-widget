mod auth;
mod browser_login;
mod install;
mod secure_store;
mod usage;
mod web_session;

use auth::{default_credentials_path, read_credentials};
use install::{
    autostart_disable, autostart_enable, autostart_is_enabled, cleanup_stale_debug_autostart,
    ensure_installed_release, guard_debug_requires_vite,
};
use tauri::{Emitter, Manager, WindowEvent};
use tauri_plugin_window_state::StateFlags;
use usage::{
    fetch_error, fetch_usage, need_login, UsageError, UsageSnapshot, SOURCE_CLAUDE_CODE, SOURCE_WEB,
};
use web_session::WebStatus;

const POLL_INTERVAL_MS: u64 = 300_000;
const SETTINGS_LABEL: &str = "settings";

fn usage_via_claude_code() -> UsageSnapshot {
    let path = default_credentials_path();
    match read_credentials(&path) {
        Ok(cred) => match fetch_usage(&cred.access_token) {
            Ok(snap) => snap,
            // 만료 전 토큰이라도 서버가 거부할 수 있습니다. 그때도 갱신 실패가
            // 아니라 로그인 문제로 안내해야 사용자가 할 일이 분명해집니다.
            Err(UsageError::Unauthorized) => {
                need_login(SOURCE_CLAUDE_CODE, UsageError::Unauthorized.to_string())
            }
            Err(e) => fetch_error(SOURCE_CLAUDE_CODE, e.to_string()),
        },
        Err(e) => need_login(SOURCE_CLAUDE_CODE, e.to_string()),
    }
}

/// 데이터 소스 선택. `auto`는 웹 세션이 있으면 웹 세션, 없으면 Claude Code OAuth입니다.
/// 웹 세션이 로그인 필요로 끝나면 Claude Code로 한 번 더 시도해 보여 줄 값이 있으면 씁니다.
fn usage_for(source: &str) -> UsageSnapshot {
    match source {
        SOURCE_CLAUDE_CODE => usage_via_claude_code(),
        SOURCE_WEB => web_session::usage(),
        _ => {
            if !web_session::has_session() {
                return usage_via_claude_code();
            }
            let web = web_session::usage();
            if web.state != "NeedLogin" {
                return web;
            }
            let cli = usage_via_claude_code();
            if cli.state == "OK" {
                cli
            } else {
                web
            }
        }
    }
}

/// 웹 세션은 401 때 headless 갱신(최대 20초)까지 갈 수 있어 메인 스레드 밖에서 돕니다.
#[tauri::command]
async fn get_usage(source: Option<String>) -> UsageSnapshot {
    let source = source.unwrap_or_default();
    tauri::async_runtime::spawn_blocking(move || usage_for(&source))
        .await
        .unwrap_or_else(|e| fetch_error(SOURCE_CLAUDE_CODE, format!("FetchError: {e}")))
}

#[tauri::command]
fn get_web_status() -> WebStatus {
    web_session::status()
}

/// 로그인 창을 띄우고 사용자가 로그인 후 창을 닫을 때까지 기다립니다(최대 10분).
/// 진행 문구는 `web-login-status` 이벤트로 설정 창에 보냅니다.
#[tauri::command]
async fn web_login(app: tauri::AppHandle) -> Result<WebStatus, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let emit = |message: &str| {
            let _ = app.emit("web-login-status", message);
        };
        web_session::login(&emit)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
fn web_disconnect() -> Result<WebStatus, String> {
    web_session::disconnect()?;
    Ok(web_session::status())
}

#[tauri::command]
async fn web_reset_profile() -> Result<WebStatus, String> {
    tauri::async_runtime::spawn_blocking(|| {
        web_session::reset_profile()?;
        Ok(web_session::status())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
fn get_poll_interval_ms() -> u64 {
    POLL_INTERVAL_MS
}

#[tauri::command]
fn quit_app(app: tauri::AppHandle) {
    app.exit(0);
}

/// 설정 창은 tauri.conf.json에 `visible: false`로 미리 선언해 두고 show/hide만
/// 토글한다. 클릭마다 새로 만들면 WebView2 초기화 지연이 그대로 노출된다.
/// 숨겨 뒀던 창을 다시 쓰는 것이라, 값을 새로 읽으라고 이벤트도 함께 보낸다.
#[tauri::command]
fn open_settings_window(app: tauri::AppHandle) -> Result<(), String> {
    let window = app
        .get_webview_window(SETTINGS_LABEL)
        .ok_or_else(|| "settings window not found".to_string())?;
    window.show().map_err(|e| e.to_string())?;
    window.unminimize().map_err(|e| e.to_string())?;
    window.set_focus().map_err(|e| e.to_string())?;
    window
        .emit("settings-open", ())
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
fn close_settings_window(app: tauri::AppHandle) -> Result<(), String> {
    if let Some(window) = app.get_webview_window(SETTINGS_LABEL) {
        window.hide().map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// The preference lives in the webview, so the menu and startup both push it
/// onto the main window through here.
#[tauri::command]
fn set_always_on_top(app: tauri::AppHandle, enabled: bool) -> Result<(), String> {
    let window = app
        .get_webview_window("main")
        .ok_or_else(|| "main window not found".to_string())?;
    window.set_always_on_top(enabled).map_err(|e| e.to_string())
}

#[tauri::command]
fn is_dev_build() -> bool {
    cfg!(debug_assertions)
}

#[tauri::command]
fn enable_autostart() -> Result<(), String> {
    autostart_enable()
}

#[tauri::command]
fn disable_autostart() -> Result<(), String> {
    autostart_disable()
}

#[tauri::command]
fn is_autostart_enabled() -> Result<bool, String> {
    autostart_is_enabled()
}

#[tauri::command]
fn install_release_copy() -> Result<String, String> {
    ensure_installed_release().map(|p| p.display().to_string())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    guard_debug_requires_vite();

    tauri::Builder::default()
        // 다른 플러그인보다 먼저 등록해야 두 번째 실행이 초기화 전에 끝납니다.
        // 작업 표시줄 버튼이 없어 가려진 위젯을 꺼낼 길이 재실행뿐이라,
        // 새로 띄우지 않고 기존 창을 앞으로 가져옵니다.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.unminimize();
                let _ = window.set_focus();
            }
        }))
        // 위치만 복원합니다. 창 높이는 트랙 수에 따라 프런트가 정하므로,
        // 크기까지 저장하면 지난 실행의 트랙 수가 이번 실행을 덮어씁니다.
        .plugin(
            tauri_plugin_window_state::Builder::default()
                .with_state_flags(StateFlags::POSITION)
                .with_denylist(&[SETTINGS_LABEL])
                .build(),
        )
        .invoke_handler(tauri::generate_handler![
            get_usage,
            get_poll_interval_ms,
            quit_app,
            open_settings_window,
            close_settings_window,
            is_dev_build,
            enable_autostart,
            disable_autostart,
            is_autostart_enabled,
            install_release_copy,
            set_always_on_top,
            get_web_status,
            web_login,
            web_disconnect,
            web_reset_profile
        ])
        .on_window_event(|window, event| {
            if window.label() != SETTINGS_LABEL {
                return;
            }
            // 설정 창을 닫아도 앱은 살아 있어야 한다. 파괴하지 않고 숨긴다.
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .setup(|_app| {
            cleanup_stale_debug_autostart();
            // Release builds keep a stable copy under LOCALAPPDATA for shortcuts/autostart.
            let _ = ensure_installed_release();
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
