//! Orquestração da aplicação: estado, loop de mensagens, comandos do menu.
//!
//! O loop é um "game loop" clássico: drena a fila de mensagens Win32, avança a
//! simulação a 60 Hz e desenha com `Present(sync=1)`, que sincroniza com o
//! vblank. `MsgWaitForMultipleObjectsEx` bloqueia até haver mensagem ou até ao
//! próximo frame, o que mantém o processo praticamente inativo em termos de CPU
//! entre frames.

use std::cell::Cell;
use std::time::Instant;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::System::Threading::{
    CreateMutexW, CreateWaitableTimerExW, SetWaitableTimer, CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
    INFINITE, TIMER_ALL_ACCESS,
};
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, SetThreadDpiAwarenessContext,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, MsgWaitForMultipleObjectsEx, PeekMessageW, PostQuitMessage, TranslateMessage,
    MSG, MSG_WAIT_FOR_MULTIPLE_OBJECTS_EX_FLAGS, PM_REMOVE, QS_ALLINPUT, WM_CONTEXTMENU,
    WM_LBUTTONDBLCLK, WM_QUIT, WM_RBUTTONUP,
};

use crate::autostart;
use crate::overlay::{take_display_dirty, take_surfaces_dirty, Overlay};
use crate::settings::{Intensity, Settings};
use crate::sim::World;
use crate::surfaces::Tracker;
use crate::tray::{Command, TrayIcon};
use crate::winutil;

thread_local! {
    /// Ponteiro para a instância de `App` do thread principal.
    ///
    /// Os WndProcs são funções `extern "system"` sem contexto, por isso
    /// comunicam com o estado da aplicação através deste ponteiro. Só é
    /// acedido no thread que corre o loop de mensagens.
    static APP: Cell<*mut App> = const { Cell::new(std::ptr::null_mut()) };
}

/// Executa `f` com acesso ao estado da aplicação (usado pelos WndProcs).
pub fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|cell| {
        let ptr = cell.get();
        if ptr.is_null() {
            None
        } else {
            // Seguro: o ponteiro aponta para `App` vivo no thread principal e
            // os WndProcs correm nesse mesmo thread.
            Some(unsafe { f(&mut *ptr) })
        }
    })
}

pub fn run() -> Result<(), String> {
    let mut settings = Settings {
        autostart: autostart::is_enabled(),
        ..Settings::default()
    };
    parse_args(&mut settings);

    // DPI antes de criar qualquer janela: sem virtualização de coordenadas o
    // overlay cobre exatamente os píxeis físicos dos monitores.
    //
    // Nota: o contexto de DPI é fixado no momento em que cada thread é criada.
    // Alterar a consciência do processo não atualiza threads já existentes, por
    // isso define-se também o contexto do thread principal (o único que cria
    // janelas e consulta métricas).
    unsafe {
        let dpi = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        winutil::log(&format!("DPI awareness (PerMonitorV2): {dpi:?}"));
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }

    // Instância única: um segundo processo termina de imediato.
    let _mutex = unsafe { CreateMutexW(None, false, w!("Local\\SnowHush.SingleInstance")) };
    if _mutex.is_ok() && unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        winutil::log("já existe uma instância em execução");
        return Ok(());
    }

    let mut app = App::new(settings)?;
    let ptr: *mut App = &mut app;
    APP.with(|cell| cell.set(ptr));

    winutil::log("SnowHush iniciado");
    winutil::log(&format!(
        "config: intensidade={:?} acumular={} janelas={} atrás={}",
        app.settings.intensity,
        app.settings.accumulate,
        app.settings.snow_on_windows,
        app.settings.behind_windows
    ));

    let result = app.message_loop();
    app.shutdown();

    APP.with(|cell| cell.set(std::ptr::null_mut()));
    winutil::log("SnowHush terminado");
    result
}

pub struct App {
    settings: Settings,
    world: World,
    overlay: Option<Overlay>,
    tray: TrayIcon,
    tracker: Tracker,
    origin: (i32, i32),
    area_mpx: f32,
    display_dirty: bool,
    surfaces_dirty: bool,
    behind_timer: f32,
    render_broken: bool,
    running: bool,
    /// Diagnóstico: contagem de frames para o log de FPS.
    frame_count: u32,
    frame_time: f32,
    /// Timer de alta resolução usado para acordar no instante exato do próximo
    /// frame (a espera por `MsgWaitForMultipleObjects` com milissegundos tem
    /// resolução de ~15,6 ms, o que limitaria a animação a ~33 FPS).
    frame_timer: Option<HANDLE>,
}

impl App {
    fn new(settings: Settings) -> Result<Self, String> {
        let tray = TrayIcon::new()?;
        let mut app = Self {
            settings,
            world: World::new(1.0, 1.0),
            overlay: None,
            tray,
            tracker: Tracker::new(0, 0),
            origin: (0, 0),
            area_mpx: 1.0,
            display_dirty: false,
            surfaces_dirty: false,
            behind_timer: 0.0,
            render_broken: false,
            running: true,
            frame_count: 0,
            frame_time: 0.0,
            frame_timer: unsafe {
                CreateWaitableTimerExW(
                    None,
                    PCWSTR::null(),
                    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                    TIMER_ALL_ACCESS.0,
                )
                .ok()
            },
        };
        app.create_overlay()?;
        let own_overlay = app.overlay.as_ref().map(|o| o.hwnd()).unwrap_or(0);
        app.tracker = Tracker::new(own_overlay, app.tray.hwnd());
        Ok(app)
    }

    fn create_overlay(&mut self) -> Result<(), String> {
        let (x, y, w, h) = winutil::virtual_screen();
        let w = w.max(1);
        let h = h.max(1);
        winutil::log(&format!("ecrã virtual: {w}x{h}@{x},{y}"));

        self.origin = (x, y);
        self.area_mpx = (w as f32 * h as f32) / 1_000_000.0;
        self.world.resize(w as f32, h as f32);

        let overlay = Overlay::new(x, y, w, h, self.settings.behind_windows)?;
        self.overlay = Some(overlay);
        self.render_broken = false;
        self.display_dirty = false;
        self.surfaces_dirty = true;
        self.update_visibility();
        Ok(())
    }

    fn rebuild_overlay(&mut self) {
        winutil::log("a reconstruir overlay");
        if let Some(o) = self.overlay.take() {
            o.destroy();
        }
        if let Err(e) = self.create_overlay() {
            winutil::log(&format!("recriar overlay falhou: {e}"));
            self.render_broken = true;
            // Evita um ciclo de reconstrução: só uma nova mudança de ecrã
            // volta a tentar.
            self.display_dirty = false;
        } else {
            let own = self.overlay.as_ref().map(|o| o.hwnd()).unwrap_or(0);
            self.tracker.set_own(own, self.tray.hwnd());
        }
    }

    fn update_visibility(&mut self) {
        let Some(overlay) = self.overlay.as_mut() else {
            return;
        };
        if self.settings.intensity.is_off() {
            overlay.hide();
        } else {
            overlay.show();
        }
    }

    fn message_loop(&mut self) -> Result<(), String> {
        let mut msg = MSG::default();
        let mut last = Instant::now();

        while self.running {
            unsafe {
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    if msg.message == WM_QUIT {
                        self.running = false;
                        break;
                    }
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            if !self.running {
                break;
            }

            let active =
                self.overlay.is_some() && !self.settings.intensity.is_off() && !self.render_broken;
            let frame = if active { 1.0 / 60.0 } else { 0.10 };
            let elapsed = last.elapsed().as_secs_f32();

            if elapsed >= frame {
                last = Instant::now();
                self.tick(elapsed.min(0.05));
                if active {
                    self.draw();
                }
                continue;
            }

            let wait_ms = ((((frame - elapsed) * 1000.0).ceil() as u32).clamp(1, 100)) as u32;
            match self.frame_timer {
                Some(timer) => {
                    // Due time negativo = relativo, em unidades de 100 ns.
                    let due = -(((frame - elapsed) * 10_000_000.0) as i64).max(100);
                    if unsafe { SetWaitableTimer(timer, &due, 0, None, None, false) }.is_ok() {
                        unsafe {
                            MsgWaitForMultipleObjectsEx(
                                Some(&[timer]),
                                INFINITE,
                                QS_ALLINPUT,
                                MSG_WAIT_FOR_MULTIPLE_OBJECTS_EX_FLAGS(0),
                            );
                        }
                        continue;
                    }
                    unsafe {
                        MsgWaitForMultipleObjectsEx(
                            None,
                            wait_ms,
                            QS_ALLINPUT,
                            MSG_WAIT_FOR_MULTIPLE_OBJECTS_EX_FLAGS(0),
                        );
                    }
                }
                None => unsafe {
                    MsgWaitForMultipleObjectsEx(
                        None,
                        wait_ms,
                        QS_ALLINPUT,
                        MSG_WAIT_FOR_MULTIPLE_OBJECTS_EX_FLAGS(0),
                    );
                },
            }
        }
        Ok(())
    }

    fn tick(&mut self, dt: f32) {
        if take_display_dirty() {
            self.display_dirty = true;
        }
        if take_surfaces_dirty() {
            self.surfaces_dirty = true;
        }
        if self.display_dirty {
            self.rebuild_overlay();
        }
        if self.surfaces_dirty {
            self.surfaces_dirty = false;
            self.tracker.force_refresh();
        }

        self.tracker
            .set_include_windows(self.settings.snow_on_windows);
        self.tracker.update(dt, self.origin);
        self.world
            .sync_surfaces(&self.tracker.surfaces, self.settings.accumulate);

        if self.settings.intensity.is_off() {
            return;
        }

        let target = self
            .settings
            .target_particles(self.area_mpx, self.world.time);
        let rate = self.settings.spawn_rate(self.area_mpx, self.world.time);
        self.world.step(dt, target, rate);

        if std::env::var_os("SNOWHUSH_LOG").is_some() {
            self.frame_count += 1;
            self.frame_time += dt;
            if self.frame_time >= 5.0 {
                winutil::log(&format!(
                    "FPS médio: {:.1} | partículas: {} | montes: {}",
                    self.frame_count as f32 / self.frame_time,
                    self.world.particles.len(),
                    self.world.piles.len()
                ));
                self.frame_count = 0;
                self.frame_time = 0.0;
            }
        }

        // Reafirma o Z-order no modo "atrás das janelas" (outras apps podem
        // reorganizar a ordem Z ao criar janelas).
        if self.settings.behind_windows {
            self.behind_timer += dt;
            if self.behind_timer >= 2.0 {
                self.behind_timer = 0.0;
                if let Some(overlay) = &self.overlay {
                    overlay.place_behind();
                }
            }
        }
    }

    fn draw(&mut self) {
        if self.render_broken {
            return;
        }
        let Some(overlay) = self.overlay.as_mut() else {
            return;
        };
        if let Err(e) = overlay.render(&self.world) {
            winutil::log(&format!("render falhou: {e}"));
            self.render_broken = true;
            self.display_dirty = true;
        }
    }

    // --- eventos enviados pelos WndProcs --------------------------------

    pub fn on_tray_event(&mut self, event: u32) {
        if event == WM_RBUTTONUP || event == WM_CONTEXTMENU {
            if let Some(cmd) = self.tray.show_menu(&self.settings) {
                self.apply(cmd);
            }
        } else if event == WM_LBUTTONDBLCLK {
            // Duplo-clique: liga/desliga rapidamente.
            let next = if self.settings.intensity.is_off() {
                Intensity::Steady
            } else {
                Intensity::Off
            };
            self.apply(Command::Intensity(next));
        }
    }

    pub fn on_taskbar_created(&mut self) {
        winutil::log("Explorador reiniciou: a readicionar ícone");
        self.tray.readd();
        self.surfaces_dirty = true;
    }

    fn apply(&mut self, cmd: Command) {
        match cmd {
            Command::Intensity(intensity) => {
                self.settings.intensity = intensity;
                if intensity.is_off() {
                    self.world.reset();
                }
                self.update_visibility();
            }
            Command::ToggleAccumulate => {
                self.settings.accumulate = !self.settings.accumulate;
            }
            Command::MeltSnow => self.world.melt_all(),
            Command::ToggleSnowOnWindows => {
                self.settings.snow_on_windows = !self.settings.snow_on_windows;
            }
            Command::ToggleBehind => {
                self.settings.behind_windows = !self.settings.behind_windows;
                if let Some(overlay) = self.overlay.as_mut() {
                    overlay.set_behind(self.settings.behind_windows);
                }
            }
            Command::ToggleAutoStart => {
                let enable = !self.settings.autostart;
                match autostart::set_enabled(enable) {
                    Ok(()) => self.settings.autostart = enable,
                    Err(e) => winutil::error_box(
                        "SnowHush",
                        &format!("Não foi possível alterar o arranque automático:\n{e}"),
                    ),
                }
            }
            Command::Quit => {
                self.running = false;
                unsafe { PostQuitMessage(0) };
            }
        }
    }

    fn shutdown(&mut self) {
        if let Some(overlay) = self.overlay.take() {
            overlay.destroy();
        }
        self.tray.remove();
        if let Some(timer) = self.frame_timer.take() {
            unsafe {
                let _ = CloseHandle(timer);
            }
        }
    }
}

fn parse_args(settings: &mut Settings) {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        match arg {
            "--intensity" | "-i" => {
                if let Some(value) = args.get(i + 1) {
                    if let Some(intensity) = Intensity::parse(value) {
                        settings.intensity = intensity;
                    }
                    i += 1;
                }
            }
            "--no-accumulate" => settings.accumulate = false,
            "--no-windows" => settings.snow_on_windows = false,
            "--behind" => settings.behind_windows = true,
            _ => {
                if let Some(value) = arg.strip_prefix("--intensity=") {
                    if let Some(intensity) = Intensity::parse(value) {
                        settings.intensity = intensity;
                    }
                }
            }
        }
        i += 1;
    }
}
