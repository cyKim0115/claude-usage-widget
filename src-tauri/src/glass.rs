//! 위젯 뒷면 유리 효과(배경 흐림).
//!
//! CSS backdrop-filter는 웹페이지 안만 흐려서 창 뒤의 바탕화면·다른 앱은 그대로 비칩니다.
//! 그래서 OS 창 효과(Tauri windowEffects, 내부는 window-vibrancy)를 씁니다.
//!
//! Windows 10 1809~22H2에서만 켭니다. 이 범위의 blur는 SetWindowCompositionAttribute로 그려져
//! 포커스가 없어도 유지되고, 끌 때도 효과를 끈 창과 같은 속도로 움직입니다.
//! Windows 11은 acrylic·mica가 포커스를 잃으면 단색으로 바뀌고 blur도 22H1 이후 깨지거나
//! 끌 때 버벅입니다. 위젯은 거의 늘 포커스가 없어 맞지 않습니다.
//! 근거와 실측: rag `research/technical-widget-glass-blur-2026-11-02.html`

use tauri::utils::config::WindowEffectsConfig;
use tauri::window::{Color, Effect, EffectsBuilder};
use tauri::WebviewWindow;

/// Windows 10 1809. 이보다 낮으면 window-vibrancy가 SWCA를 쓰지 않습니다.
const MIN_BUILD: u32 = 17763;
/// Windows 11 첫 빌드.
const WINDOWS_11_BUILD: u32 = 22000;

pub fn supported() -> bool {
    let build = windows_version::OsVersion::current().build;
    (MIN_BUILD..WINDOWS_11_BUILD).contains(&build)
}

/// 켜졌으면 true를 돌려줍니다. 끄거나 지원하지 않는 OS면 효과를 지우고 false입니다.
///
/// Tauri는 효과 적용 실패를 알려 주지 않습니다. 그래서 성공 여부는 OS 빌드로만 판단합니다.
pub fn apply(window: &WebviewWindow, enabled: bool) -> bool {
    if enabled && supported() {
        // 틴트는 CSS 카드(--bg-glass)가 맡습니다. 여기 색에 알파를 주면 둥근 카드 밖
        // 귀퉁이까지 창 사각형 전체가 칠해져 회색 사각형이 뚜렷해집니다.
        let effects = EffectsBuilder::new()
            .effect(Effect::Blur)
            .color(Color(22, 24, 28, 0))
            .build();
        let _ = window.set_effects(effects);
        true
    } else {
        let _ = window.set_effects(None::<WindowEffectsConfig>);
        false
    }
}
