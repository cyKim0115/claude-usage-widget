//! 위젯 뒷면 유리 효과(배경 흐림).
//!
//! CSS backdrop-filter는 웹페이지 안만 흐려서 창 뒤의 바탕화면·다른 앱은 그대로 비칩니다.
//! Tauri windowEffects(blur)는 창 뒤를 흐리지만 창 사각형 전체에 깔려, 둥근 카드 밖
//! 귀퉁이까지 흐려집니다. 창 리전(SetWindowRgn)으로 깎아도 그 블러는 따라오지 않습니다.
//!
//! 그래서 블러를 직접 그립니다. 창에 HOSTBACKDROP accent를 켜고, Windows.UI.Composition의
//! HostBackdrop 브러시(시스템이 흐려 둔 창 뒤 화면)를 웹뷰 아래에 깐 뒤 카드와 같은
//! 스퀘어클로 자릅니다. 클립은 벡터라 가장자리가 매끈합니다. 블러 반경은 Windows Acrylic과 같고
//! 조절할 수 없어서, 「흐림 세기」는 블러 층의 불투명도로 흐린 화면과 원래 화면을 섞습니다.
//! 낮추면 흐려지지 않은 뒤 글자가 옅게 겹쳐 보입니다.
//!
//! Windows 10 1903~22H2에서만 켭니다(경로 클립이 1903부터). 포커스가 없어도 유지되고,
//! 끌 때 비용은 효과를 끈 창과 같습니다(19045 실측). Windows 11은 시험하지 않아 끕니다.
//! 근거와 실측: rag `research/technical-widget-glass-blur-2026-11-02.html`

use std::cell::RefCell;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use tauri::{PhysicalSize, WebviewWindow};
use windows::core::{implement, s, w, Interface, Ref, Result};
use windows::Graphics::{IGeometrySource2D, IGeometrySource2D_Impl};
use windows::System::DispatcherQueueController;
use windows::UI::Composition::Desktop::DesktopWindowTarget;
use windows::UI::Composition::{CompositionPath, CompositionPathGeometry, Compositor, SpriteVisual};
use windows::Win32::Foundation::{E_NOTIMPL, HWND};
use windows::Win32::Graphics::Direct2D::Common::{D2D1_FIGURE_BEGIN_FILLED, D2D1_FIGURE_END_CLOSED};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1Factory, ID2D1Geometry, D2D1_FACTORY_TYPE_MULTI_THREADED,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::System::WinRT::Composition::ICompositorDesktopInterop;
use windows::Win32::System::WinRT::Graphics::Direct2D::{
    IGeometrySource2DInterop, IGeometrySource2DInterop_Impl,
};
use windows::Win32::System::WinRT::{
    CreateDispatcherQueueController, DispatcherQueueOptions, DQTAT_COM_NONE, DQTYPE_THREAD_CURRENT,
};
use windows_numerics::Vector2;

/// Windows 10 1903. CompositionGeometricClip이 이 빌드부터 있습니다.
const MIN_BUILD: u32 = 18362;
/// Windows 11 첫 빌드.
const WINDOWS_11_BUILD: u32 = 22000;

/// styles.css `.widget`의 border-radius(CSS px)와 같아야 합니다.
const CORNER_RADIUS: f64 = 12.0;
/// CSS `corner-shape: squircle`은 superellipse(2), 즉 지수 4인 곡선입니다.
const SQUIRCLE_EXPONENT: f64 = 4.0;
/// 모서리 하나를 나누는 선분 수.
const CORNER_SEGMENTS: usize = 32;

/// SetWindowCompositionAttribute의 accent 상태.
const ACCENT_DISABLED: u32 = 0;
const ACCENT_ENABLE_HOSTBACKDROP: u32 = 5;
/// WCA_ACCENT_POLICY.
const WCA_ACCENT_POLICY: u32 = 0x13;

/// 효과가 켜져 있는지. 창 크기가 바뀔 때 블러를 다시 맞출지 정합니다.
static ACTIVE: AtomicBool = AtomicBool::new(false);
/// 흐림 세기(블러 층 불투명도, 0~1)를 f32 비트로 둡니다. 기본값 1.0.
static STRENGTH: AtomicU32 = AtomicU32::new(0x3F80_0000);

thread_local! {
    /// Composition 객체는 만든 스레드(메인 스레드)에서만 다룹니다.
    static BACKDROP: RefCell<Option<Backdrop>> = const { RefCell::new(None) };
}

struct Backdrop {
    _queue: DispatcherQueueController,
    _compositor: Compositor,
    /// 창 하나에 한 번만 만들 수 있어, 끌 때도 지우지 않고 visual만 숨깁니다.
    _target: DesktopWindowTarget,
    visual: SpriteVisual,
    clip: CompositionPathGeometry,
}

pub fn supported() -> bool {
    let build = windows_version::OsVersion::current().build;
    (MIN_BUILD..WINDOWS_11_BUILD).contains(&build)
}

/// 켜졌으면 true를 돌려줍니다. 끄거나 지원하지 않는 OS면 블러를 숨기고 false입니다.
pub fn apply(window: &WebviewWindow, enabled: bool) -> bool {
    let on = enabled && supported();
    ACTIVE.store(on, Ordering::Relaxed);
    let (Ok(hwnd), Ok(size), Ok(scale)) =
        (window.hwnd(), window.outer_size(), window.scale_factor())
    else {
        return false;
    };
    // HWND는 스레드 사이로 못 넘기므로 정수로 옮깁니다.
    let hwnd = hwnd.0 as isize;
    let _ = window.run_on_main_thread(move || {
        let hwnd = HWND(hwnd as *mut c_void);
        if on {
            if let Err(e) = show(hwnd, size, scale) {
                eprintln!("glass: {e}");
            }
        } else {
            hide(hwnd);
        }
    });
    on
}

/// 흐림 세기를 0~1로 받아 저장하고, 블러가 떠 있으면 바로 반영합니다.
pub fn set_strength(window: &WebviewWindow, strength: f32) {
    let strength = strength.clamp(0.0, 1.0);
    STRENGTH.store(strength.to_bits(), Ordering::Relaxed);
    let _ = window.run_on_main_thread(move || {
        BACKDROP.with(|cell| {
            if let Some(backdrop) = cell.borrow().as_ref() {
                let _ = backdrop.visual.SetOpacity(strength);
            }
        });
    });
}

fn strength() -> f32 {
    f32::from_bits(STRENGTH.load(Ordering::Relaxed))
}

/// 위젯 높이는 렌더마다 바뀝니다. 블러 크기와 클립도 그때마다 맞춥니다.
/// on_window_event는 메인 스레드에서 불립니다.
pub fn on_resized(window: &tauri::Window, size: PhysicalSize<u32>) {
    if !ACTIVE.load(Ordering::Relaxed) {
        return;
    }
    let Ok(scale) = window.scale_factor() else {
        return;
    };
    BACKDROP.with(|cell| {
        if let Some(backdrop) = cell.borrow().as_ref() {
            if let Err(e) = backdrop.fit(size, scale) {
                eprintln!("glass: {e}");
            }
        }
    });
}

fn show(hwnd: HWND, size: PhysicalSize<u32>, scale: f64) -> Result<()> {
    set_accent(hwnd, ACCENT_ENABLE_HOSTBACKDROP);
    BACKDROP.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            *slot = Some(Backdrop::new(hwnd)?);
        }
        let backdrop = slot.as_ref().expect("backdrop was just created");
        backdrop.fit(size, scale)?;
        backdrop.visual.SetOpacity(strength())?;
        backdrop.visual.SetIsVisible(true)
    })
}

fn hide(hwnd: HWND) {
    set_accent(hwnd, ACCENT_DISABLED);
    BACKDROP.with(|cell| {
        if let Some(backdrop) = cell.borrow().as_ref() {
            let _ = backdrop.visual.SetIsVisible(false);
        }
    });
}

impl Backdrop {
    fn new(hwnd: HWND) -> Result<Self> {
        // Compositor는 스레드에 DispatcherQueue가 있어야 합니다. Tauri 메인 스레드는 이미
        // 메시지 루프와 COM을 돌리고 있어, 그 큐에 붙기만 합니다.
        let queue = unsafe {
            CreateDispatcherQueueController(DispatcherQueueOptions {
                dwSize: std::mem::size_of::<DispatcherQueueOptions>() as u32,
                threadType: DQTYPE_THREAD_CURRENT,
                apartmentType: DQTAT_COM_NONE,
            })?
        };
        let compositor = Compositor::new()?;
        let interop: ICompositorDesktopInterop = compositor.cast()?;
        // false: 창 내용(웹뷰) 아래에 깝니다.
        let target = unsafe { interop.CreateDesktopWindowTarget(hwnd, false)? };
        let visual = compositor.CreateSpriteVisual()?;
        visual.SetBrush(&compositor.CreateHostBackdropBrush()?)?;
        let clip = compositor.CreatePathGeometry()?;
        visual.SetClip(&compositor.CreateGeometricClipWithGeometry(&clip)?)?;
        target.SetRoot(&visual)?;
        Ok(Self {
            _queue: queue,
            _compositor: compositor,
            _target: target,
            visual,
            clip,
        })
    }

    /// 창 픽셀 크기에 맞춰 블러와 스퀘어클 클립을 다시 잡습니다.
    fn fit(&self, size: PhysicalSize<u32>, scale: f64) -> Result<()> {
        let (width, height) = (f64::from(size.width), f64::from(size.height));
        self.visual.SetSize(Vector2 {
            X: width as f32,
            Y: height as f32,
        })?;
        let source: IGeometrySource2D = Squircle {
            width,
            height,
            radius: CORNER_RADIUS * scale,
        }
        .into();
        self.clip.SetPath(&CompositionPath::Create(&source)?)
    }
}

/// CompositionPath에 넘길 스퀘어클 도형. Composition은 이 인터페이스로 D2D 도형을 받아 갑니다.
#[implement(IGeometrySource2D, IGeometrySource2DInterop)]
struct Squircle {
    width: f64,
    height: f64,
    radius: f64,
}

impl IGeometrySource2D_Impl for Squircle_Impl {}

impl IGeometrySource2DInterop_Impl for Squircle_Impl {
    fn GetGeometry(&self) -> Result<ID2D1Geometry> {
        // Composition은 렌더 스레드에서 도형을 씁니다. 단일 스레드 팩토리로 만들면 그려지지 않습니다.
        let factory: ID2D1Factory =
            unsafe { D2D1CreateFactory(D2D1_FACTORY_TYPE_MULTI_THREADED, None)? };
        self.build(&factory)
    }

    fn TryGetGeometryUsingFactory(&self, factory: Ref<'_, ID2D1Factory>) -> Result<ID2D1Geometry> {
        match factory.as_ref() {
            Some(factory) => self.build(factory),
            None => Err(E_NOTIMPL.into()),
        }
    }
}

impl Squircle {
    /// 창 좌표(픽셀) 기준 스퀘어클 외곽선. 왼쪽 위부터 시계 방향입니다.
    fn build(&self, factory: &ID2D1Factory) -> Result<ID2D1Geometry> {
        let (w, h, r) = (self.width, self.height, self.radius);
        let power = 2.0 / SQUIRCLE_EXPONENT;
        // (곡선 중심 x, y, x 방향, y 방향, 각도를 거꾸로 돌지)
        let corners = [
            (r, r, -1.0, -1.0, false),
            (w - r, r, 1.0, -1.0, true),
            (w - r, h - r, 1.0, 1.0, false),
            (r, h - r, -1.0, 1.0, true),
        ];
        let path = unsafe { factory.CreatePathGeometry()? };
        let sink = unsafe { path.Open()? };
        let mut started = false;
        for (cx, cy, sx, sy, reverse) in corners {
            for i in 0..=CORNER_SEGMENTS {
                let step = if reverse { CORNER_SEGMENTS - i } else { i };
                let t = step as f64 / CORNER_SEGMENTS as f64 * std::f64::consts::FRAC_PI_2;
                // 90°에서 cos가 아주 작은 음수가 되면 거듭제곱이 NaN이 되어 도형 전체가 깨집니다.
                let point = Vector2 {
                    X: (cx + sx * r * t.cos().max(0.0).powf(power)) as f32,
                    Y: (cy + sy * r * t.sin().max(0.0).powf(power)) as f32,
                };
                unsafe {
                    if started {
                        sink.AddLine(point);
                    } else {
                        sink.BeginFigure(point, D2D1_FIGURE_BEGIN_FILLED);
                        started = true;
                    }
                }
            }
        }
        unsafe {
            sink.EndFigure(D2D1_FIGURE_END_CLOSED);
            sink.Close()?;
        }
        path.cast()
    }
}

#[repr(C)]
struct AccentPolicy {
    state: u32,
    flags: u32,
    gradient_color: u32,
    animation_id: u32,
}

#[repr(C)]
struct CompositionAttributeData {
    attribute: u32,
    data: *mut c_void,
    size: usize,
}

/// user32의 비공개 함수라 import 라이브러리에 없어서 이름으로 찾아 부릅니다.
fn set_accent(hwnd: HWND, state: u32) {
    type SetWindowCompositionAttribute =
        unsafe extern "system" fn(HWND, *mut CompositionAttributeData) -> i32;
    unsafe {
        let Ok(user32) = GetModuleHandleW(w!("user32.dll")) else {
            return;
        };
        let Some(proc) = GetProcAddress(user32, s!("SetWindowCompositionAttribute")) else {
            return;
        };
        let set: SetWindowCompositionAttribute = std::mem::transmute(proc);
        let mut policy = AccentPolicy {
            state,
            flags: 0,
            gradient_color: 0,
            animation_id: 0,
        };
        let mut data = CompositionAttributeData {
            attribute: WCA_ACCENT_POLICY,
            data: &mut policy as *mut _ as *mut c_void,
            size: std::mem::size_of::<AccentPolicy>(),
        };
        set(hwnd, &mut data);
    }
}
