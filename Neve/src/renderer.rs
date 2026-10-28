//! Renderer GPU: D3D11 -> Direct2D -> DirectComposition.
//!
//! Porque não WPF/WinForms? Uma janela `WS_EX_LAYERED` "clássica" obriga a
//! composição por CPU (`UpdateLayeredWindow`) e limita a ~5-10 ms de CPU por
//! frame em ecrãs grandes. Aqui a transparência vem da **DirectComposition**:
//!
//! 1. Cria-se um dispositivo D3D11 (hardware, com fallback WARP).
//! 2. A swap-chain é criada com `CreateSwapChainForComposition` e
//!    `DXGI_ALPHA_MODE_PREMULTIPLIED`, que suporta alfa por píxel real.
//! 3. Um `IDCompositionVisual` recebe a swap-chain e é ligado ao HWND com
//!    `IDCompositionDevice::CreateTargetForHwnd`.
//! 4. O desenho é feito em Direct2D sobre o back-buffer, com antialiasing e
//!    geometria 100% na GPU.
//!
//! A janela correspondente tem `WS_EX_NOREDIRECTIONBITMAP`: não existe
//! superfície GDI, pelo que o único conteúdo visível é o que o DComp compõe.

use core::mem::ManuallyDrop;

use windows::core::Interface;
use windows::Win32::Foundation::{HMODULE, HWND};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT, D2D_RECT_F, D2D_SIZE_U,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1Bitmap, ID2D1Bitmap1, ID2D1Device, ID2D1DeviceContext, ID2D1Factory1,
    ID2D1SolidColorBrush, D2D1_BITMAP_OPTIONS_CANNOT_DRAW, D2D1_BITMAP_OPTIONS_NONE,
    D2D1_BITMAP_OPTIONS_TARGET, D2D1_BITMAP_PROPERTIES1, D2D1_DEVICE_CONTEXT_OPTIONS_NONE,
    D2D1_ELLIPSE, D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_INTERPOLATION_MODE_LINEAR,
    D2D1_ROUNDED_RECT, D2D1_UNIT_MODE_PIXELS,
};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_10_0,
    D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_11_0,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::DirectComposition::{
    DCompositionCreateDevice, IDCompositionDevice, IDCompositionTarget, IDCompositionVisual,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_PREMULTIPLIED, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIDevice, IDXGIFactory2, IDXGIOutput, IDXGISurface, IDXGISwapChain1, DXGI_PRESENT,
    DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
    DXGI_USAGE_RENDER_TARGET_OUTPUT,
};
use windows_numerics::Vector2;

use crate::geometry::Rect;
use crate::sim::{Particle, Pile, World};

/// Número de níveis de opacidade pré-calculados (pincéis ficam em cache).
const ALPHA_LEVELS: usize = 32;

pub struct Renderer {
    _device: ID3D11Device,
    _dxgi_device: IDXGIDevice,
    swapchain: IDXGISwapChain1,
    d2d_context: ID2D1DeviceContext,
    _target: ID2D1Bitmap1,
    _dcomp_device: IDCompositionDevice,
    _dcomp_target: IDCompositionTarget,
    _dcomp_visual: IDCompositionVisual,
    _d2d_device: ID2D1Device,
    brushes: Vec<ID2D1SolidColorBrush>,
    /// Retângulos (ordem Z, retângulo) das janelas com neve, para recortar a
    /// neve das janelas tapadas. Reutilizado entre frames (sem alocações).
    occluders: Vec<(i32, Rect)>,
    /// Textura de floco (ponto branco suave), desenhada com `DrawBitmap`.
    flake_bitmap: Option<ID2D1Bitmap>,
}

impl Renderer {
    pub fn new(hwnd: HWND, width: u32, height: u32) -> Result<Self, String> {
        let width = width.max(1);
        let height = height.max(1);

        let features: [D3D_FEATURE_LEVEL; 3] = [
            D3D_FEATURE_LEVEL_11_0,
            D3D_FEATURE_LEVEL_10_1,
            D3D_FEATURE_LEVEL_10_0,
        ];

        let (device, driver_label) = match create_d3d_device(D3D_DRIVER_TYPE_HARDWARE, &features) {
            Ok(device) => (device, "hardware"),
            // Fallback: rasterizador por software (RDP, VMs sem GPU, etc.).
            Err(_) => (
                create_d3d_device(D3D_DRIVER_TYPE_WARP, &features)
                    .map_err(|e| format!("D3D11CreateDevice: {e}"))?,
                "WARP (software)",
            ),
        };

        let dxgi_device: IDXGIDevice = device.cast().map_err(|e| format!("IDXGIDevice: {e}"))?;
        let adapter =
            unsafe { dxgi_device.GetAdapter() }.map_err(|e| format!("GetAdapter: {e}"))?;
        if std::env::var_os("SNOWHUSH_LOG").is_some() {
            if let Ok(desc) = unsafe { adapter.GetDesc() } {
                let name = String::from_utf16_lossy(&desc.Description);
                crate::winutil::log(&format!(
                    "GPU: {} | driver: {} | VRAM dedicada: {} MB",
                    name.trim_end_matches('\0'),
                    driver_label,
                    desc.DedicatedVideoMemory / (1024 * 1024)
                ));
            }
        }
        let factory: IDXGIFactory2 =
            unsafe { adapter.GetParent() }.map_err(|e| format!("IDXGIFactory2: {e}"))?;

        // Swap-chain com alfa pré-multiplicado, flip model (obrigatório para
        // composição) e 2 buffers (menos memória, latência baixa).
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: width,
            Height: height,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            Stereo: false.into(),
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            Scaling: DXGI_SCALING_STRETCH,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
            AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
            Flags: 0,
        };
        let swapchain = unsafe {
            factory
                .CreateSwapChainForComposition(&dxgi_device, &desc, None::<&IDXGIOutput>)
                .map_err(|e| format!("CreateSwapChainForComposition: {e}"))?
        };

        // --- Direct2D ------------------------------------------------------
        let d2d_factory: ID2D1Factory1 =
            unsafe { D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None) }
                .map_err(|e| format!("D2D1CreateFactory: {e}"))?;
        let d2d_device = unsafe { d2d_factory.CreateDevice(&dxgi_device) }
            .map_err(|e| format!("CreateDevice(D2D): {e}"))?;
        let d2d_context =
            unsafe { d2d_device.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE) }
                .map_err(|e| format!("CreateDeviceContext: {e}"))?;

        let target = bind_target(&d2d_context, &swapchain)?;
        unsafe {
            d2d_context.SetUnitMode(D2D1_UNIT_MODE_PIXELS);
        }

        // --- pincéis brancos por nível de alfa -----------------------------
        let mut brushes = Vec::with_capacity(ALPHA_LEVELS + 1);
        for i in 0..=ALPHA_LEVELS {
            let a = i as f32 / ALPHA_LEVELS as f32;
            let color = D2D1_COLOR_F {
                r: 1.0,
                g: 1.0,
                b: 1.0,
                a,
            };
            let brush = unsafe { d2d_context.CreateSolidColorBrush(&color, None) }
                .map_err(|e| format!("CreateSolidColorBrush: {e}"))?;
            brushes.push(brush);
        }

        // --- DirectComposition ---------------------------------------------
        let dcomp_device: IDCompositionDevice =
            unsafe { DCompositionCreateDevice(&dxgi_device) }
                .map_err(|e| format!("DCompositionCreateDevice: {e}"))?;
        let dcomp_target = unsafe { dcomp_device.CreateTargetForHwnd(hwnd, true) }
            .map_err(|e| format!("CreateTargetForHwnd: {e}"))?;
        let dcomp_visual =
            unsafe { dcomp_device.CreateVisual() }.map_err(|e| format!("CreateVisual: {e}"))?;
        unsafe {
            dcomp_visual
                .SetContent(&swapchain)
                .map_err(|e| format!("SetContent: {e}"))?;
            dcomp_target
                .SetRoot(&dcomp_visual)
                .map_err(|e| format!("SetRoot: {e}"))?;
            dcomp_device.Commit().map_err(|e| format!("Commit: {e}"))?;
        }

        // --- textura dos flocos -------------------------------------------
        let flake_bitmap = match init_flake_bitmap(&d2d_context) {
            Ok(bitmap) => Some(bitmap),
            Err(e) => {
                crate::winutil::log(&format!(
                    "textura de floco indisponível ({e}); a usar elipses"
                ));
                None
            }
        };

        Ok(Self {
            _device: device,
            _dxgi_device: dxgi_device,
            swapchain,
            d2d_context,
            _target: target,
            _dcomp_device: dcomp_device,
            _dcomp_target: dcomp_target,
            _dcomp_visual: dcomp_visual,
            _d2d_device: d2d_device,
            brushes,
            occluders: Vec::with_capacity(32),
            flake_bitmap,
        })
    }

    #[inline]
    fn brush(&self, alpha: f32) -> &ID2D1SolidColorBrush {
        let idx =
            ((alpha.clamp(0.0, 1.0) * ALPHA_LEVELS as f32).round() as usize).min(ALPHA_LEVELS);
        &self.brushes[idx]
    }

    /// Desenha um frame completo e apresenta-o.
    pub fn render(&mut self, world: &World) -> Result<(), String> {
        unsafe {
            self.d2d_context.BeginDraw();
            let transparent = D2D1_COLOR_F {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.0,
            };
            self.d2d_context.Clear(Some(&transparent));

            // A neve acumulada fica por baixo dos flocos em queda.
            self.occluders.clear();
            for pile in &world.piles {
                if !pile.sliding() {
                    self.occluders.push((pile.z, pile.rect));
                }
            }
            for pile in &world.piles {
                self.draw_pile(pile);
            }
            self.draw_flakes(&world.particles);

            self.d2d_context
                .EndDraw(None, None)
                .map_err(|e| format!("EndDraw: {e}"))?;
        }

        // Present com sincronização vertical: 1 = aguarda o vblank.
        unsafe {
            self.swapchain
                .Present(1, DXGI_PRESENT(0))
                .ok()
                .map_err(|e| format!("Present: {e}"))?;
        }
        Ok(())
    }

    /// Monte de neve: um retângulo arredondado por célula.
    ///
    /// As células sobrepõem-se 1 px na horizontal, pelo que o resultado é uma
    /// camada contínua com o topo arredondado (as "ondulações" vêm das alturas
    /// irregulares produzidas pela deposição). É uma única chamada de desenho
    /// por célula — a GPU resolve milhões de píxeis sem custo de CPU relevante.
    fn draw_pile(&self, pile: &Pile) {
        let n = pile.cells.len();
        if n == 0 {
            return;
        }
        let alpha = pile.alpha();
        if alpha <= 0.02 {
            return;
        }
        let brush = self.brush(alpha);
        let x0 = pile.rect.x;
        let base_y = pile.rect.y + pile.slide_y;
        let cw = pile.cell_w;

        for (i, &h) in pile.cells.iter().enumerate() {
            if h < 0.2 {
                continue;
            }
            let left = x0 + i as f32 * cw - 0.75;
            let right = left + cw + 1.5;
            let top = base_y - h;

            // Recorta as células tapadas por janelas mais acima.
            let cell = Rect::new(left, top, right - left, base_y - top + 0.6);
            if self
                .occluders
                .iter()
                .any(|(z, r)| *z < pile.z && intersects(*r, cell))
            {
                continue;
            }

            // Raio pequeno: as células fundem-se numa camada contínua; um raio
            // próximo de metade da largura criaria cápsulas separadas.
            let radius = (h * 0.35).clamp(0.4, 2.0);
            let shape = D2D1_ROUNDED_RECT {
                rect: D2D_RECT_F {
                    left,
                    top,
                    right,
                    bottom: base_y + 0.6,
                },
                radiusX: radius,
                radiusY: radius,
            };
            unsafe {
                self.d2d_context.FillRoundedRectangle(&shape, brush);
            }
        }
    }

    /// Desenha os flocos com uma textura de ponto suave (32×32) escalada pela
    /// GPU.
    ///
    /// Um `FillEllipse` com antialiasing custa ~6 µs de CPU por floco (o
    /// Direct2D tesselar a elipse em software), o que a 700 partículas dava
    /// ~4 ms/frame. Um `DrawBitmap` filtrado é substancialmente mais barato e o
    /// resultado é mais suave (halo desfocado em vez de disco duro).
    fn draw_flakes(&mut self, particles: &[Particle]) {
        let Some(bitmap) = self.flake_bitmap.clone() else {
            for p in particles {
                self.draw_flake(p.x, p.y, p.r, p.alpha);
            }
            return;
        };
        unsafe {
            for p in particles {
                if p.alpha <= 0.02 {
                    continue;
                }
                // A textura tem uma queda radial; 2,1× o raio do núcleo deixa
                // um halo ténue à volta do floco.
                let s = p.r * 2.1;
                let dest = D2D_RECT_F {
                    left: p.x - s,
                    top: p.y - s,
                    right: p.x + s,
                    bottom: p.y + s,
                };
                self.d2d_context.DrawBitmap(
                    &bitmap,
                    Some(&dest),
                    (p.alpha * 0.95).clamp(0.0, 1.0),
                    D2D1_INTERPOLATION_MODE_LINEAR,
                    None,
                    None,
                );
            }
        }
    }
    fn draw_flake(&self, x: f32, y: f32, r: f32, alpha: f32) {
        if alpha <= 0.02 {
            return;
        }
        // Halo suave para flocos maiores (profundidade/desfoque barato).
        if r > 1.7 {
            let halo = D2D1_ELLIPSE {
                point: Vector2 { X: x, Y: y },
                radiusX: r * 1.95,
                radiusY: r * 1.95,
            };
            unsafe {
                self.d2d_context
                    .FillEllipse(&halo, self.brush(alpha * 0.18));
            }
        }
        let core = D2D1_ELLIPSE {
            point: Vector2 { X: x, Y: y },
            radiusX: r,
            radiusY: r,
        };
        unsafe {
            self.d2d_context.FillEllipse(&core, self.brush(alpha));
        }
    }
}

/// Interseção de dois retângulos.
fn intersects(a: Rect, b: Rect) -> bool {
    a.x < b.right() && b.x < a.right() && a.y < b.bottom() && b.y < a.bottom()
}

fn create_d3d_device(
    driver: windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE,
    features: &[D3D_FEATURE_LEVEL],
) -> windows::core::Result<ID3D11Device> {
    let mut device: Option<ID3D11Device> = None;
    let mut level = D3D_FEATURE_LEVEL_11_0;
    let mut context = None;
    unsafe {
        D3D11CreateDevice(
            None::<&windows::Win32::Graphics::Dxgi::IDXGIAdapter>,
            driver,
            // Para HARDWARE/WARP o módulo de software tem de ser NULL.
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(features),
            D3D11_SDK_VERSION,
            Some(&mut device),
            Some(&mut level),
            Some(&mut context),
        )?;
    }
    device.ok_or_else(|| windows::core::Error::from_thread())
}
/// Cria a textura de floco.
///
/// A textura é gerada em código: um ponto branco pré-multiplicado com queda
/// radial suave, o que dá ao floco um contorno desfocado em vez de um disco
/// duro — e permite desenhar tamanhos arbitrários por escalonamento.
fn init_flake_bitmap(ctx: &ID2D1DeviceContext) -> Result<ID2D1Bitmap, String> {
    const S: u32 = 32;

    let mut pixels = vec![0u32; (S * S) as usize];
    let c = (S as f32 - 1.0) * 0.5;
    for y in 0..S {
        for x in 0..S {
            let dx = x as f32 - c;
            let dy = y as f32 - c;
            let d = ((dx * dx + dy * dy).sqrt() / (c + 0.5)).min(1.0);
            let a = (1.0 - d).powf(1.7);
            let a = (a * 255.0).round() as u32;
            // BGRA pré-multiplicado (branco).
            pixels[(y * S + x) as usize] = (a << 24) | (a << 16) | (a << 8) | a;
        }
    }

    let props = D2D1_BITMAP_PROPERTIES1 {
        pixelFormat: D2D1_PIXEL_FORMAT {
            format: DXGI_FORMAT_B8G8R8A8_UNORM,
            alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
        },
        dpiX: 96.0,
        dpiY: 96.0,
        bitmapOptions: D2D1_BITMAP_OPTIONS_NONE,
        colorContext: ManuallyDrop::new(None),
    };
    let bitmap = unsafe {
        ctx.CreateBitmap(
            D2D_SIZE_U {
                width: S,
                height: S,
            },
            Some(pixels.as_ptr() as *const core::ffi::c_void),
            S * 4,
            &props,
        )
    }
    .map_err(|e| format!("CreateBitmap(floco): {e}"))?;
    let bitmap: ID2D1Bitmap = bitmap.cast().map_err(|e| format!("ID2D1Bitmap: {e}"))?;
    Ok(bitmap)
}

/// Cria o `ID2D1Bitmap1` sobre o back-buffer atual da swap-chain.
fn bind_target(
    context: &ID2D1DeviceContext,
    swapchain: &IDXGISwapChain1,
) -> Result<ID2D1Bitmap1, String> {
    unsafe {
        let backbuffer: IDXGISurface = swapchain
            .GetBuffer(0)
            .map_err(|e| format!("GetBuffer: {e}"))?;
        let props = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: 96.0,
            dpiY: 96.0,
            bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
            colorContext: ManuallyDrop::new(None),
        };
        let bitmap = context
            .CreateBitmapFromDxgiSurface(&backbuffer, Some(&props))
            .map_err(|e| format!("CreateBitmapFromDxgiSurface: {e}"))?;
        context.SetTarget(&bitmap);
        Ok(bitmap)
    }
}
