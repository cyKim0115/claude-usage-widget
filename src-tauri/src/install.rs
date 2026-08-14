use auto_launch::{AutoLaunch, AutoLaunchBuilder};
use std::fs;
use std::path::{Path, PathBuf};

const APP_FOLDER: &str = "ClaudeUsageWidget";
const EXE_NAME: &str = "claude-usage-widget.exe";
const AUTOSTART_NAME: &str = "Claude Usage Widget";

pub fn install_dir() -> Result<PathBuf, String> {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .ok_or_else(|| "LOCALAPPDATA를 찾을 수 없습니다.".to_string())?;
    Ok(base.join(APP_FOLDER))
}

pub fn installed_exe_path() -> Result<PathBuf, String> {
    Ok(install_dir()?.join(EXE_NAME))
}

fn is_debug_exe(path: &Path) -> bool {
    path.components().any(|c| c.as_os_str() == "debug")
}

#[cfg(debug_assertions)]
fn release_candidate_near(current: &Path) -> Option<PathBuf> {
    // .../target/debug/foo.exe → .../target/release/foo.exe
    let parent = current.parent()?;
    if parent.file_name()?.to_string_lossy() != "debug" {
        return None;
    }
    let candidate = parent.parent()?.join("release").join(EXE_NAME);
    candidate.exists().then_some(candidate)
}

/// Prefer installed release binary; otherwise copy a suitable source into LOCALAPPDATA.
pub fn ensure_installed_release() -> Result<PathBuf, String> {
    let dest = installed_exe_path()?;
    let dest_dir = install_dir()?;
    fs::create_dir_all(&dest_dir).map_err(|e| format!("설치 폴더 생성 실패: {e}"))?;
    let current = std::env::current_exe().map_err(|e| e.to_string())?;

    #[cfg(debug_assertions)]
    {
        if dest.exists() && !is_debug_exe(&dest) {
            return Ok(dest);
        }
        if let Some(release) = release_candidate_near(&current) {
            fs::copy(&release, &dest).map_err(|e| format!("릴리스 복사 실패: {e}"))?;
            return Ok(dest);
        }
        return Err(
            "시작프로그램/바로가기는 릴리스 실행 파일이 필요합니다.\n\n프로젝트의 「시작.bat」을 한 번 실행해 설치해 주세요."
                .into(),
        );
    }

    #[cfg(not(debug_assertions))]
    {
        let need_copy = match fs::metadata(&dest) {
            Ok(meta) => {
                let src_meta = fs::metadata(&current).map_err(|e| e.to_string())?;
                meta.len() != src_meta.len()
                    || meta.modified().ok() != src_meta.modified().ok()
                    || is_debug_exe(&dest)
            }
            Err(_) => true,
        };
        if need_copy {
            fs::copy(&current, &dest).map_err(|e| format!("설치 복사 실패: {e}"))?;
        }
        Ok(dest)
    }
}

fn build_auto_launch(exe: &Path) -> Result<AutoLaunch, String> {
    AutoLaunchBuilder::new()
        .set_app_name(AUTOSTART_NAME)
        .set_app_path(&exe.display().to_string())
        .set_args(&["--from-autostart"])
        .build()
        .map_err(|e| e.to_string())
}

pub fn autostart_enable() -> Result<(), String> {
    let exe = ensure_installed_release()?;
    if is_debug_exe(&exe) {
        return Err("디버그 실행 파일은 시작프로그램에 등록할 수 없습니다.".into());
    }
    build_auto_launch(&exe)?
        .enable()
        .map_err(|e| format!("시작프로그램 등록 실패: {e}"))
}

fn disable_named(name: &str, exe: &Path) {
    let _ = AutoLaunchBuilder::new()
        .set_app_name(name)
        .set_app_path(&exe.display().to_string())
        .set_args(&[] as &[&str])
        .build()
        .ok()
        .and_then(|al| al.disable().ok());
}

pub fn autostart_disable() -> Result<(), String> {
    let installed = installed_exe_path().unwrap_or_else(|_| PathBuf::from(EXE_NAME));
    let current = std::env::current_exe().unwrap_or_else(|_| installed.clone());
    for name in [AUTOSTART_NAME, "claude-usage-widget"] {
        disable_named(name, &installed);
        disable_named(name, &current);
    }
    Ok(())
}

/// Remove mistaken debug-exe autostart entries left by older builds.
pub fn cleanup_stale_debug_autostart() {
    #[cfg(windows)]
    {
        use winreg::enums::{HKEY_CURRENT_USER, KEY_ALL_ACCESS};
        use winreg::RegKey;
        if let Ok(key) = RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(
            r"Software\Microsoft\Windows\CurrentVersion\Run",
            KEY_ALL_ACCESS,
        ) {
            for name in [AUTOSTART_NAME, "claude-usage-widget"] {
                if let Ok(val) = key.get_value::<String, _>(name) {
                    let lower = val.to_lowercase();
                    if lower.contains(r"\debug\") || lower.contains("/debug/") {
                        let _ = key.delete_value(name);
                    }
                }
            }
        }
    }

    let Ok(current) = std::env::current_exe() else {
        return;
    };
    if !is_debug_exe(&current) {
        return;
    }
    for name in [AUTOSTART_NAME, "claude-usage-widget"] {
        if let Ok(al) = AutoLaunchBuilder::new()
            .set_app_name(name)
            .set_app_path(&current.display().to_string())
            .set_args(&[] as &[&str])
            .build()
        {
            if al.is_enabled().unwrap_or(false) {
                let _ = al.disable();
            }
        }
    }
}

pub fn autostart_is_enabled() -> Result<bool, String> {
    let exe = installed_exe_path().unwrap_or_else(|_| PathBuf::from(EXE_NAME));
    match build_auto_launch(&exe) {
        Ok(al) => al.is_enabled().map_err(|e| e.to_string()),
        Err(_) => Ok(false),
    }
}

#[cfg(debug_assertions)]
fn vite_dev_server_up() -> bool {
    use std::net::TcpStream;
    use std::time::Duration;
    TcpStream::connect_timeout(
        &"127.0.0.1:1420".parse().expect("static addr"),
        Duration::from_millis(400),
    )
    .is_ok()
}

#[cfg(all(debug_assertions, windows))]
fn show_native_error(title: &str, message: &str) {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use std::ptr::null_mut;

    #[link(name = "user32")]
    extern "system" {
        fn MessageBoxW(
            hwnd: *mut core::ffi::c_void,
            text: *const u16,
            caption: *const u16,
            flags: u32,
        ) -> i32;
    }

    fn wide(s: &str) -> Vec<u16> {
        OsStr::new(s).encode_wide().chain(Some(0)).collect()
    }

    let title_w = wide(title);
    let message_w = wide(message);
    unsafe {
        MessageBoxW(null_mut(), message_w.as_ptr(), title_w.as_ptr(), 0x10);
    }
}

#[cfg(all(debug_assertions, not(windows)))]
fn show_native_error(title: &str, message: &str) {
    eprintln!("{title}: {message}");
}

/// If this is a debug binary started without `tauri dev` (no Vite), explain and exit.
pub fn guard_debug_requires_vite() {
    #[cfg(debug_assertions)]
    {
        if vite_dev_server_up() {
            return;
        }
        show_native_error(
            "Claude Usage Widget",
            "이 실행 파일은 개발용(debug)이라 혼자서는 화면이 나오지 않습니다.\n\n\
대신 프로젝트 폴더의 「시작.bat」을 실행하세요.\n\
(처음 한 번은 릴리스 빌드 후 %LOCALAPPDATA%\\ClaudeUsageWidget 에 설치됩니다.)\n\n\
개발 중이면 터미널에서:\nnpm run tauri dev",
        );
        std::process::exit(1);
    }
}
