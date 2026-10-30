//! Simulação física da neve e acumulação sobre superfícies.
//!
//! Modelo (deliberadamente simples e estável a 60 FPS):
//!
//! * Cada floco é uma partícula pontual com velocidade vertical própria, uma
//!   oscilação lateral harmónica (seno com fase/frequência individuais) e um
//!   fator de sensibilidade ao vento. O vento global é a soma de três senos de
//!   períodos diferentes — barato, determinístico e sem ruído de alta
//!   frequência (o que evita "trepidação" visual).
//! * A neve acumulada é guardada por superfície (janela ou barra de tarefas)
//!   num vetor de alturas (uma por célula de `CELL_W` px). Cada floco que
//!   atravessa o topo de uma superfície deposita volume na célula
//!   correspondente (e um pouco nas vizinhas), o que produz montes orgânicos.
//! * As alturas sofrem difusão (alisamento) e derretimento lento, atingindo um
//!   equilíbrio dependente da intensidade.
//! * Quando uma janela se move, é minimizada ou fechada, o monte é "desligado"
//!   da janela e passa a escorregar para baixo enquanto perde opacidade.

use crate::geometry::Rect;

/// Largura (px físicos) de cada célula de acumulação. Células de 2 px permitem
/// curvas contínuas e um alisamento térmico sem facetas visíveis.
const CELL_W: f32 = 2.0;
/// Número máximo de células por monte (ecrãs 8K continuam cobertos).
const MAX_CELLS: usize = 4096;
/// Altura máxima (px) da neve sobre o topo de uma janela.
pub const MAX_WINDOW_SNOW: f32 = 24.0;
/// Altura máxima (px) da neve sobre a barra de tarefas. Mantém os ícones e a
/// navegação visíveis.
pub const MAX_TASKBAR_SNOW: f32 = 48.0;
/// Derretimento passivo (px/s) por célula. Zero: o excesso é redistribuído ou
/// descartado nos limites, não é preciso derreter a camada inteira.
const MELT_RATE: f32 = 0.0;
/// Derretimento acelerado (px/s) quando o utilizador escolhe "Melt Snow".
const FAST_MELT_RATE: f32 = 120.0;
/// Teto absoluto de partículas, independentemente da resolução.
pub const MAX_PARTICLES: usize = 3000;

/// Velocidade de escavação: fração de altura removida por segundo no centro
/// do raio do cursor (com falloff suave até à borda).
const CARVE_RATE: f32 = 12.0;
/// Número máximo de partículas soltas (efeito de escavação).
const MAX_LOOSE: usize = 320;
/// Gravidade aplicada às partículas soltas (px/s²).
const LOOSE_GRAVITY: f32 = 1400.0;
/// Velocidade do cursor (px/s) a partir da qual se soltam partículas.
const LOOSE_SPEED_MIN: f32 = 220.0;
/// Distância (px) a partir do topo em que a neve no bordo de uma janela
/// transborda (só quando a coluna está praticamente cheia).
const EDGE_SPILL_MARGIN: f32 = 3.0;
/// Fração do excesso no bordo que transborda por segundo.
const EDGE_SPILL_RATE: f32 = 0.8;
/// Máximo de partículas de transbordo criadas por monte/frame.
const EDGE_SPILL_BUDGET: usize = 8;
/// Raio máximo (em células) para onde o excesso de uma célula cheia escorrega.
const CAP_SPREAD_CELLS: isize = 40;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SurfaceKind {
    Window,
    Taskbar,
}

/// Superfície detetada pela camada Win32, em coordenadas locais do overlay.
#[derive(Clone, Copy, Debug)]
pub struct Surface {
    pub id: isize,
    pub kind: SurfaceKind,
    pub rect: Rect,
    /// Posição na ordem Z (0 = mais acima). Usada para não desenhar neve de
    /// janelas de fundo por cima das janelas que as tapam.
    pub z: i32,
}

#[derive(Clone, Copy, Debug)]
pub struct Particle {
    pub x: f32,
    pub y: f32,
    /// Velocidade terminal de queda (px/s).
    pub vy: f32,
    pub r: f32,
    pub alpha: f32,
    /// Fase atual da oscilação lateral.
    pub phase: f32,
    pub sway_freq: f32,
    pub sway_amp: f32,
    pub wind_scale: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PileState {
    /// Preso a uma superfície viva; recebe e cede neve.
    Settled,
    /// Destacado (janela moveu/fechou): escorrega e desaparece.
    Sliding,
}

#[derive(Debug)]
pub struct Pile {
    /// HWND de origem; 0 quando o monte já foi destacado.
    pub id: isize,
    /// Tipo de superfície (janela ou barra de tarefas).
    pub kind: SurfaceKind,
    /// Retângulo da superfície (apenas o topo e a largura são usados).
    pub rect: Rect,
    /// Altura de neve por célula, em px.
    pub cells: Vec<f32>,
    /// Marca de escavação por célula (0 = intacta, 1 = buraco). Impede a
    /// difusão/deslizamento de preencher o buraco por baixo; só a deposição de
    /// novos flocos o reduz.
    pub hole: Vec<f32>,
    /// Buffer auxiliar para a difusão (evita alocar por frame).
    scratch: Vec<f32>,
    pub cell_w: f32,
    /// Altura máxima admissível.
    max_h: f32,
    /// Ordem Z da superfície de origem.
    pub z: i32,
    state: PileState,
    pub slide_y: f32,
    slide_v: f32,
    fade: f32,
    melting: bool,
}

/// Partícula solta projetada pela passagem rápida do cursor sobre a neve.
#[derive(Clone, Copy, Debug)]
pub struct LooseSnow {
    pub x: f32,
    pub y: f32,
    pub vx: f32,
    pub vy: f32,
    pub r: f32,
    pub alpha: f32,
}

impl Pile {
    fn new(s: Surface) -> Self {
        let n = ((s.rect.w / CELL_W).ceil() as usize).clamp(1, MAX_CELLS);
        // Em ecrãs muito largos o limite de células é atingido; nesse caso a
        // largura da célula cresce para que a última coincida com a borda.
        let cell_w = s.rect.w / n as f32;
        Self {
            id: s.id,
            kind: s.kind,
            rect: s.rect,
            cells: vec![0.0; n],
            hole: vec![0.0; n],
            scratch: vec![0.0; n],
            cell_w,
            max_h: if s.kind == SurfaceKind::Taskbar {
                MAX_TASKBAR_SNOW
            } else {
                MAX_WINDOW_SNOW
            },
            z: s.z,
            state: PileState::Settled,
            slide_y: 0.0,
            slide_v: 0.0,
            fade: 1.0,
            melting: false,
        }
    }

    /// Altura da neve em `x` (interpolação linear entre células).
    pub fn height_at(&self, x: f32) -> f32 {
        let n = self.cells.len();
        if n == 0 {
            return 0.0;
        }
        let f = (x - self.rect.x) / self.cell_w - 0.5;
        let i = f.floor();
        let t = f - i;
        let a = self.cells[i.clamp(0.0, (n - 1) as f32) as usize];
        let b = self.cells[(i + 1.0).clamp(0.0, (n - 1) as f32) as usize];
        a + (b - a) * t
    }

    /// Intensidade do buraco escavado em `x` (0 = intacto, 1 = vazio).
    pub fn hole_at(&self, x: f32) -> f32 {
        let n = self.hole.len();
        if n == 0 {
            return 0.0;
        }
        let f = (x - self.rect.x) / self.cell_w - 0.5;
        let i = f.floor();
        let t = f - i;
        let a = self.hole[i.clamp(0.0, (n - 1) as f32) as usize];
        let b = self.hole[(i + 1.0).clamp(0.0, (n - 1) as f32) as usize];
        a + (b - a) * t
    }

    /// Deposição de um floco de raio `r` na abcissa `x`.
    ///
    /// O volume é distribuído por uma gaussiana estreita (em vez de uma coluna
    /// única), o que produz grumos arredondados em vez de torres alinhadas �
    /// grelha de células. O total depositado é independente da largura da
    /// célula.
    fn deposit(&mut self, x: f32, r: f32) {
        let n = self.cells.len();
        if n == 0 || !x.is_finite() {
            return;
        }
        let center = (x - self.rect.x) / self.cell_w;
        let sigma = (1.9 + r * 0.85) / self.cell_w;
        if !(sigma > 0.0) {
            return;
        }
        let reach = (sigma * 2.6).ceil() as isize;
        let lo = (center.floor() as isize - reach).max(0);
        let hi = (center.floor() as isize + reach).min(n as isize - 1);
        if lo > hi {
            return;
        }

        let mut wsum = 0.0;
        for i in lo..=hi {
            let d = (i as f32 + 0.5 - center) / sigma;
            wsum += (-0.5 * d * d).exp();
        }
        if !(wsum > 0.0) {
            return;
        }

        // Volume total do floco convertido em altura somada. Os flocos maiores
        // acrescentam mais neve; o valor não depende de `cell_w`.
        let amount = 1.1 + r * 0.62;
        let mut excess = 0.0;
        for i in lo..=hi {
            let d = (i as f32 + 0.5 - center) / sigma;
            let w = (-0.5 * d * d).exp() / wsum;
            let idx = i as usize;
            let before = self.cells[idx];
            let desired = before + amount * w;
            let after = desired.min(self.max_h);
            self.cells[idx] = after;
            excess += (desired - after).max(0.0);
            let added = after - before;
            if added > 0.0 && self.hole[idx] > 0.0 {
                self.hole[idx] = (self.hole[idx] - added * 0.08).max(0.0);
            }
        }
        if excess > 1e-4 {
            // No teto, os flocos novos escorregam para as células vizinhas
            // mais baixas. O que não couber derrete.
            self.redistribute_excess(center.round() as isize, excess);
        }
    }

    /// Espalha o excesso de uma célula cheia para as células mais baixas mais
    /// próximas (até `CAP_SPREAD_CELLS`), alternando esquerda/direita. O que
    /// não couber é descartado, como neve que derrete no topo.
    fn redistribute_excess(&mut self, center: isize, mut excess: f32) {
        let n = self.cells.len() as isize;
        if n == 0 || excess <= 0.0 {
            return;
        }
        for dist in 1..=CAP_SPREAD_CELLS {
            for side in [-1isize, 1isize] {
                if excess <= 1e-4 {
                    return;
                }
                let idx = center + side * dist;
                if idx < 0 || idx >= n {
                    continue;
                }
                let idx = idx as usize;
                let room = (self.max_h - self.cells[idx]).max(0.0);
                if room <= 0.0 {
                    continue;
                }
                let add = room.min(excess);
                self.cells[idx] += add;
                excess -= add;
                if self.hole[idx] > 0.0 {
                    self.hole[idx] = (self.hole[idx] - add * 0.5).max(0.0);
                }
            }
        }
    }

    /// Relaxamento térmico + deslizamento (ângulo de repouso).
    ///
    /// Duas passagens do filtro `(h[i-1] + 2*h[i] + h[i+1]) / 4` por tick dão o
    /// comportamento de fluido viscoso denso: a superfície torna-se contínua e
    /// sem dentes. O deslizamento corrige declives acima do ângulo de repouso.
    fn relax(&mut self, dt: f32) {
        let n = self.cells.len();
        if n < 3 {
            return;
        }

        for _ in 0..2 {
            for i in 0..n {
                let l = self.cells[i.saturating_sub(1)];
                let r = self.cells[(i + 1).min(n - 1)];
                self.scratch[i] = (l + 2.0 * self.cells[i] + r) * 0.25;
            }
            std::mem::swap(&mut self.cells, &mut self.scratch);
        }

        // A marca de escavação desvanece: a camada de base volta a cobrir a
        // cratera à medida que o relevo assenta.
        let hole_decay = dt * 0.8;
        for h in &mut self.hole {
            *h = (*h - hole_decay).max(0.0);
        }

        // Inclinação máxima admissível. Com células de 2 px, 0,9 px por célula
        // correspondem a ~24°, próximo do ângulo de repouso.
        let max_step = 0.9;
        let slip_k = (dt * 9.0).min(0.55);
        for _ in 0..8 {
            for i in 0..n - 1 {
                let d = self.cells[i] - self.cells[i + 1];
                if d.abs() <= max_step {
                    continue;
                }
                let slip = (d.abs() - max_step) * slip_k;
                if d > 0.0 {
                    self.cells[i] -= slip;
                    self.cells[i + 1] += slip;
                } else {
                    self.cells[i] += slip;
                    self.cells[i + 1] -= slip;
                }
            }
            for i in (0..n - 1).rev() {
                let d = self.cells[i] - self.cells[i + 1];
                if d.abs() <= max_step {
                    continue;
                }
                let slip = (d.abs() - max_step) * slip_k;
                if d > 0.0 {
                    self.cells[i] -= slip;
                    self.cells[i + 1] += slip;
                } else {
                    self.cells[i] += slip;
                    self.cells[i + 1] -= slip;
                }
            }
        }
    }

    /// Escava um buraco centrado em `x` com o raio dado. O volume removido é
    /// empurrado para cristas laterais, com mais massa à frente do movimento
    /// (`dir_x` entre -1 e 1). Devolve o volume que não coube no monte (bordo
    /// esquerdo, bordo direito), para o chamador o transformar em neve em
    /// queda ou o varrer para fora do ecrã.
    fn carve(&mut self, x: f32, radius: f32, dt: f32, dir_x: f32) -> (f32, f32) {
        let n = self.cells.len();
        if n == 0 || radius <= 0.0 || !x.is_finite() {
            return (0.0, 0.0);
        }
        let cw = self.cell_w;
        let center = (x - self.rect.x) / cw;
        let r_cells = radius / cw;
        let lo = (center - r_cells).floor().max(0.0) as usize;
        let hi = ((center + r_cells).ceil() as isize).clamp(0, n as isize - 1) as usize;

        let mut removed_left = 0.0;
        let mut removed_right = 0.0;
        for i in lo..=hi {
            let cx = self.rect.x + (i as f32 + 0.5) * cw;
            let d = (cx - x).abs();
            if d > radius {
                continue;
            }
            // Falloff smoothstep: derivada nula nas extremidades, pelo que a
            // cratera se funde na superfície sem arestas.
            let f = 1.0 - d / radius;
            let f = f * f * (3.0 - 2.0 * f);
            if self.hole[i] < f {
                self.hole[i] = f;
            }
            let cut = self.cells[i] * (1.0 - (-dt * CARVE_RATE * f).exp());
            if cut <= 0.0 {
                continue;
            }
            self.cells[i] -= cut;
            if cx < x {
                removed_left += cut;
            } else {
                removed_right += cut;
            }
        }

        // Reparte o volume total: a maior parte segue a direção do movimento,
        // criando um monte à frente; o resto fica atrás. A soma é conservada.
        let total = removed_left + removed_right;
        if total <= 0.0 {
            return (0.0, 0.0);
        }
        let dir = dir_x.clamp(-1.0, 1.0);
        let forward = total * (0.5 + 0.35 * dir.abs());
        let back = total - forward;
        let (left_amount, right_amount) = if dir >= 0.0 {
            (back, forward)
        } else {
            (forward, back)
        };

        let reach = (r_cells.ceil() as isize).max(1) + 1;
        let center_i = center.round() as isize;
        let span = ((r_cells * 0.9).round() as isize).clamp(3, 10);
        let overflow_left = push_lobe(self, center_i - reach, -1, left_amount, span);
        let overflow_right = push_lobe(self, center_i + reach, 1, right_amount, span);
        (overflow_left, overflow_right)
    }

    fn decay(&mut self, amount: f32) {
        for c in &mut self.cells {
            *c = (*c - amount).max(0.0);
        }
    }

    pub fn max_height(&self) -> f32 {
        self.cells.iter().copied().fold(0.0, f32::max)
    }

    pub fn alpha(&self) -> f32 {
        match self.state {
            // Neve acumulada é totalmente opaca.
            PileState::Settled => 1.0,
            PileState::Sliding => self.fade.clamp(0.0, 1.0),
        }
    }

    /// `true` se o monte já se desprendeu da janela (a escorregar).
    pub fn sliding(&self) -> bool {
        self.state == PileState::Sliding
    }

    /// Desliga o monte da superfície: começa a escorregar e a desvanecer.
    fn detach(&mut self) {
        if self.state == PileState::Settled {
            self.state = PileState::Sliding;
            self.id = 0;
            self.slide_v = 0.0;
            self.fade = 1.0;
            self.melting = false;
        }
    }

    fn is_dead(&self) -> bool {
        self.state == PileState::Sliding && (self.fade <= 0.02 || self.slide_y > 6000.0)
    }
}

/// Distribui `amount` de neve numa faixa de `span` células a partir de `start`,
/// na direção `dir` (crista lateral criada pela escavação). A altura decai
/// linearmente para o exterior. Devolve o volume que não pôde ser colocado
/// (fora do monte ou acima de `max_h`), para transbordar.
fn push_lobe(pile: &mut Pile, start: isize, dir: isize, amount: f32, span: isize) -> f32 {
    if amount <= 0.0 || span <= 0 {
        return amount.max(0.0);
    }
    let n = pile.cells.len() as isize;
    let span = span.max(1);
    let wsum = (span * (span + 1) / 2) as f32;
    let mut added_total = 0.0;
    for k in 1..=span {
        let j = start + dir * (k - 1);
        if j < 0 || j >= n {
            continue;
        }
        let j = j as usize;
        let w = (span - k + 1) as f32 / wsum;
        let before = pile.cells[j];
        let after = (before + amount * w).min(pile.max_h);
        pile.cells[j] = after;
        let added = after - before;
        added_total += added;
        if added > 0.0 && pile.hole[j] > 0.0 {
            pile.hole[j] = (pile.hole[j] - added * 0.5).max(0.0);
        }
    }
    (amount - added_total).max(0.0)
}

pub struct World {
    pub width: f32,
    pub height: f32,
    pub time: f32,
    pub particles: Vec<Particle>,
    pub piles: Vec<Pile>,
    /// Partículas soltas projetadas pelo cursor ao escavar.
    pub loose: Vec<LooseSnow>,
    spawn_acc: f32,
    /// Última posição do cursor (para calcular a velocidade).
    cursor_last: Option<(f32, f32)>,
    loose_acc: f32,
    rng: u32,
}

impl World {
    pub fn new(width: f32, height: f32) -> Self {
        Self {
            width,
            height,
            time: 0.0,
            particles: Vec::with_capacity(MAX_PARTICLES.min(1024)),
            piles: Vec::with_capacity(32),
            loose: Vec::with_capacity(MAX_LOOSE),
            spawn_acc: 0.0,
            cursor_last: None,
            loose_acc: 0.0,
            rng: 0x9E37_79B9,
        }
    }

    /// Muda o tamanho do "mundo" (resolução/ecrãs alterados).
    pub fn resize(&mut self, width: f32, height: f32) {
        self.width = width;
        self.height = height;
        self.particles
            .retain(|p| p.x >= -40.0 && p.x <= width + 40.0 && p.y < height + 40.0);
        self.loose
            .retain(|p| p.x >= -40.0 && p.x <= width + 40.0 && p.y < height + 40.0);
        self.cursor_last = None;
        // Montes antigos deixam de fazer sentido: desaparecem suavemente.
        for p in &mut self.piles {
            p.detach();
        }
    }

    /// Limpa tudo (usado ao desligar a neve).
    pub fn reset(&mut self) {
        self.particles.clear();
        self.loose.clear();
        self.piles.clear();
        self.spawn_acc = 0.0;
        self.loose_acc = 0.0;
        self.cursor_last = None;
        self.time = 0.0;
    }

    /// Força o derretimento de toda a neve acumulada (ação "Melt Snow").
    pub fn melt_all(&mut self) {
        for p in &mut self.piles {
            p.melting = true;
            p.hole.fill(0.0);
        }
    }

    /// Sincroniza montes com as superfícies detetadas.
    ///
    /// * nova superfície -> monte vazio;
    /// * superfície que se moveu/redimensionou -> o monte antigo desprende-se e
    ///   nasce um novo no local atual (a neve "cai" da posição anterior);
    /// * superfície desapareceu -> o monte desprende-se e escorrega.
    pub fn sync_surfaces(&mut self, surfaces: &[Surface], accumulate: bool) {
        if !accumulate {
            for p in &mut self.piles {
                p.detach();
            }
            return;
        }

        for s in surfaces {
            match self
                .piles
                .iter()
                .position(|p| p.state == PileState::Settled && p.id == s.id)
            {
                Some(idx) => {
                    let p = &self.piles[idx];
                    let moved = (p.rect.x - s.rect.x).abs() > 1.0
                        || (p.rect.y - s.rect.y).abs() > 1.0
                        || (p.rect.w - s.rect.w).abs() > 1.0;
                    if moved {
                        self.piles[idx].detach();
                        self.piles.push(Pile::new(*s));
                    } else {
                        self.piles[idx].rect = s.rect;
                        self.piles[idx].z = s.z;
                    }
                }
                None => self.piles.push(Pile::new(*s)),
            }
        }

        // Superfícies que desapareceram desde o último varrimento.
        for i in 0..self.piles.len() {
            let p = &self.piles[i];
            if p.state == PileState::Settled && !surfaces.iter().any(|s| s.id == p.id) {
                self.piles[i].detach();
            }
        }

        self.piles.retain(|p| !p.is_dead());
    }

    /// `true` se o ponto `(x, y)` estiver tapado por uma janela acima do monte
    /// `idx` na ordem Z.
    fn point_covered(&self, x: f32, y: f32, idx: usize) -> bool {
        let z = self.piles[idx].z;
        self.piles.iter().enumerate().any(|(j, other)| {
            j != idx
                && other.state == PileState::Settled
                && other.z < z
                && x >= other.rect.x
                && x <= other.rect.right()
                && y >= other.rect.y
                && y <= other.rect.bottom()
        })
    }

    /// xorshift32: rápido, sem dependências, suficiente para jitter visual.
    fn rand01(&mut self) -> f32 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.rng = x;
        (x & 0x00FF_FFFF) as f32 / 16_777_216.0
    }

    fn range(&mut self, a: f32, b: f32) -> f32 {
        a + (b - a) * self.rand01()
    }

    fn spawn(&mut self) {
        if self.particles.len() >= MAX_PARTICLES {
            return;
        }
        let r = self.range(1.0, 3.1);
        let p = Particle {
            x: self.range(0.0, self.width),
            // Nascem ligeiramente acima do topo para entrarem suavemente.
            y: self.range(-24.0, -1.0),
            vy: self.range(38.0, 96.0) * (0.75 + r * 0.16),
            r,
            alpha: self.range(0.40, 0.95),
            phase: self.range(0.0, std::f32::consts::TAU),
            sway_freq: self.range(0.7, 2.4),
            sway_amp: self.range(4.0, 22.0),
            wind_scale: self.range(0.35, 1.45),
        };
        self.particles.push(p);
    }

    /// Vento global em px/s: soma de três senos com períodos distintos.
    pub fn wind(&self) -> f32 {
        let t = self.time;
        26.0 * (t * 0.19).sin() + 13.0 * (t * 0.47 + 1.7).sin() + 6.5 * (t * 1.07 + 0.4).sin()
    }

    /// Interação do cursor: escava a neve sob o ponteiro e, se o movimento for
    /// rápido, projeta partículas soltas para os lados.
    ///
    /// `cursor` está em coordenadas locais do overlay; `None` quando o cursor
    /// não está disponível (ex.: overlay desligado).
    pub fn interact(&mut self, cursor: Option<(f32, f32)>, radius: f32, dt: f32) {
        let Some((x, y)) = cursor else {
            self.cursor_last = None;
            return;
        };
        if !x.is_finite() || !y.is_finite() || !(radius > 0.0) {
            return;
        }
        let dt = dt.max(0.001);
        let (vx, vy) = match self.cursor_last {
            Some((lx, ly)) => ((x - lx) / dt, (y - ly) / dt),
            None => (0.0, 0.0),
        };
        self.cursor_last = Some((x, y));
        let raw_speed = (vx * vx + vy * vy).sqrt();
        // Um salto grande (mudança de monitor, alt-tab) não deve gerar uma
        // rajada de partículas.
        let speed = if raw_speed > 5000.0 { 0.0 } else { raw_speed };

        // Escolhe o monte mais à frente cuja massa de neve contém o cursor.
        let mut target: Option<(usize, f32, f32)> = None; // (índice, superfície, base)
        for (i, pile) in self.piles.iter().enumerate() {
            if pile.state != PileState::Settled || pile.max_height() < 0.05 {
                continue;
            }
            if x < pile.rect.x - 2.0 || x > pile.rect.right() + 2.0 {
                continue;
            }
            let base_y = pile.rect.y + pile.slide_y;
            let top = base_y - pile.height_at(x);
            // Numa janela, só a massa de neve (e uma pequena margem) é
            // interativa; na barra de tarefas toda a barra o é, para que uma
            // camada fina também possa ser afastada.
            let over_snow = if pile.kind == SurfaceKind::Taskbar {
                y >= top - 8.0 && y <= pile.rect.bottom() + 8.0
            } else {
                y >= top - 8.0 && y <= base_y + 8.0
            };
            if !over_snow {
                continue;
            }
            if self.point_covered(x, top, i) {
                continue;
            }
            let better = match target {
                Some((ti, _, _)) => pile.z < self.piles[ti].z,
                None => true,
            };
            if better {
                target = Some((i, top, base_y));
            }
        }

        let Some((pi, surface, base_y)) = target else {
            return;
        };
        let speed_for_particles = speed;
        let dir = if raw_speed > 30.0 {
            (vx / raw_speed.max(1.0)).clamp(-1.0, 1.0)
        } else {
            0.0
        };
        let (overflow_left, overflow_right) = self.piles[pi].carve(x, radius, dt, dir);
        self.handle_carve_overflow(pi, overflow_left, overflow_right);

        if speed_for_particles > LOOSE_SPEED_MIN && self.loose.len() < MAX_LOOSE {
            let count =
                ((speed_for_particles - LOOSE_SPEED_MIN) / 160.0).clamp(0.0, 4.0) * (dt * 60.0);
            self.loose_acc += count;
            let dir = if vx.abs() > 30.0 {
                vx.signum()
            } else if self.range(0.0, 1.0) < 0.5 {
                -1.0
            } else {
                1.0
            };
            while self.loose_acc >= 1.0 && self.loose.len() < MAX_LOOSE {
                self.loose_acc -= 1.0;
                let sx = x + self.range(-7.0, 7.0);
                let sy = y.clamp(surface, base_y) - self.range(0.0, 5.0);
                let vx0 = dir * self.range(60.0, 190.0) + vx.clamp(-400.0, 400.0) * 0.2;
                let vy0 = -self.range(40.0, 190.0) + vy.clamp(-100.0, 100.0) * 0.1;
                let r = self.range(0.8, 1.8);
                self.loose.push(LooseSnow {
                    x: sx,
                    y: sy,
                    vx: vx0,
                    vy: vy0,
                    r,
                    alpha: 1.0,
                });
            }
        }
    }

    /// O bordo de um monte coincide com o limite do ecrã? A barra de tarefas
    /// é sempre tratada como limite; nesses bordos não há transbordo natural
    /// (o excesso empurrado pelo cursor é varrido para fora).
    fn edge_is_screen(&self, pile: &Pile, left: bool) -> bool {
        if pile.kind == SurfaceKind::Taskbar {
            return true;
        }
        if left {
            pile.rect.x <= 0.5
        } else {
            pile.rect.right() >= self.width - 0.5
        }
    }

    /// Encaminha o volume que não coube nas cristas. Nos bordos interiores de
    /// uma janela cai como neve solta; nos limites do ecrã é varrido para fora
    /// do monitor (as partículas saem e são descartadas).
    fn handle_carve_overflow(&mut self, pi: usize, left: f32, right: f32) {
        if left > 0.02 {
            self.spawn_edge_particles(pi, true, left);
        }
        if right > 0.02 {
            self.spawn_edge_particles(pi, false, right);
        }
    }

    /// Cria partículas soltas que caem do canto de uma janela. A massa já foi
    /// retirada do monte por quem chama.
    fn spawn_edge_particles(&mut self, pi: usize, left: bool, amount: f32) {
        if amount <= 0.0 || self.loose.len() >= MAX_LOOSE {
            return;
        }
        let (x_edge, y_edge) = {
            let pile = &self.piles[pi];
            let idx = if left {
                0
            } else {
                pile.cells.len().saturating_sub(1)
            };
            let h = pile.cells.get(idx).copied().unwrap_or(0.0);
            let x = if left {
                pile.rect.x - 2.0
            } else {
                pile.rect.right() + 2.0
            };
            (x, pile.rect.y + pile.slide_y - h)
        };
        // Não emitir de um canto escondido por uma janela da frente.
        if self.point_covered(x_edge, y_edge, pi) {
            return;
        }
        let count = ((amount / 0.5).ceil() as usize).clamp(1, EDGE_SPILL_BUDGET);
        for _ in 0..count {
            if self.loose.len() >= MAX_LOOSE {
                break;
            }
            let dir = if left { -1.0 } else { 1.0 };
            let px = x_edge + self.range(-2.0, 2.0);
            let py = y_edge + self.range(-3.0, 2.0);
            let pvx = dir * self.range(15.0, 75.0);
            let pvy = self.range(-35.0, 25.0);
            let pr = self.range(0.8, 1.7);
            self.loose.push(LooseSnow {
                x: px,
                y: py,
                vx: pvx,
                vy: pvy,
                r: pr,
                alpha: 1.0,
            });
        }
    }

    /// Transbordo natural: a neve acumulada acima de um limiar no bordo de uma
    /// janela desprende-se e cai. Nos cantos do ecrã/barra de tarefas não cai.
    fn spill_edges(&mut self, dt: f32) {
        for pi in 0..self.piles.len() {
            if self.piles[pi].state != PileState::Settled {
                continue;
            }
            let n = self.piles[pi].cells.len();
            if n == 0 {
                continue;
            }
            let left_screen = self.edge_is_screen(&self.piles[pi], true);
            let right_screen = self.edge_is_screen(&self.piles[pi], false);
            let sides = [(true, 0usize, left_screen), (false, n - 1, right_screen)];
            for (left, idx, screen) in sides {
                if screen {
                    continue;
                }
                let h = self.piles[pi].cells[idx];
                let threshold = (self.piles[pi].max_h - EDGE_SPILL_MARGIN).max(0.0);
                if h <= threshold {
                    continue;
                }
                // Não deixar cair neve de um canto escondido por uma janela
                // da frente: as partículas soltas são desenhadas por cima.
                let pile = &self.piles[pi];
                let x_edge = if left {
                    pile.rect.x - 1.0
                } else {
                    pile.rect.right() + 1.0
                };
                let y_edge = pile.rect.y + pile.slide_y - h;
                if self.point_covered(x_edge, y_edge, pi) {
                    continue;
                }
                let amount = ((h - threshold) * EDGE_SPILL_RATE * dt).min(h);
                if amount < 0.05 || self.loose.len() >= MAX_LOOSE {
                    continue;
                }
                self.piles[pi].cells[idx] = h - amount;
                self.spawn_edge_particles(pi, left, amount);
            }
        }
    }

    /// Passo de simulação.
    pub fn step(&mut self, dt: f32, target: usize, spawn_rate: f32) {
        self.time += dt;
        let wind = self.wind();

        // --- acumulação: difusão, derretimento e deslize -------------------
        let mut dead = false;
        for pile in &mut self.piles {
            match pile.state {
                PileState::Settled => {
                    pile.relax(dt);
                    let rate = if pile.melting {
                        FAST_MELT_RATE
                    } else {
                        MELT_RATE
                    };
                    pile.decay(rate * dt);
                    if pile.melting && pile.max_height() <= 0.01 {
                        // Terminado o degelo, volta a acumular normalmente.
                        pile.melting = false;
                    }
                }
                PileState::Sliding => {
                    // Gravidade: a neve escorrega pela fachada da janela.
                    pile.slide_v += 1800.0 * dt;
                    pile.slide_y += pile.slide_v * dt;
                    pile.fade -= dt * 1.35;
                    pile.decay(7.0 * dt);
                    if pile.is_dead() {
                        dead = true;
                    }
                }
            }
        }
        if dead {
            self.piles.retain(|p| !p.is_dead());
        }

        // --- transbordo nos bordos das janelas -----------------------------
        self.spill_edges(dt);

        // --- emissão -------------------------------------------------------
        let target = target.min(MAX_PARTICLES);
        if self.particles.len() < target {
            self.spawn_acc += spawn_rate * dt;
            // Limite por frame para evitar rajadas após uma pausa longa.
            let mut budget = 80;
            while self.spawn_acc >= 1.0 && self.particles.len() < target && budget > 0 {
                self.spawn();
                self.spawn_acc -= 1.0;
                budget -= 1;
            }
            if self.spawn_acc > 4.0 {
                self.spawn_acc = 4.0;
            }
        } else {
            self.spawn_acc = 0.0;
        }

        // --- integração das partículas -------------------------------------
        let mut i = 0;
        while i < self.particles.len() {
            let p = self.particles[i];
            let prev_bottom = p.y + p.r;
            let phase = p.phase + p.sway_freq * dt;
            let x = p.x + (wind * p.wind_scale + p.sway_amp * phase.sin()) * dt;
            let y = p.y + p.vy * dt;
            let bottom = y + p.r;

            // Colisão com o topo da neve acumulada (ou com o topo da
            // superfície quando ainda não há neve).
            let mut landing: Option<(usize, f32)> = None;
            for (pi, pile) in self.piles.iter().enumerate() {
                if pile.state != PileState::Settled {
                    continue;
                }
                // Uma janela maximizada tem o topo no limite do ecrã: a neve
                // que "pousasse" aí ficaria invisível e nunca chegaria à barra
                // de tarefas. Nesse caso o topo não é uma superfície válida.
                if pile.rect.y <= 2.0 {
                    continue;
                }
                if x < pile.rect.x - 1.0 || x > pile.rect.right() + 1.0 {
                    continue;
                }
                let top = pile.rect.y - pile.height_at(x);
                if prev_bottom <= top && bottom >= top {
                    // O topo só recebe neve se estiver realmente visível. Uma
                    // janela tapada por outra não pode "comer" os flocos que
                    // caem por cima da janela da frente.
                    if self.point_covered(x, top, pi) {
                        continue;
                    }
                    // Entre várias superfícies sobrepostas, pousa na mais alta.
                    match landing {
                        Some((_, best)) if best <= top => {}
                        _ => landing = Some((pi, top)),
                    }
                }
            }

            if let Some((pi, _)) = landing {
                // Pequeno desvio lateral: os flocos não caem todos na mesma
                // coluna, o que quebra a periodicidade da grelha e forma
                // grumos irregulares.
                let jitter = self.range(-0.8 - p.r * 0.7, 0.8 + p.r * 0.7);
                self.piles[pi].deposit(x + jitter, p.r);
                self.particles.swap_remove(i);
                continue;
            }

            if x < -40.0 || x > self.width + 40.0 || y > self.height + 20.0 {
                // Saiu do ecrã: é reciclado no topo pela emissão contínua.
                self.particles.swap_remove(i);
                continue;
            }

            let p = &mut self.particles[i];
            p.x = x;
            p.y = y;
            p.phase = phase;
            i += 1;
        }

        // --- partículas soltas (escavação) ---------------------------------
        let mut li = 0;
        while li < self.loose.len() {
            let l = self.loose[li];
            let prev_bottom = l.y + l.r;
            let x = l.x + l.vx * dt;
            let y = l.y + l.vy * dt;
            let vy = l.vy + LOOSE_GRAVITY * dt;
            let vx = l.vx * (1.0 - 1.6 * dt).max(0.0);
            let bottom = y + l.r;

            let mut landing: Option<usize> = None;
            if vy > 0.0 {
                for (pi, pile) in self.piles.iter().enumerate() {
                    if pile.state != PileState::Settled || pile.rect.y <= 2.0 {
                        continue;
                    }
                    if x < pile.rect.x - 1.0 || x > pile.rect.right() + 1.0 {
                        continue;
                    }
                    let top = pile.rect.y + pile.slide_y - pile.height_at(x);
                    if prev_bottom <= top && bottom >= top && !self.point_covered(x, top, pi) {
                        match landing {
                            Some(best) if self.piles[best].rect.y <= pile.rect.y => {}
                            _ => landing = Some(pi),
                        }
                    }
                }
            }

            if let Some(pi) = landing {
                let r = l.r.clamp(0.5, 2.0);
                self.piles[pi].deposit(x, r * 0.7);
                self.loose.swap_remove(li);
                continue;
            }

            if x < -40.0 || x > self.width + 40.0 || y > self.height + 40.0 {
                self.loose.swap_remove(li);
                continue;
            }

            let l = &mut self.loose[li];
            l.x = x;
            l.y = y;
            l.vx = vx;
            l.vy = vy;
            li += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(id: isize, z: i32, x: f32, y: f32, w: f32) -> Surface {
        Surface {
            id,
            kind: SurfaceKind::Window,
            rect: Rect::new(x, y, w, 400.0),
            z,
        }
    }

    fn flake(world: &mut World, x: f32, y: f32) {
        world.particles.push(Particle {
            x,
            y,
            vy: 120.0,
            r: 2.0,
            alpha: 1.0,
            phase: 0.0,
            sway_freq: 0.0,
            sway_amp: 0.0,
            wind_scale: 0.0,
        });
    }

    fn run(world: &mut World, seconds: f32) {
        let steps = (seconds * 60.0) as usize;
        for _ in 0..steps {
            world.step(1.0 / 60.0, 0, 0.0);
        }
    }

    #[test]
    fn flake_lands_on_visible_top_and_accumulates() {
        let mut world = World::new(800.0, 600.0);
        let s = window(42, 1, 100.0, 300.0, 400.0);
        world.sync_surfaces(&[s], true);
        flake(&mut world, 150.0, 280.0);
        run(&mut world, 1.0);
        assert!(world.particles.is_empty(), "o floco devia ter pousado");
        assert!(world.piles[0].max_height() > 0.0, "o monte devia crescer");
    }

    #[test]
    fn covered_top_does_not_intercept_snow() {
        let mut world = World::new(800.0, 600.0);
        // Janela de fundo com o topo em y=300...
        let back = window(1, 2, 100.0, 300.0, 400.0);
        // ...tapada por uma janela da frente que ocupa y=250..450.
        let front = window(2, 1, 0.0, 250.0, 800.0);
        world.sync_surfaces(&[front, back], true);
        flake(&mut world, 150.0, 280.0);
        run(&mut world, 4.0);
        assert!(
            world.particles.is_empty(),
            "o floco devia sair pelo fundo do ecra"
        );
        for pile in &world.piles {
            assert_eq!(
                pile.max_height(),
                0.0,
                "nenhuma superficie visivel devia acumular"
            );
        }
    }

    #[test]
    fn melt_empties_piles_and_allows_new_snow() {
        let mut world = World::new(800.0, 600.0);
        let s = window(7, 1, 100.0, 300.0, 400.0);
        world.sync_surfaces(&[s], true);
        for _ in 0..30 {
            world.piles[0].deposit(150.0, 2.5);
        }
        assert!(world.piles[0].max_height() > 1.0);
        world.melt_all();
        run(&mut world, 4.0);
        assert_eq!(
            world.piles[0].max_height(),
            0.0,
            "a neve devia derreter toda"
        );
        // Depois do degelo o monte volta a aceitar neve normalmente.
        world.piles[0].deposit(150.0, 2.5);
        assert!(world.piles[0].max_height() > 0.0);
    }

    #[test]
    fn moving_window_detaches_old_pile() {
        let mut world = World::new(800.0, 600.0);
        let s = window(9, 1, 100.0, 300.0, 400.0);
        world.sync_surfaces(&[s], true);
        for _ in 0..10 {
            world.piles[0].deposit(150.0, 2.0);
        }
        let moved = window(9, 1, 160.0, 300.0, 400.0);
        world.sync_surfaces(&[moved], true);
        assert_eq!(world.piles.len(), 2, "monte antigo + monte novo");
        assert!(world.piles.iter().any(|p| p.sliding()));
        assert!(world
            .piles
            .iter()
            .any(|p| !p.sliding() && p.max_height() == 0.0));
        run(&mut world, 3.0);
        assert_eq!(world.piles.len(), 1, "o monte antigo devia desaparecer");
    }

    #[test]
    fn closed_window_pile_slides_and_fades() {
        let mut world = World::new(800.0, 600.0);
        let s = window(11, 1, 100.0, 300.0, 400.0);
        world.sync_surfaces(&[s], true);
        for _ in 0..10 {
            world.piles[0].deposit(150.0, 2.0);
        }
        world.sync_surfaces(&[], true);
        assert!(world.piles[0].sliding());
        run(&mut world, 3.0);
        assert!(
            world.piles.is_empty(),
            "o monte desprendido devia ser removido"
        );
    }

    #[test]
    fn disabling_accumulation_detaches_everything() {
        let mut world = World::new(800.0, 600.0);
        let s = window(13, 1, 100.0, 300.0, 400.0);
        world.sync_surfaces(&[s], true);
        world.sync_surfaces(&[s], false);
        assert!(world.piles.iter().all(|p| p.sliding()));
    }

    #[test]
    fn top_edge_at_screen_border_is_not_a_landing_surface() {
        // Janela maximizada (topo em y=0): a neve deve passar e chegar a outras
        // superficies, em vez de desaparecer numa borda invisivel.
        let mut world = World::new(800.0, 600.0);
        let maximized = window(17, 1, 0.0, 0.0, 800.0);
        let taskbar = window(18, 0, 0.0, 560.0, 800.0);
        world.sync_surfaces(&[maximized, taskbar], true);
        flake(&mut world, 400.0, -10.0);
        run(&mut world, 6.0);
        assert!(world.particles.is_empty());
        let taskbar_pile = world.piles.iter().find(|p| p.id == 18).unwrap();
        assert!(
            taskbar_pile.max_height() > 0.0,
            "a barra de tarefas devia receber a neve"
        );
    }

    #[test]
    fn tall_pile_keeps_a_natural_slope() {
        // Deposição concentrada numa faixa estreita (como acontece com o
        // vento): o manto deve espalhar-se lateralmente em vez de formar uma
        // agulha de neve.
        let mut world = World::new(1920.0, 1080.0);
        let s = window(31, 0, 0.0, 1020.0, 1920.0);
        world.sync_surfaces(&[s], true);
        for _ in 0..3600 {
            world.piles[0].deposit(1200.0, 2.0);
            world.piles[0].relax(1.0 / 60.0);
        }
        let cells = &world.piles[0].cells;
        let max_step = cells
            .windows(2)
            .map(|w| (w[0] - w[1]).abs())
            .fold(0.0, f32::max);
        assert!(
            max_step < 4.0,
            "declive excessivo entre células: {max_step:.2} (altura máx {:.1})",
            world.piles[0].max_height()
        );
        assert!(
            world.piles[0].max_height() > 15.0,
            "a neve devia acumular em altura"
        );
        assert!(
            world.piles[0].max_height() <= MAX_WINDOW_SNOW + 0.01,
            "o teto de altura devia ser respeitado"
        );
    }

    #[test]
    fn cursor_push_moves_mass_forward_and_conserves_it() {
        let mut world = World::new(1920.0, 1080.0);
        let s = window(41, 1, 300.0, 300.0, 800.0);
        world.sync_surfaces(&[s], true);
        for _ in 0..200 {
            world.piles[0].deposit(700.0, 2.5);
        }
        let before: f32 = world.piles[0].cells.iter().sum();
        // Cursores: primeiro fixa a posição, depois move para a direita.
        world.interact(Some((650.0, 295.0)), 30.0, 1.0 / 60.0);
        world.interact(Some((700.0, 295.0)), 30.0, 1.0 / 60.0);

        let after: f32 = world.piles[0].cells.iter().sum();
        let tol = before * 0.002 + 0.5;
        assert!(
            (after - before).abs() <= tol,
            "o volume devia conservar-se: {before:.2} -> {after:.2}"
        );

        let left = world.piles[0].height_at(700.0 - 45.0);
        let right = world.piles[0].height_at(700.0 + 45.0);
        assert!(
            right > left,
            "a crista da frente devia ser maior: esq={left:.2} dir={right:.2}"
        );
    }

    #[test]
    fn overflow_at_interior_window_edge_falls_as_particles() {
        let mut world = World::new(1920.0, 1080.0);
        let s = window(43, 1, 200.0, 300.0, 300.0);
        world.sync_surfaces(&[s], true);
        for _ in 0..120 {
            world.piles[0].deposit(230.0, 2.5);
        }
        // Parado, para não confundir com as partículas do movimento rápido.
        world.interact(Some((230.0, 295.0)), 30.0, 1.0 / 60.0);
        world.interact(Some((230.0, 295.0)), 30.0, 1.0 / 60.0);
        assert!(
            !world.loose.is_empty(),
            "o excesso no bordo devia cair como neve solta"
        );
    }

    #[test]
    fn overflow_at_screen_edge_is_swept_off_screen() {
        let mut world = World::new(1920.0, 1080.0);
        let s = window(45, 1, 0.0, 300.0, 300.0);
        world.sync_surfaces(&[s], true);
        for _ in 0..120 {
            world.piles[0].deposit(30.0, 2.5);
        }
        // Parado: sem partículas do movimento rápido.
        world.interact(Some((30.0, 295.0)), 42.0, 1.0 / 60.0);
        assert!(
            world.loose.iter().any(|p| p.vx < 0.0),
            "o excesso no limite esquerdo devia ser varrido para fora"
        );
    }

    #[test]
    fn natural_accumulation_spills_over_a_window_edge() {
        let mut world = World::new(1920.0, 1080.0);
        let s = window(47, 1, 300.0, 300.0, 400.0);
        world.sync_surfaces(&[s], true);
        for _ in 0..200 {
            world.piles[0].deposit(302.0, 2.5);
        }
        // Coluna de bordo cheia: o transbordo deve disparar.
        world.piles[0].cells[0] = world.piles[0].max_h;
        world.spill_edges(1.0);
        assert!(
            !world.loose.is_empty(),
            "o excesso no bordo devia transbordar naturalmente"
        );
    }

    #[test]
    fn snow_height_has_strict_caps() {
        let mut world = World::new(1920.0, 1080.0);
        let win = window(49, 1, 100.0, 300.0, 400.0);
        let bar = Surface {
            id: 50,
            kind: SurfaceKind::Taskbar,
            rect: Rect::new(0.0, 1020.0, 1920.0, 60.0),
            z: 0,
        };
        world.sync_surfaces(&[win, bar], true);
        for _ in 0..600 {
            let wi = world.piles.iter().position(|p| p.id == 49).unwrap();
            world.piles[wi].deposit(300.0, 2.5);
            let bi = world.piles.iter().position(|p| p.id == 50).unwrap();
            world.piles[bi].deposit(900.0, 2.5);
        }
        let win = world.piles.iter().find(|p| p.id == 49).unwrap();
        let bar = world.piles.iter().find(|p| p.id == 50).unwrap();
        assert!(
            win.max_height() <= MAX_WINDOW_SNOW + 0.01,
            "teto da janela excedido: {:.2}",
            win.max_height()
        );
        assert!(
            bar.max_height() <= MAX_TASKBAR_SNOW + 0.01,
            "teto da barra excedido: {:.2}",
            bar.max_height()
        );
        assert!(win.max_height() > MAX_WINDOW_SNOW - 3.0);
        assert!(bar.max_height() > MAX_TASKBAR_SNOW - 3.0);
    }

    #[test]
    fn accumulated_snow_is_fully_opaque() {
        let mut world = World::new(800.0, 600.0);
        let s = window(19, 1, 100.0, 300.0, 400.0);
        world.sync_surfaces(&[s], true);
        assert_eq!(world.piles[0].alpha(), 1.0);
    }

    #[test]
    fn cursor_carves_a_smooth_crater_that_relaxes() {
        let mut world = World::new(800.0, 600.0);
        let s = window(21, 1, 100.0, 300.0, 400.0);
        world.sync_surfaces(&[s], true);
        for _ in 0..120 {
            world.piles[0].deposit(200.0, 2.5);
        }
        let before = world.piles[0].height_at(200.0);
        assert!(before > 1.0);

        for _ in 0..30 {
            world.interact(Some((200.0, 295.0)), 42.0, 1.0 / 60.0);
        }
        let center = world.piles[0].height_at(200.0);
        assert!(center < before * 0.6, "o cursor devia cavar uma cratera");
        assert!(
            world.piles[0].hole_at(200.0) > 0.3,
            "a cratera devia ficar marcada"
        );

        // O relaxamento térmico suaviza as paredes e recompõe parte da altura
        // mesmo sem novos flocos.
        run(&mut world, 1.0);
        let relaxed = world.piles[0].height_at(200.0);
        assert!(relaxed > center, "a cratera devia suavizar/relaxar");
        assert!(relaxed < before, "a depressão devia continuar visível");

        // Neve nova acelera o preenchimento.
        let hole_before = world.piles[0].hole_at(200.0);
        for _ in 0..30 {
            world.piles[0].deposit(200.0, 2.5);
        }
        assert!(world.piles[0].hole_at(200.0) < hole_before);
        assert!(world.piles[0].height_at(200.0) > relaxed);
    }

    #[test]
    fn fast_cursor_throws_loose_snow() {
        let mut world = World::new(800.0, 600.0);
        let s = window(23, 1, 100.0, 300.0, 400.0);
        world.sync_surfaces(&[s], true);
        for _ in 0..80 {
            world.piles[0].deposit(200.0, 2.5);
        }
        world.interact(Some((200.0, 295.0)), 30.0, 1.0 / 60.0);
        world.interact(Some((280.0, 295.0)), 30.0, 1.0 / 60.0);
        assert!(
            !world.loose.is_empty(),
            "um movimento rápido devia projetar partículas"
        );
    }
}
