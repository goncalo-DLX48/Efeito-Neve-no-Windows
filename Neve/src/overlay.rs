//! Janela de overlay: um único HWND sem bordas que cobre todo o ecrã virtual.
//!
//! Estilos importantes:
//! * `WS_POPUP`        — sem moldura nem barra de título.
//! * `WS_EX_TOOLWINDOW`— não aparece no Alt+Tab nem na barra de tarefas.
//! * `WS_EX_NOACTIVATE`— nunca rouba o foco ao clicar.
//! * `WS_EX_TRANSPARENT`+`WS_EX_LAYERED` — **click-through**: o rato atravessa
//!   a janela e continua a interagir com o que está por baixo.
//! * `WS_EX_NOREDIRECTIONBITMAP` — sem superfície GDI; o conteúdo vem só da
//!   DirectComposition (obrigatório para transparência acelerada por GPU).
//! * `WS_EX_TOPMOST`   — opcional; desligado no modo "Behind Windows".

use std::cell::Cell;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    GetLastError, ERROR_CLASS_ALREADY_EXISTS, HWND, LPARAM, LRESULT, WPARAM,
};
use windows::Win32::Graphics::Gdi::{BeginPaint, EndPaint, HBRUSH, PAINTSTRUCT};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, FindWindowW, GetWindowLongPtrW,
    RegisterClassExW, SetWindowLongPtrW, SetWindowPos, ShowWindow, CS_HREDRAW, CS_VREDRAW,
    GWL_EXSTYLE, HCURSOR, HICON, HWND_BOTTOM, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOOWNERZORDER, SWP_NOSIZE, SWP_SHOWWINDOW, SW_HIDE, SW_SHOWNOACTIVATE, WM_DISPLAYCHANGE,
    WM_DPICHANGED, WM_ERASEBKGND, WM_NCHITTEST, WM_PAINT, WM_SETTINGCHANGE, WNDCLASSEXW,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_NOREDIRECTIONBITMAP, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_EX_TRANSPARENT, WS_POPUP,
};

use crate::renderer::Renderer;
use crate::sim::World;
use crate::winutil;

const CLASS_NAME: &str = "SnowHushOverlayWindow";

thread_local! {
    /// Sinalizado pelo WndProc; consumido pelo loop principal.
    static DISPLAY_DIRTY: Cell<bool> = const { Cell::new(false) };
    static SURFACES_DIRTY: Cell<bool> = const { Cell::new(false) };
}

pub fn take_display_dirty() -> bool {
    DISPLAY_DIRTY.with(|c| {
        let v = c.get();
        c.set(false);
        v
    })
}

pub fn take_surfaces_dirty() -> bool {
    SURFACES_DIRTY.with(|c| {
        let v = c.get();
        c.set(false);
        v
    })
}

pub struct Overlay {
    hwnd: HWND,
    renderer: Renderer,
    behind: bool,
    visible: bool,
}

impl Overlay {
    pub fn new(x: i32, y: i32, width: i32, height: i32, behind: bool) -> Result<Self, String> {
        register_class()?;
        let class = winutil::wide(CLASS_NAME);
        let title = winutil::wide("SnowHush");

        // O bit WS_EX_TOPMOST só entra na criação se não estivermos atrás.
        let mut ex = WS_EX_LAYERED
            | WS_EX_TRANSPARENT
            | WS_EX_NOACTIVATE
            | WS_EX_TOOLWINDOW
            | WS_EX_NOREDIRECTIONBITMAP;
        if !behind {
            ex |= WS_EX_TOPMOST;
        }

        let hwnd = unsafe {
            CreateWindowExW(
                ex,
                PCWSTR(class.as_ptr()),
                PCWSTR(title.as_ptr()),
                WS_POPUP,
                x,
                y,
                width.max(1),
                height.max(1),
                None,
                None,
                Some(winutil::hinstance()),
                None,
            )
        }
        .map_err(|e| format!("CreateWindowEx(overlay): {e}"))?;

        let renderer = Renderer::new(hwnd, width.max(1) as u32, height.max(1) as u32)?;

        if std::env::var_os("SNOWHUSH_LOG").is_some() {
            let mut r = windows::Win32::Foundation::RECT::default();
            let _ = unsafe { windows::Win32::UI::WindowsAndMessaging::GetWindowRect(hwnd, &mut r) };
            winutil::log(&format!(
                "overlay criado: pedido {width}x{height}@{x},{y} | real {}x{}@{};{} | dpi={}",
                r.right - r.left,
                r.bottom - r.top,
                r.left,
                r.top,
                unsafe { windows::Win32::UI::HiDpi::GetDpiForWindow(hwnd) }
            ));
        }
        let mut overlay = Self {
            hwnd,
            renderer,
            behind: false,
            visible: false,
        };
        overlay.behind = behind;
        if behind {
            overlay.place_behind();
        }
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
        // Informa o shell de que esta janela não é uma aplicação fullscreen.
        // Sem isto, um overlay topmost do tamanho do monitor pode fazer o
        // Windows retirar o WS_EX_TOPMOST à barra de tarefas, deixando as
        // janelas normais cobrirem-na.
        mark_not_fullscreen(hwnd);
        overlay.visible = true;
        Ok(overlay)
    }

    pub fn hwnd(&self) -> isize {
        self.hwnd.0 as isize
    }

    pub fn render(&mut self, world: &World) -> Result<(), String> {
        self.renderer.render(world)
    }

    pub fn show(&mut self) {
        if !self.visible {
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
            }
            self.visible = true;
            self.ensure_taskbar_on_top();
        }
    }

    /// Verifica periodicamente se o Windows voltou a retirar o `WS_EX_TOPMOST`
    /// à barra de tarefas (acontece após algumas mudanças de foco/janela). Se
    /// sim, volta a marcar o overlay como não-fullscreen e reafirma o topmost.
    pub fn ensure_taskbar_on_top(&self) {
        if self.behind || !self.visible {
            return;
        }
        let class = winutil::wide("Shell_TrayWnd");
        unsafe {
            let Ok(tray) = FindWindowW(PCWSTR(class.as_ptr()), PCWSTR::null()) else {
                return;
            };
            if tray.0.is_null() {
                return;
            }
            let ex = GetWindowLongPtrW(tray, GWL_EXSTYLE);
            if ex & (WS_EX_TOPMOST.0 as isize) != 0 {
                return;
            }
            mark_not_fullscreen(self.hwnd);
            // Reafirma o topmost, mas deixa a barra *abaixo* do overlay para
            // a neve continuar visível por cima dela.
            let ex = GetWindowLongPtrW(tray, GWL_EXSTYLE) | WS_EX_TOPMOST.0 as isize;
            let _ = SetWindowLongPtrW(tray, GWL_EXSTYLE, ex);
            let _ = SetWindowPos(
                tray,
                Some(self.hwnd),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
    }

    pub fn hide(&mut self) {
        if self.visible {
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            }
            self.visible = false;
        }
    }

    /// Alterna entre "por cima de tudo" e "atrás das janelas".
    pub fn set_behind(&mut self, behind: bool) {
        if behind == self.behind {
            return;
        }
        self.behind = behind;
        unsafe {
            let ex = GetWindowLongPtrW(self.hwnd, GWL_EXSTYLE) as u32;
            let new_ex = if behind {
                ex & !WS_EX_TOPMOST.0
            } else {
                ex | WS_EX_TOPMOST.0
            };
            SetWindowLongPtrW(self.hwnd, GWL_EXSTYLE, new_ex as isize);
        }
        if behind {
            self.place_behind();
        } else {
            unsafe {
                let _ = SetWindowPos(
                    self.hwnd,
                    Some(HWND_TOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
                );
            }
        }
    }

    /// Insere o overlay imediatamente acima do papel de parede e abaixo dos
    /// ícones: `SetWindowPos(hwnd, workerw, ...)` coloca-o *depois* do
    /// `WorkerW` na ordem Z, ou seja, por baixo deste mas por cima do `Progman`.
    pub fn place_behind(&self) {
        let insert = winutil::find_workerw().unwrap_or(HWND_BOTTOM);
        unsafe {
            let _ = SetWindowPos(
                self.hwnd,
                Some(insert),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER,
            );
        }
    }

    pub fn destroy(self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

/// Informa o shell (via `ITaskbarList2::MarkFullscreenWindow`) de que o
/// overlay não deve ser tratado como uma aplicação fullscreen. O Windows
/// retira o `WS_EX_TOPMOST` à barra de tarefas quando deteta uma janela
/// fullscreen; sem esta chamada, as janelas normais passam a poder cobri-la.
fn mark_not_fullscreen(hwnd: HWND) {
    use windows::core::GUID;
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
    use windows::Win32::UI::Shell::ITaskbarList2;

    // CLSID_TaskbarList.
    let clsid = GUID::from_u128(0x56fdf344_fd6d_11d0_958a_006097c9a090);
    unsafe {
        if let Ok(taskbar) = CoCreateInstance::<Option<&windows::core::IUnknown>, ITaskbarList2>(
            &clsid,
            None,
            CLSCTX_INPROC_SERVER,
        ) {
            let _ = taskbar.HrInit();
            let _ = taskbar.MarkFullscreenWindow(hwnd, false);
        }
    }
}

fn register_class() -> Result<(), String> {
    let name = winutil::wide(CLASS_NAME);
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(wnd_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: winutil::hinstance(),
        hIcon: HICON::default(),
        hCursor: HCURSOR::default(),
        hbrBackground: HBRUSH::default(),
        lpszMenuName: PCWSTR::null(),
        lpszClassName: PCWSTR(name.as_ptr()),
        hIconSm: HICON::default(),
    };
    let atom = unsafe { RegisterClassExW(&wc) };
    if atom == 0 {
        let err = unsafe { GetLastError() };
        if err != ERROR_CLASS_ALREADY_EXISTS {
            return Err(format!("RegisterClassEx(overlay) falhou: {err:?}"));
        }
    }
    Ok(())
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        // O rato atravessa a janela mesmo que o WS_EX_TRANSPARENT falhe em
        // algum cenário (ex.: composição desativada).
        WM_NCHITTEST => LRESULT(-1), // HTTRANSPARENT
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let _ = BeginPaint(hwnd, &mut ps);
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        // Mudança de monitores/resolução: reconstruir a swap-chain.
        WM_DISPLAYCHANGE => {
            DISPLAY_DIRTY.with(|c| c.set(true));
            SURFACES_DIRTY.with(|c| c.set(true));
            LRESULT(0)
        }
        // Ignorar o retângulo sugerido (a janela é gerida pelo código).
        WM_DPICHANGED => LRESULT(0),
        // Barra de tarefas movida/redimensionada, tema alterado, etc.
        WM_SETTINGCHANGE => {
            SURFACES_DIRTY.with(|c| c.set(true));
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
