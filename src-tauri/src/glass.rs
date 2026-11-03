//! 위젯 뒷면 유리 효과(배경 흐림).
//!
//! OS 블러는 쓰지 않습니다. Tauri windowEffects(blur)는 창 사각형 전체에 깔려 둥근 카드 밖까지
//! 흐려지고, Composition HostBackdrop은 카드 모양으로 자를 수는 있지만 Windows가 Acrylic
//! 세기로 미리 흐려 둔 화면이라 반경을 바꿀 수 없습니다.
//!
//! 그래서 위젯 뒤 화면을 직접 캡처해 웹뷰로 넘기고, 카드 안에서 CSS `blur()`로 흐립니다.
//! 반경은 설정에서 고르고, 모서리는 CSS 스퀘어클이 자릅니다.
//!
//! 캡처에 위젯 자신이 찍히면 안 되므로 캡처하는 순간에만 WDA_EXCLUDEFROMCAPTURE를 켭니다.
//! 그 순간에는 사용자의 스크린샷·화면 공유에서도 위젯이 빠집니다. 그래서 화면이 그대로면
//! 초당 4번만 캡처하고, 바뀌거나 창을 옮길 때만 잠깐 초당 15번으로 올립니다.
//! WDA_EXCLUDEFROMCAPTURE가 Windows 10 2004(19041)부터라 그 아래에서는 끕니다.
//! 근거와 실측: rag `research/technical-widget-glass-blur-2026-11-02.html`

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use tauri::http::{Request, Response};
use tauri::{AppHandle, Emitter, Manager, WebviewWindow};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC,
    SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, SRCCOPY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowRect, IsIconic, IsWindowVisible, SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE,
    WDA_NONE,
};

/// Windows 10 2004. WDA_EXCLUDEFROMCAPTURE가 이 빌드부터 있습니다.
const MIN_BUILD: u32 = 19041;

/// 화면이 바뀐 직후와 창을 옮기는 동안의 캡처 간격.
const FAST_INTERVAL: Duration = Duration::from_millis(66);
/// 화면이 그대로일 때의 캡처 간격. 길수록 스크린샷에서 위젯이 빠질 일이 줄어듭니다.
const IDLE_INTERVAL: Duration = Duration::from_millis(250);
/// 마지막 변화 뒤 이 시간 동안은 빠른 간격을 유지합니다.
const FAST_HOLD: Duration = Duration::from_millis(1500);

/// 흐림 반경 상한(CSS px). 설정 슬라이더의 max와 같아야 합니다.
pub const MAX_BLUR_PX: f64 = 32.0;

/// 흐림 반경(CSS px)을 f32 비트로 둡니다. 캡처 여백을 정하는 데 씁니다. 기본 12px.
static BLUR_PX: AtomicU32 = AtomicU32::new(0x4140_0000);
/// 가장 최근 프레임 번호. 웹뷰가 받아 갈 프레임을 구분합니다.
static SEQ: AtomicU64 = AtomicU64::new(0);

static CAPTURE: Mutex<Option<Worker>> = Mutex::new(None);
static FRAME: Mutex<Option<Arc<Vec<u8>>>> = Mutex::new(None);

struct Worker {
    stop: Arc<AtomicBool>,
    wake: Arc<(Mutex<bool>, Condvar)>,
}

/// 웹뷰가 캡처 화면을 어디에 그릴지 알려 줍니다. 좌표와 크기는 물리 픽셀입니다.
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct FrameInfo {
    seq: u64,
    width: i32,
    height: i32,
    /// 창 왼쪽 위에서 캡처 영역 왼쪽 위까지의 거리(창 밖으로 나간 만큼).
    margin: i32,
}

pub fn supported() -> bool {
    windows_version::OsVersion::current().build >= MIN_BUILD
}

/// 켜졌으면 true를 돌려줍니다. 끄거나 지원하지 않는 OS면 캡처를 멈추고 false입니다.
pub fn apply(window: &WebviewWindow, enabled: bool) -> bool {
    let on = enabled && supported();
    let mut slot = CAPTURE.lock().unwrap();
    if on && slot.is_none() {
        let Ok(hwnd) = window.hwnd() else {
            return false;
        };
        let stop = Arc::new(AtomicBool::new(false));
        let wake = Arc::new((Mutex::new(false), Condvar::new()));
        let (app, hwnd) = (window.app_handle().clone(), hwnd.0 as isize);
        let (stop2, wake2) = (stop.clone(), wake.clone());
        std::thread::spawn(move || capture_loop(app, HWND(hwnd as *mut c_void), stop2, wake2));
        *slot = Some(Worker { stop, wake });
    } else if !on {
        if let Some(worker) = slot.take() {
            worker.stop.store(true, Ordering::Relaxed);
            worker.wake.1.notify_one();
        }
        *FRAME.lock().unwrap() = None;
    }
    on
}

/// 흐림 반경(CSS px). 캡처 여백이 바뀌므로 바로 다시 캡처합니다.
pub fn set_blur(px: f64) {
    let px = px.clamp(0.0, MAX_BLUR_PX) as f32;
    BLUR_PX.store(px.to_bits(), Ordering::Relaxed);
    wake();
}

/// 창을 옮기거나 크기가 바뀌면 기다리지 않고 바로 캡처합니다.
pub fn wake() {
    if let Some(worker) = CAPTURE.lock().unwrap().as_ref() {
        *worker.wake.0.lock().unwrap() = true;
        worker.wake.1.notify_one();
    }
}

/// 캡처 프레임을 읽어 갈 수 있는 페이지 출처. 빌드된 위젯(Windows)과 개발 서버입니다.
const FRAME_ORIGINS: [&str; 3] = [
    "http://tauri.localhost",
    "https://tauri.localhost",
    "http://localhost:1420",
];

/// `http://glassbg.localhost/frame`: 가장 최근 캡처의 RGBA 바이트.
///
/// 화면 캡처라 위젯 창(main)의 요청에만 답하고, CORS도 앱 자신의 출처에만 엽니다.
pub fn frame_response(webview_label: &str, request: Request<Vec<u8>>) -> Response<Vec<u8>> {
    let origin = request
        .headers()
        .get("Origin")
        .and_then(|value| value.to_str().ok())
        .filter(|origin| FRAME_ORIGINS.contains(origin));
    let (Some(origin), "main") = (origin, webview_label) else {
        return Response::builder().status(403).body(Vec::new()).unwrap();
    };
    let body = FRAME
        .lock()
        .unwrap()
        .as_ref()
        .map(|frame| frame.as_ref().clone())
        .unwrap_or_default();
    Response::builder()
        .header("Content-Type", "application/octet-stream")
        .header("Access-Control-Allow-Origin", origin)
        .header("Cache-Control", "no-store")
        .body(body)
        .unwrap()
}

fn capture_loop(app: AppHandle, hwnd: HWND, stop: Arc<AtomicBool>, wake: Arc<(Mutex<bool>, Condvar)>) {
    let mut last_hash = 0u64;
    let mut last_change = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        let started = Instant::now();
        if unsafe { IsWindowVisible(hwnd).as_bool() && !IsIconic(hwnd).as_bool() } {
            if let Some((frame, info)) = capture_once(hwnd) {
                let hash = sample_hash(&frame);
                if hash != last_hash {
                    last_hash = hash;
                    last_change = Instant::now();
                    *FRAME.lock().unwrap() = Some(Arc::new(frame));
                    let _ = app.emit_to("main", "glass-frame", info);
                }
            }
        }
        let interval = if last_change.elapsed() < FAST_HOLD {
            FAST_INTERVAL
        } else {
            IDLE_INTERVAL
        };
        let (lock, cvar) = &*wake;
        let mut woken = lock.lock().unwrap();
        let wait = interval.saturating_sub(started.elapsed());
        if !*woken {
            woken = cvar.wait_timeout(woken, wait).unwrap().0;
        }
        if *woken {
            // 옮기는 중에는 빠른 간격으로 따라갑니다.
            last_change = Instant::now();
            *woken = false;
        }
    }
    unsafe {
        let _ = SetWindowDisplayAffinity(hwnd, WDA_NONE);
    }
}

/// 창 주변을 여백까지 캡처합니다. 여백이 있어야 카드 가장자리에서도 블러가 비지 않습니다.
fn capture_once(hwnd: HWND) -> Option<(Vec<u8>, FrameInfo)> {
    let mut rect = RECT::default();
    unsafe { GetWindowRect(hwnd, &mut rect).ok()? };
    let scale = unsafe { windows::Win32::UI::HiDpi::GetDpiForWindow(hwnd) } as f64 / 96.0;
    let blur = f64::from(f32::from_bits(BLUR_PX.load(Ordering::Relaxed)));
    // CSS blur는 반경의 약 2배 거리까지 번집니다.
    let margin = ((blur * 2.0 + 4.0) * scale).ceil() as i32;
    let (width, height) = (
        rect.right - rect.left + 2 * margin,
        rect.bottom - rect.top + 2 * margin,
    );
    let frame = unsafe {
        let _ = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);
        let frame = grab(rect.left - margin, rect.top - margin, width, height);
        let _ = SetWindowDisplayAffinity(hwnd, WDA_NONE);
        frame?
    };
    let seq = SEQ.fetch_add(1, Ordering::Relaxed) + 1;
    Some((
        frame,
        FrameInfo {
            seq,
            width,
            height,
            margin,
        },
    ))
}

/// 화면 사각형을 RGBA로 읽습니다.
unsafe fn grab(x: i32, y: i32, width: i32, height: i32) -> Option<Vec<u8>> {
    let screen = GetDC(None);
    let memory = CreateCompatibleDC(Some(screen));
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            // 음수면 위에서 아래로 쌓입니다.
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut c_void = std::ptr::null_mut();
    let mut rgba = None;
    if let Ok(bitmap) = CreateDIBSection(Some(memory), &info, DIB_RGB_COLORS, &mut bits, None, 0) {
        let previous = SelectObject(memory, bitmap.into());
        if BitBlt(memory, 0, 0, width, height, Some(screen), x, y, SRCCOPY).is_ok() && !bits.is_null() {
            let bgra = std::slice::from_raw_parts(bits as *const u8, (width * height * 4) as usize);
            let mut out = Vec::with_capacity(bgra.len());
            for pixel in bgra.chunks_exact(4) {
                out.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 255]);
            }
            rgba = Some(out);
        }
        SelectObject(memory, previous);
        let _ = DeleteObject(bitmap.into());
    }
    let _ = DeleteDC(memory);
    ReleaseDC(None, screen);
    rgba
}

/// 프레임이 바뀌었는지만 보려는 성긴 해시(FNV-1a, 97바이트마다 하나).
fn sample_hash(frame: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in frame.iter().step_by(97) {
        hash = (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3);
    }
    hash ^ frame.len() as u64
}
