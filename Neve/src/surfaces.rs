//! Deteção de janelas e da barra de tarefas via Win32.
//!
//! Estratégia:
//! * `EnumWindows` devolve todas as janelas de topo por ordem de Z-order.
//! * Filtram-se janelas invisíveis/minimizadas (`IsWindowVisible`, `IsIconic`),
//!   "cloaked" (`DwmGetWindowAttribute(DWMWA_CLOAKED)` — janelas de outros
//!   desktops virtuais), janelas filho, ferramentas/no-activate e overlays
//!   alheios.
//! * Os limites visíveis vêm de `DWMWA_EXTENDED_FRAME_BOUNDS`, que exclui a
//!   borda de redimensionamento invisível e a sombra DWM (com `GetWindowRect`
//!   como fallback). Isto evita que a neve flutue alguns píxeis acima da janela.
//! * A barra de tarefas é localizada com `FindWindowW("Shell_TrayWnd")`; as
//!   barras dos monitores secundários usam a classe `Shell_SecondaryTrayWnd`.
//!
//! O varrimento completo corre ~7 vezes por segundo (0,14 s) num thread
//! separado? Não: é barato o suficiente e evita sincronização — mas fica
//! limitado a uma janela de tempo para não competir com o render.

use std::mem::size_of;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS,
};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, GetWindowLongW, GetWindowRect, IsIconic, IsWindowVisible, GWL_EXSTYLE, GWL_STYLE,
    WS_CHILD, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT,
};

use crate::geometry::Rect;
use crate::sim::{Surface, SurfaceKind};
use crate::winutil::{class_name, top_level_windows, window_rect};

const REFRESH_INTERVAL: f32 = 0.14;

pub struct Tracker {
    pub surfaces: Vec<Surface>,
    timer: f32,
    include_windows: bool,
    /// HWND do overlay e da janela de mensagens (nunca devem ser tratados como
    /// superfícies).
    own: [isize; 2],
    force: bool,
    /// Diagnóstico: quantos varrimentos iniciais ainda devem ser registados.
    dump_left: u8,
}

impl Tracker {
    pub fn new(own_overlay: isize, own_msg: isize) -> Self {
        Self {
            surfaces: Vec::with_capacity(48),
            timer: REFRESH_INTERVAL,
            include_windows: true,
            own: [own_overlay, own_msg],
            force: false,
            dump_left: 2,
        }
    }

    pub fn set_own(&mut self, overlay: isize, msg: isize) {
        self.own = [overlay, msg];
        self.force = true;
    }

    pub fn set_include_windows(&mut self, value: bool) {
        if self.include_windows != value {
            self.include_windows = value;
            self.force = true;
        }
    }

    pub fn force_refresh(&mut self) {
        self.force = true;
    }

    pub fn update(&mut self, dt: f32, origin: (i32, i32)) {
        self.timer += dt;
        if !self.force && self.timer < REFRESH_INTERVAL {
            return;
        }
        self.timer = 0.0;
        self.force = false;
        self.refresh(origin);
    }

    fn refresh(&mut self, origin: (i32, i32)) {
        self.surfaces.clear();

        // --- barra de tarefas principal ------------------------------------
        // A barra de tarefas é uma janela topmost: z = 0 (acima das janelas
        // normais). O `FindWindowW` é a forma documentada de a localizar.
        let mut main_tray = HWND::default();
        if let Ok(hwnd) = unsafe { FindWindowW(w!("Shell_TrayWnd"), PCWSTR::null()) } {
            if !hwnd.is_invalid() {
                main_tray = hwnd;
                if let Some(r) = window_rect(hwnd) {
                    self.push_taskbar(hwnd, r, origin);
                }
            }
        }

        // --- restantes janelas de topo -------------------------------------
        // `EnumWindows` devolve as janelas por ordem Z (topmost primeiro), o
        // que permite numerar a oclusão usada pelo renderer.
        let mut z = 1;
        for hwnd in top_level_windows() {
            if hwnd == main_tray {
                continue; // já tratada acima
            }
            let cls = class_name(hwnd);
            if cls == "Shell_SecondaryTrayWnd" {
                if let Some(r) = window_rect(hwnd) {
                    self.push_taskbar(hwnd, r, origin);
                }
                continue;
            }
            if !self.include_windows {
                continue;
            }
            if let Some(mut surface) = self.window_surface(hwnd, &cls, origin) {
                surface.z = z;
                z += 1;
                self.surfaces.push(surface);
            }
        }

        if self.dump_left > 0 && std::env::var_os("SNOWHUSH_LOG").is_some() {
            self.dump_left -= 1;
            for s in &self.surfaces {
                crate::winutil::log(&format!(
                    "superfície z={} {:?} x={:.0} y={:.0} w={:.0} h={:.0}",
                    s.z, s.kind, s.rect.x, s.rect.y, s.rect.w, s.rect.h
                ));
            }
        }
    }

    fn push_taskbar(&mut self, hwnd: HWND, r: RECT, origin: (i32, i32)) {
        let rect = rect_from(r, origin);
        if rect.w >= 8.0 && rect.h >= 8.0 {
            self.surfaces.push(Surface {
                id: hwnd.0 as isize,
                kind: SurfaceKind::Taskbar,
                rect,
                z: 0,
            });
        }
    }

    fn window_surface(&self, hwnd: HWND, cls: &str, origin: (i32, i32)) -> Option<Surface> {
        let id = hwnd.0 as isize;
        if id == self.own[0] || id == self.own[1] {
            return None;
        }
        if is_ignored_class(cls) {
            return None;
        }

        unsafe {
            if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
                return None;
            }

            let ex = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
            if ex & (WS_EX_TOOLWINDOW.0 | WS_EX_NOACTIVATE.0 | WS_EX_TRANSPARENT.0) != 0 {
                return None;
            }
            let style = GetWindowLongW(hwnd, GWL_STYLE) as u32;
            if style & WS_CHILD.0 != 0 {
                return None;
            }

            // Janelas "cloaked" (outro desktop virtual, UWP suspensa) não são
            // visíveis apesar de IsWindowVisible devolver TRUE.
            let mut cloaked: u32 = 0;
            if DwmGetWindowAttribute(
                hwnd,
                DWMWA_CLOAKED,
                &mut cloaked as *mut u32 as *mut _,
                size_of::<u32>() as u32,
            )
            .is_ok()
                && cloaked != 0
            {
                return None;
            }

            let mut r = RECT::default();
            let dwm_ok = DwmGetWindowAttribute(
                hwnd,
                DWMWA_EXTENDED_FRAME_BOUNDS,
                &mut r as *mut RECT as *mut _,
                size_of::<RECT>() as u32,
            )
            .is_ok();
            if !dwm_ok {
                GetWindowRect(hwnd, &mut r).ok()?;
            }

            if r.right - r.left < 40 || r.bottom - r.top < 24 {
                return None;
            }

            let rect = rect_from(r, origin);
            // Janelas completamente fora do ecrã virtual não interessam.
            if rect.right() < 0.0 || rect.bottom() < 0.0 {
                return None;
            }

            Some(Surface {
                id,
                kind: SurfaceKind::Window,
                rect,
                z: 0,
            })
        }
    }
}

fn rect_from(r: RECT, origin: (i32, i32)) -> Rect {
    Rect::new(
        (r.left - origin.0) as f32,
        (r.top - origin.1) as f32,
        (r.right - r.left) as f32,
        (r.bottom - r.top) as f32,
    )
}

/// Classes de janelas do shell que não devem receber neve (ocupam o ecrã todo
/// ou são superfícies internas do Explorador).
fn is_ignored_class(cls: &str) -> bool {
    matches!(
        cls,
        "Progman"
            | "WorkerW"
            | "Shell_TrayWnd"
            | "Shell_SecondaryTrayWnd"
            | "Windows.UI.Core.CoreWindow"
            | "ApplicationManager_DesktopShellWindow"
            | "Shell_InputSwitchTopLevelWindow"
            | "MultitaskingViewFrame"
            | "TaskListThumbnailWnd"
            | "XamlExplorerHostIslandWindow"
            | "ForegroundStaging"
            | "SysShadow"
    )
}
