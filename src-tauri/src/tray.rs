//! 알림 영역(트레이) 아이콘.
//!
//! 위젯 창은 작업 표시줄 단추가 없어서, 다른 창에 가려지거나 숨긴 위젯을 꺼낼
//! 자리를 여기에 둔다. 왼쪽 클릭은 위젯 보이기/숨기기, 오른쪽 클릭은 메뉴다.

use tauri::{
    menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent},
    AppHandle, Manager,
};

const MAIN_LABEL: &str = "main";
const TRAY_ID: &str = "main";
const MENU_SHOW: &str = "tray-show";
const MENU_HIDE: &str = "tray-hide";
const MENU_SETTINGS: &str = "tray-settings";
const MENU_QUIT: &str = "tray-quit";

pub fn install(app: &tauri::App) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, MENU_SHOW, "위젯 보이기", true, None::<&str>)?;
    let hide = MenuItem::with_id(app, MENU_HIDE, "위젯 숨기기", true, None::<&str>)?;
    let settings = MenuItem::with_id(app, MENU_SETTINGS, "설정", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, MENU_QUIT, "종료", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[
            &show,
            &hide,
            &PredefinedMenuItem::separator(app)?,
            &settings,
            &PredefinedMenuItem::separator(app)?,
            &quit,
        ],
    )?;

    let mut builder = TrayIconBuilder::with_id(TRAY_ID)
        .tooltip(&app.package_info().name)
        .menu(&menu)
        // 왼쪽 클릭은 메뉴 대신 보이기/숨기기에 쓴다.
        .show_menu_on_left_click(false)
        .on_menu_event(on_menu_event)
        .on_tray_icon_event(on_tray_icon_event);
    // icon.ico 의 첫 항목(16px)이 기본 창 아이콘이라 트레이 크기에 그대로 맞는다.
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    builder.build(app)?;
    Ok(())
}

fn on_menu_event(app: &AppHandle, event: MenuEvent) {
    match event.id().as_ref() {
        MENU_SHOW => show_widget(app),
        MENU_HIDE => hide_widget(app),
        MENU_SETTINGS => {
            let _ = super::open_settings_window(app.clone());
        }
        MENU_QUIT => app.exit(0),
        _ => {}
    }
}

fn on_tray_icon_event(tray: &TrayIcon, event: TrayIconEvent) {
    if let TrayIconEvent::Click {
        button: MouseButton::Left,
        button_state: MouseButtonState::Up,
        ..
    } = event
    {
        let app = tray.app_handle();
        let visible = app
            .get_webview_window(MAIN_LABEL)
            .and_then(|window| window.is_visible().ok())
            .unwrap_or(false);
        if visible {
            hide_widget(app);
        } else {
            show_widget(app);
        }
    }
}

fn show_widget(app: &AppHandle) {
    let Some(window) = app.get_webview_window(MAIN_LABEL) else {
        return;
    };
    let _ = window.show();
    let _ = window.unminimize();
    let _ = window.set_focus();
    // tao 는 창을 만들 때만 작업 표시줄 단추를 지운다. 다시 띄운 뒤에도 단추가
    // 붙지 않게 한 번 더 지운다.
    let _ = window.set_skip_taskbar(true);
}

fn hide_widget(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(MAIN_LABEL) {
        let _ = window.hide();
    }
}
