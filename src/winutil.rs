//! Utilitários Win32 partilhados (strings, janelas, métricas, logging).

use std::io::Write;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, RECT};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClassNameW, GetSystemMetrics, GetWindowRect, IsWindowVisible, MessageBoxW,
    MB_ICONERROR, MB_OK, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN,
};

/// Converte uma `&str` numa string UTF-16 terminada em NUL.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn hinstance() -> HINSTANCE {
    unsafe {
        HINSTANCE(
            GetModuleHandleW(None)
                .map(|m| m.0)
                .unwrap_or(std::ptr::null_mut()),
        )
    }
}

pub fn class_name(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    let n = unsafe { GetClassNameW(hwnd, &mut buf) };
    if n <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buf[..n as usize])
}

pub fn window_rect(hwnd: HWND) -> Option<RECT> {
    let mut r = RECT::default();
    unsafe { GetWindowRect(hwnd, &mut r).ok()? };
    if r.right <= r.left || r.bottom <= r.top {
        return None;
    }
    Some(r)
}

/// Retângulo do ecrã virtual em píxeis físicos: `(x, y, largura, altura)`.
///
/// Como o processo é Per-Monitor-V2, estes valores são coordenadas reais, sem
/// virtualização de DPI. O overlay usa-os diretamente em `CreateWindowExW`.
pub fn virtual_screen() -> (i32, i32, i32, i32) {
    unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    }
}

unsafe extern "system" fn collect_proc(hwnd: HWND, lparam: LPARAM) -> windows::core::BOOL {
    let list = &mut *(lparam.0 as *mut Vec<HWND>);
    list.push(hwnd);
    windows::core::BOOL(1)
}

/// Todos os HWND de topo, por ordem de Z-order.
pub fn top_level_windows() -> Vec<HWND> {
    let mut list: Vec<HWND> = Vec::with_capacity(128);
    unsafe {
        let _ = EnumWindows(Some(collect_proc), LPARAM(&mut list as *mut _ as isize));
    }
    list
}

/// Encontra o `WorkerW` que cobre o ecrã.
///
/// Em Windows 10/11 a área de trabalho é composta por `Progman` (fundo) e por um
/// `WorkerW` irmão que aloja o `SHELLDLL_DefView` (ícones). Inserir o overlay
/// imediatamente abaixo desse `WorkerW` coloca-o acima do papel de parede mas
/// abaixo dos ícones e de todas as janelas normais — o modo "Behind Windows".
pub fn find_workerw() -> Option<HWND> {
    let (vx, vy, vw, vh) = virtual_screen();
    for hwnd in top_level_windows() {
        if class_name(hwnd) != "WorkerW" {
            continue;
        }
        if !unsafe { IsWindowVisible(hwnd) }.as_bool() {
            continue;
        }
        if let Some(r) = window_rect(hwnd) {
            let covers = r.left <= vx && r.top <= vy && r.right >= vx + vw && r.bottom >= vy + vh;
            if covers {
                return Some(hwnd);
            }
        }
    }
    None
}

pub fn error_box(title: &str, message: &str) {
    let text = wide(message);
    let caption = wide(title);
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(text.as_ptr()),
            PCWSTR(caption.as_ptr()),
            MB_OK | MB_ICONERROR,
        );
    }
}

/// Log opcional em `%TEMP%\snowhush.log` (ativado por `SNOWHUSH_LOG=1`).
pub fn log(msg: &str) {
    if std::env::var_os("SNOWHUSH_LOG").is_none() {
        return;
    }
    let path = std::env::temp_dir().join("snowhush.log");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{msg}");
    }
}

pub fn show_help() {
    error_box(
        "SnowHush",
        "SnowHush 1.0\n\n\
         Uso: SnowHush.exe [opções]\n\
         \n\
         --intensity <off|light|steady|heavy|variable>\n\
         --no-accumulate     desliga a acumulação\n\
         --no-windows        a neve não pousa sobre janelas\n\
         --behind            desenha atrás das janelas\n\
         --carve-radius <px> raio da escavação do cursor (20-80)\n\
         \n\
         O ícone na bandeja do sistema contém todas as opções.",
    );
}

/// Nome de registo usado para o arranque automático.
pub const RUN_VALUE: &str = "SnowHush";
pub const RUN_SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
