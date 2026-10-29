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
    D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_BEZIER_SEGMENT, D2D1_COLOR_F, D2D1_FIGURE_BEGIN_FILLED,
    D2D1_FIGURE_END_CLOSED, D2D1_GRADIENT_STOP, D2D1_PIXEL_FORMAT, D2D_RECT_F, D2D_SIZE_U,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1Bitmap, ID2D1Bitmap1, ID2D1Brush, ID2D1Device, ID2D1DeviceContext,
    ID2D1Factory1, ID2D1GeometrySink, ID2D1GradientStopCollection, ID2D1LinearGradientBrush,
    ID2D1RenderTarget, ID2D1SolidColorBrush, D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
    D2D1_BITMAP_OPTIONS_NONE, D2D1_BITMAP_OPTIONS_TARGET, D2D1_BITMAP_PROPERTIES1,
    D2D1_DEVICE_CONTEXT_OPTIONS_NONE, D2D1_ELLIPSE, D2D1_EXTEND_MODE_CLAMP,
    D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_GAMMA_2_2, D2D1_INTERPOLATION_MODE_LINEAR,
    D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES, D2D1_UNIT_MODE_PIXELS,
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
use windows_numerics::{Matrix3x2, Vector2};

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
    d2d_factory: ID2D1Factory1,
    brushes: Vec<ID2D1SolidColorBrush>,
    /// Sombra azulada projetada pela neve sobre o vidro/moldura.
    shadow_brushes: Vec<ID2D1SolidColorBrush>,
    /// Gradiente vertical da camada de neve: branco no topo, azul-acinzentado
    /// na base (dá volume sem "chapas" brancas).
    pile_gradient: ID2D1LinearGradientBrush,
    _pile_stops: ID2D1GradientStopCollection,
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
        let mut shadow_brushes = Vec::with_capacity(ALPHA_LEVELS + 1);
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

            let shadow_color = D2D1_COLOR_F {
                r: 0.58,
                g: 0.68,
                b: 0.84,
                a,
            };
            let shadow_brush = unsafe { d2d_context.CreateSolidColorBrush(&shadow_color, None) }
                .map_err(|e| format!("CreateSolidColorBrush(sombra): {e}"))?;
            shadow_brushes.push(shadow_brush);
        }

        // --- gradiente da neve acumulada -----------------------------------
        let stops = [
            D2D1_GRADIENT_STOP {
                position: 0.0,
                color: D2D1_COLOR_F {
                    r: 1.0,
                    g: 1.0,
                    b: 1.0,
                    a: 1.0,
                },
            },
            D2D1_GRADIENT_STOP {
                position: 0.55,
                color: D2D1_COLOR_F {
                    r: 0.965,
                    g: 0.978,
                    b: 1.0,
                    a: 0.97,
                },
            },
            D2D1_GRADIENT_STOP {
                position: 1.0,
                color: D2D1_COLOR_F {
                    r: 0.78,
                    g: 0.85,
                    b: 0.97,
                    a: 0.94,
                },
            },
        ];
        let rt: &ID2D1RenderTarget = &d2d_context;
        let stop_collection = unsafe {
            rt.CreateGradientStopCollection(&stops, D2D1_GAMMA_2_2, D2D1_EXTEND_MODE_CLAMP)
        }
        .map_err(|e| format!("CreateGradientStopCollection: {e}"))?;
        let gradient_props = D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES {
            startPoint: Vector2 { X: 0.0, Y: 0.0 },
            endPoint: Vector2 { X: 0.0, Y: 1.0 },
        };
        let pile_gradient = unsafe {
            d2d_context.CreateLinearGradientBrush(&gradient_props, None, &stop_collection)
        }
        .map_err(|e| format!("CreateLinearGradientBrush(neve): {e}"))?;

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
            d2d_factory,
            brushes,
            shadow_brushes,
            pile_gradient,
            _pile_stops: stop_collection,
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

    #[inline]
    fn shadow_brush(&self, alpha: f32) -> &ID2D1SolidColorBrush {
        let idx =
            ((alpha.clamp(0.0, 1.0) * ALPHA_LEVELS as f32).round() as usize).min(ALPHA_LEVELS);
        &self.shadow_brushes[idx]
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

    /// Monte de neve com silhueta contínua e orgânica.
    ///
    /// As células da simulação são apenas amostras de altura. Aqui são unidas
    /// por uma curva Catmull-Rom (convertida em Béziers cúbicas) e recebem um
    /// pequeno ruído de baixa frequência: o resultado é uma camada de neve com
    /// lóbulos e irregularidades suaves, em vez de retângulos por célula.
    fn draw_pile(&self, pile: &Pile) {
        let n = pile.cells.len();
        if n == 0 {
            return;
        }
        let alpha = pile.alpha();
        if alpha <= 0.02 {
            return;
        }
        let max_h = pile.max_height();
        if max_h < 0.12 {
            return;
        }
        // Camada de base contínua: a deposição real concentra-se em faixas
        // (vento), mas uma superfície com neve nunca fica com "buracos"
        // alternados. O véu fino preenche os intervalos entre os montes.
        let base = (max_h * 0.22).clamp(0.0, 1.25);

        let x0 = pile.rect.x;
        let base_y = pile.rect.y + pile.slide_y;
        // A neve "drapa" 1-2 px pela fachada, em vez de terminar numa linha
        // perfeitamente reta.
        let overhang = (max_h * 0.16).clamp(0.4, 2.2);
        let bottom = base_y + overhang;
        let seed = (pile.id as f32) * 1.618 + x0 * 0.017;
        let Ok(geometry) = (unsafe { self.d2d_factory.CreatePathGeometry() }) else {
            return;
        };
        let Ok(sink) = (unsafe { geometry.Open() }) else {
            return;
        };

        // Segmentos visíveis (as células tapadas por janelas da frente não são
        // desenhadas). Cada troço contínuo dá origem a uma figura fechada.
        let mut run_start: Option<usize> = None;
        for i in 0..=n {
            let visible =
                i < n && pile.cells[i] + base >= 0.12 && self.cell_visible(pile, i, base_y, base);
            if visible {
                if run_start.is_none() {
                    run_start = Some(i);
                }
            } else if let Some(start) = run_start.take() {
                add_snow_run(&sink, pile, start, i, x0, base_y, bottom, seed, base);
            }
        }
        unsafe {
            let _ = sink.Close();
        }

        // Sombra azulada muito ligeira por baixo da neve: dá volume e faz a
        // camada "assentar" no vidro em vez de flutuar.
        let shadow_alpha = alpha * 0.30;
        if shadow_alpha > 0.02 {
            let shadow = Matrix3x2 {
                M11: 1.0,
                M12: 0.0,
                M21: 0.0,
                M22: 1.0,
                M31: 0.55,
                M32: 1.35,
            };
            unsafe {
                let _ = self.d2d_context.SetTransform(&shadow);
                let _ = self.d2d_context.FillGeometry(
                    &geometry,
                    self.shadow_brush(shadow_alpha),
                    None::<&ID2D1Brush>,
                );
                // O Direct2D não aceita um ponteiro nulo aqui em todas as
                // versões; repõe a identidade explicitamente.
                let identity = Matrix3x2 {
                    M11: 1.0,
                    M12: 0.0,
                    M21: 0.0,
                    M22: 1.0,
                    M31: 0.0,
                    M32: 0.0,
                };
                let _ = self.d2d_context.SetTransform(&identity);
            }
        }

        unsafe {
            self.pile_gradient.SetStartPoint(Vector2 {
                X: 0.0,
                Y: base_y - max_h - 3.0,
            });
            self.pile_gradient
                .SetEndPoint(Vector2 { X: 0.0, Y: bottom });
            self.pile_gradient.SetOpacity(alpha);
            let _ =
                self.d2d_context
                    .FillGeometry(&geometry, &self.pile_gradient, None::<&ID2D1Brush>);
        }
    }

    /// `true` se a célula `i` não estiver tapada por uma janela mais acima.
    fn cell_visible(&self, pile: &Pile, i: usize, base_y: f32, base: f32) -> bool {
        let cw = pile.cell_w;
        let h = pile.cells[i] + base;
        let left = pile.rect.x + i as f32 * cw;
        let top = base_y - h;
        let cell = Rect::new(left, top, cw, base_y - top + 0.8);
        !self
            .occluders
            .iter()
            .any(|(z, r)| *z < pile.z && intersects(*r, cell))
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

/// Acrescenta uma figura fechada com o perfil suave de um troço de neve.
#[allow(clippy::too_many_arguments)]
fn add_snow_run(
    sink: &ID2D1GeometrySink,
    pile: &Pile,
    start: usize,
    end: usize,
    x0: f32,
    base_y: f32,
    bottom: f32,
    seed: f32,
    base: f32,
) {
    let n = pile.cells.len();
    let cw = pile.cell_w;
    let left = x0 + start as f32 * cw;
    let right = x0 + end as f32 * cw;
    let edge_left = start == 0;
    let edge_right = end == n;
    // Nas bordas exteriores da superfície a neve prolonga-se um pouco e
    // "morre" suavemente, como um lábio de neve sobre a aresta.
    let lip = 1.8;
    let pl = left - if edge_left { lip } else { 0.0 };
    let pr = right + if edge_right { lip } else { 0.0 };

    let mut points: Vec<Vector2> = Vec::with_capacity(end - start + 2);
    if edge_left {
        let x = left + cw * 0.5;
        let h = snow_top(pile, x, seed, base);
        points.push(Vector2 {
            X: pl,
            Y: base_y - h * 0.35,
        });
    }
    for i in start..end {
        let x = x0 + (i as f32 + 0.5) * cw;
        points.push(Vector2 {
            X: x,
            Y: base_y - snow_top(pile, x, seed, base),
        });
    }
    if edge_right {
        let x = right - cw * 0.5;
        let h = snow_top(pile, x, seed, base);
        points.push(Vector2 {
            X: pr,
            Y: base_y - h * 0.35,
        });
    }

    if points.len() == 1 {
        let p = points[0];
        points.insert(
            0,
            Vector2 {
                X: p.X - cw * 0.30,
                Y: p.Y + 0.15,
            },
        );
        points.push(Vector2 {
            X: p.X + cw * 0.30,
            Y: p.Y + 0.15,
        });
    } else if points.is_empty() {
        return;
    }

    // Catmull-Rom -> Bézier cúbica. A tensão ligeiramente inferior a 1 evita
    // que o ruído crie pequenos "saltos" acima/abaixo da altura simulada.
    let mut beziers: Vec<D2D1_BEZIER_SEGMENT> = Vec::with_capacity(points.len());
    const TENSION: f32 = 6.8;
    for i in 0..points.len() - 1 {
        let p0 = points[i.saturating_sub(1)];
        let p1 = points[i];
        let p2 = points[i + 1];
        let p3 = points[(i + 2).min(points.len() - 1)];
        beziers.push(D2D1_BEZIER_SEGMENT {
            point1: Vector2 {
                X: p1.X + (p2.X - p0.X) / TENSION,
                Y: p1.Y + (p2.Y - p0.Y) / TENSION,
            },
            point2: Vector2 {
                X: p2.X - (p3.X - p1.X) / TENSION,
                Y: p2.Y - (p3.Y - p1.Y) / TENSION,
            },
            point3: p2,
        });
    }

    unsafe {
        sink.BeginFigure(Vector2 { X: pl, Y: bottom }, D2D1_FIGURE_BEGIN_FILLED);
        sink.AddLine(points[0]);
        sink.AddBeziers(&beziers);
        sink.AddLine(Vector2 { X: pr, Y: bottom });
        sink.EndFigure(D2D1_FIGURE_END_CLOSED);
    }
}

/// Altura da superfície visível em `x`, incluindo o relevo do renderer.
///
/// Combina três escalas: deriva lenta (o manto não tem espessura uniforme),
/// lóbulos médios (grumos de neve) e uma textura fina que quebra a linha
/// perfeita do topo. A deriva é proporcional à espessura; o ruído fino é
/// limitado para não parecer serrilhado.
fn snow_top(pile: &Pile, x: f32, seed: f32, base: f32) -> f32 {
    let h = pile.height_at(x) + base;
    let drift = 0.11 * (x * 0.031 + seed * 3.1).sin()
        + 0.075 * (x * 0.082 + seed * 1.7).sin()
        + 0.045 * (x * 0.19 + seed * 0.6).sin();
    let amp = (h.min(14.0) / 14.0) * 0.85 + 0.05;
    let lumps = amp
        * (0.55 * (x * 0.44 + seed * 2.3).sin()
            + 0.34 * (x * 0.97 + seed * 0.9).sin()
            + 0.14 * (x * 2.1 + seed * 1.3).sin());
    (h * (1.0 + drift) + lumps).max(0.10)
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
