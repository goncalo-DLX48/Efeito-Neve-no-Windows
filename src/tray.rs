//! Ícone na bandeja do sistema (Notification Area) e menu de contexto.
//!
//! Usa `Shell_NotifyIconW` diretamente (sem WinForms/WPF): o ícone é desenhado
//! em código (um floco de neve) num DIB 32bpp e convertido em `HICON` com
//! `CreateIconIndirect`, evitando ficheiros de recursos.
//!
//! O menu é reconstruído a cada abertura (estado dos "checks" sempre correto) e
//! é apresentado com `TrackPopupMenuEx(TPM_RETURNCMD)`, que devolve o comando
//! escolhido de forma síncrona.

use std::ffi::c_void;
use std::mem::size_of;
use std::sync::atomic::{AtomicU32, Ordering};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    GetLastError, ERROR_CLASS_ALREADY_EXISTS, HWND, LPARAM, LRESULT, POINT, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    CreateBitmap, CreateDIBSection, DeleteObject, GetDC, ReleaseDC, BITMAPINFO, BITMAPINFOHEADER,
    BI_RGB, DIB_RGB_COLORS,
};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconIndirect, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon,
    DestroyMenu, DestroyWindow, GetCursorPos, PostMessageW, RegisterClassExW,
    RegisterWindowMessageW, SetForegroundWindow, TrackPopupMenuEx, HCURSOR, HICON, HMENU, ICONINFO,
    MENU_ITEM_FLAGS, MF_CHECKED, MF_POPUP, MF_SEPARATOR, MF_STRING, TPM_NONOTIFY, TPM_RETURNCMD,
    TPM_RIGHTBUTTON, WINDOW_EX_STYLE, WM_APP, WM_DESTROY, WM_NULL, WNDCLASSEXW, WS_POPUP,
};

use crate::settings::{Intensity, Settings};

const CLASS_NAME: &str = "SnowHushTrayWindow";
pub const WM_TRAY: u32 = WM_APP + 1;
const ICON_ID: u32 = 1;

/// `TaskbarCreated`: enviado quando o Explorador reinicia (o ícone desaparece).
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Intensity(Intensity),
    ToggleAccumulate,
    MeltSnow,
    ToggleSnowOnWindows,
    ToggleBehind,
    ToggleAutoStart,
    Quit,
}

impl Command {
    fn from_id(id: u32) -> Option<Self> {
        match id {
            101 => Some(Command::Intensity(Intensity::Off)),
            102 => Some(Command::Intensity(Intensity::Light)),
            103 => Some(Command::Intensity(Intensity::Steady)),
            104 => Some(Command::Intensity(Intensity::Heavy)),
            105 => Some(Command::Intensity(Intensity::Variable)),
            201 => Some(Command::ToggleAccumulate),
            202 => Some(Command::MeltSnow),
            203 => Some(Command::ToggleSnowOnWindows),
            204 => Some(Command::ToggleBehind),
            205 => Some(Command::ToggleAutoStart),
            301 => Some(Command::Quit),
            _ => None,
        }
    }
}

pub struct TrayIcon {
    hwnd: HWND,
    icon: HICON,
    added: bool,
}

impl TrayIcon {
    pub fn new() -> Result<Self, String> {
        register_class()?;
        let class = crate::winutil::wide(CLASS_NAME);
        let title = crate::winutil::wide("SnowHush");

        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                PCWSTR(class.as_ptr()),
                PCWSTR(title.as_ptr()),
                WS_POPUP,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(crate::winutil::hinstance()),
                None,
            )
        }
        .map_err(|e| format!("CreateWindowEx(tray): {e}"))?;

        let taskbar_msg = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };
        TASKBAR_CREATED.store(taskbar_msg, Ordering::Relaxed);

        let icon = make_icon()?;
        let mut tray = Self {
            hwnd,
            icon,
            added: false,
        };
        tray.add();
        Ok(tray)
    }

    pub fn hwnd(&self) -> isize {
        self.hwnd.0 as isize
    }

    fn add(&mut self) {
        let mut nid = NOTIFYICONDATAW::default();
        nid.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = self.hwnd;
        nid.uID = ICON_ID;
        nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        nid.uCallbackMessage = WM_TRAY;
        nid.hIcon = self.icon;
        let tip: Vec<u16> = "SnowHush - Interactive Snow"
            .encode_utf16()
            .take(127)
            .collect();
        nid.szTip[..tip.len()].copy_from_slice(&tip);
        self.added = unsafe { Shell_NotifyIconW(NIM_ADD, &nid) }.as_bool();
        crate::winutil::log(if self.added {
            "ícone da bandeja: registado"
        } else {
            "ícone da bandeja: FALHOU"
        });
    }

    /// Re-regista o ícone depois de o Explorador reiniciar.
    pub fn readd(&mut self) {
        self.added = false;
        self.add();
    }

    pub fn remove(&mut self) {
        if self.added {
            let mut nid = NOTIFYICONDATAW::default();
            nid.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
            nid.hWnd = self.hwnd;
            nid.uID = ICON_ID;
            unsafe {
                let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
            }
            self.added = false;
        }
        unsafe {
            let _ = DestroyWindow(self.hwnd);
            let _ = DestroyIcon(self.icon);
        }
    }

    /// Constrói e apresenta o menu; devolve o comando escolhido.
    pub fn show_menu(&self, settings: &Settings) -> Option<Command> {
        unsafe {
            let _ = SetForegroundWindow(self.hwnd);

            let menu = CreatePopupMenu().ok()?;
            build_menu(menu, settings);

            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);

            // TPM_RETURNCMD: o valor devolvido é o ID do comando (0 = cancelado).
            let flags = TPM_RETURNCMD.0 | TPM_RIGHTBUTTON.0 | TPM_NONOTIFY.0;
            let id = TrackPopupMenuEx(menu, flags, pt.x, pt.y, self.hwnd, None).0 as u32;
            let _ = DestroyMenu(menu);
            // Documentado pela Microsoft: sem este PostMessage o menu pode não
            // fechar corretamente quando se clica fora dele.
            let _ = PostMessageW(Some(self.hwnd), WM_NULL, WPARAM(0), LPARAM(0));

            Command::from_id(id)
        }
    }
}

fn append(menu: HMENU, flags: MENU_ITEM_FLAGS, id: usize, text: Option<&str>) {
    let buf = text.map(crate::winutil::wide);
    let ptr = match &buf {
        Some(b) => PCWSTR(b.as_ptr()),
        None => PCWSTR::null(),
    };
    unsafe {
        let _ = AppendMenuW(menu, flags, id, ptr);
    }
}

fn build_menu(menu: HMENU, settings: &Settings) {
    let check = |on: bool| {
        if on {
            MENU_ITEM_FLAGS(MF_STRING.0 | MF_CHECKED.0)
        } else {
            MF_STRING
        }
    };

    // Submenu Snowfall ---------------------------------------------------
    if let Ok(sub) = unsafe { CreatePopupMenu() } {
        for (i, intensity) in Intensity::ALL.iter().enumerate() {
            append(
                sub,
                check(*intensity == settings.intensity),
                100 + i,
                Some(intensity.label()),
            );
        }
        append(
            menu,
            MENU_ITEM_FLAGS(MF_POPUP.0 | MF_STRING.0),
            sub.0 as usize,
            Some("Snowfall"),
        );
    }

    append(menu, MF_SEPARATOR, 0, None);

    append(menu, check(settings.accumulate), 201, Some("Accumulate"));
    append(menu, MF_STRING, 202, Some("Melt Snow"));
    append(
        menu,
        check(settings.snow_on_windows),
        203,
        Some("Snow on Windows"),
    );
    append(
        menu,
        check(settings.behind_windows),
        204,
        Some("Behind Windows"),
    );
    append(menu, check(settings.autostart), 205, Some("Open at Login"));

    append(menu, MF_SEPARATOR, 0, None);
    append(menu, MF_STRING, 301, Some("Quit"));
}

fn register_class() -> Result<(), String> {
    let name = crate::winutil::wide(CLASS_NAME);
    let wc = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(wnd_proc),
        hInstance: crate::winutil::hinstance(),
        hCursor: HCURSOR::default(),
        lpszMenuName: PCWSTR::null(),
        lpszClassName: PCWSTR(name.as_ptr()),
        ..Default::default()
    };
    let atom = unsafe { RegisterClassExW(&wc) };
    if atom == 0 {
        let err = unsafe { GetLastError() };
        if err != ERROR_CLASS_ALREADY_EXISTS {
            return Err(format!("RegisterClassEx(tray) falhou: {err:?}"));
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
    if msg == TASKBAR_CREATED.load(Ordering::Relaxed) && msg != 0 {
        crate::app::with_app(|app| app.on_taskbar_created());
        return LRESULT(0);
    }
    match msg {
        WM_TRAY => {
            // LOWORD(lParam) = mensagem do rato (WM_RBUTTONUP, WM_LBUTTONDBLCLK...).
            let event = (lparam.0 as u32) & 0xFFFF;
            crate::app::with_app(|app| app.on_tray_event(event));
            LRESULT(0)
        }
        WM_DESTROY => LRESULT(0),
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

// ---------------------------------------------------------------------------
// Ícone: floco de neve desenhado em código.
// ---------------------------------------------------------------------------

fn make_icon() -> Result<HICON, String> {
    const S: i32 = 32;
    unsafe {
        let mut bmi = BITMAPINFO::default();
        bmi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
        bmi.bmiHeader.biWidth = S;
        bmi.bmiHeader.biHeight = -S; // top-down
        bmi.bmiHeader.biPlanes = 1;
        bmi.bmiHeader.biBitCount = 32;
        bmi.bmiHeader.biCompression = BI_RGB.0;

        let mut bits: *mut c_void = std::ptr::null_mut();
        let hdc = GetDC(None);
        let color = CreateDIBSection(Some(hdc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0);
        let _ = ReleaseDC(None, hdc);
        let color = color.map_err(|e| format!("CreateDIBSection: {e}"))?;

        let pixels = std::slice::from_raw_parts_mut(bits as *mut u32, (S * S) as usize);
        paint_flake(pixels, S as usize);

        // Máscara toda a zero (opaca): a transparência vem do canal alfa do
        // DIB. Os bits têm de ser explícitos — `CreateBitmap` sem `lpBits`
        // deixa a máscara indefinida e pode recortar o ícone.
        let mask_stride = ((S as usize + 31) / 32) * 4;
        let mask_bits = vec![0u8; mask_stride * S as usize];
        let mask = CreateBitmap(S, S, 1, 1, Some(mask_bits.as_ptr() as *const c_void));
        let info = ICONINFO {
            fIcon: true.into(),
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask,
            hbmColor: color,
        };
        let icon = CreateIconIndirect(&info).map_err(|e| format!("CreateIconIndirect: {e}"))?;
        let _ = DeleteObject(color.into());
        let _ = DeleteObject(mask.into());
        Ok(icon)
    }
}

/// Rasteriza um floco de 6 braços com cobertura sub-píxel (antialiasing).
fn paint_flake(pixels: &mut [u32], size: usize) {
    let mut cov = vec![0f32; size * size];
    let c = (size as f32 - 1.0) * 0.5;

    for arm in 0..6 {
        let a = arm as f32 * std::f32::consts::PI / 3.0;
        let (dx, dy) = (a.cos(), a.sin());
        // Braço principal.
        let mut t = 0.0;
        while t <= 1.0 {
            let r = 2.0 + t * (c - 2.5);
            splat(&mut cov, size, c + dx * r, c + dy * r, 1.0);
            t += 0.035;
        }
        // Duas ramificações curtas por braço.
        for base in [0.55f32, 0.78] {
            let br = 2.0 + base * (c - 2.5);
            let (bx, by) = (c + dx * br, c + dy * br);
            for sign in [-0.62f32, 0.62] {
                let (ex, ey) = ((a + sign).cos(), (a + sign).sin());
                let mut s = 0.0;
                while s <= 3.2 {
                    splat(&mut cov, size, bx + ex * s, by + ey * s, 0.9);
                    s += 0.5;
                }
            }
        }
    }

    for (i, c) in cov.iter().enumerate() {
        let alpha = (c * 255.0).clamp(0.0, 255.0) as u32;
        // 0xAARRGGBB num DIB BGRA little-endian; branco puro.
        pixels[i] = (alpha << 24) | 0x00FF_FFFF;
    }
}

/// Deposita cobertura com interpolação bilinear (antialiasing barato).
fn splat(cov: &mut [f32], size: usize, x: f32, y: f32, weight: f32) {
    let xi = x.floor() as isize;
    let yi = y.floor() as isize;
    let fx = x - xi as f32;
    let fy = y - yi as f32;
    for (ox, oy, w) in [
        (0isize, 0isize, (1.0 - fx) * (1.0 - fy)),
        (1, 0, fx * (1.0 - fy)),
        (0, 1, (1.0 - fx) * fy),
        (1, 1, fx * fy),
    ] {
        let px = xi + ox;
        let py = yi + oy;
        if px >= 0 && py >= 0 && (px as usize) < size && (py as usize) < size {
            let idx = py as usize * size + px as usize;
            cov[idx] = (cov[idx] + w * weight).min(1.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Graphics::Gdi::{GetDC, GetDIBits, ReleaseDC, DIB_RGB_COLORS};
    use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo};

    #[test]
    fn generated_icon_has_visible_pixels() {
        unsafe {
            let icon = make_icon().expect("criar ícone");
            let mut info = ICONINFO::default();
            GetIconInfo(icon, &mut info).expect("GetIconInfo");

            let mut bmi = BITMAPINFO::default();
            bmi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
            bmi.bmiHeader.biWidth = 32;
            bmi.bmiHeader.biHeight = -32;
            bmi.bmiHeader.biPlanes = 1;
            bmi.bmiHeader.biBitCount = 32;
            bmi.bmiHeader.biCompression = BI_RGB.0;

            let mut buf = vec![0u8; 32 * 32 * 4];
            let hdc = GetDC(None);
            let lines = GetDIBits(
                hdc,
                info.hbmColor,
                0,
                32,
                Some(buf.as_mut_ptr() as *mut _),
                &mut bmi,
                DIB_RGB_COLORS,
            );
            let _ = ReleaseDC(None, hdc);
            assert_eq!(lines, 32, "GetDIBits não leu o bitmap do ícone");

            // Píxeis com alfa útil: o floco tem de ser visível na bandeja.
            let opaque = buf.chunks(4).filter(|p| p[3] > 32).count();
            assert!(opaque > 40, "ícone vazio ({opaque} píxeis opacos)");

            let _ = DeleteObject(info.hbmColor.into());
            let _ = DeleteObject(info.hbmMask.into());
            let _ = DestroyIcon(icon);
        }
    }
}
