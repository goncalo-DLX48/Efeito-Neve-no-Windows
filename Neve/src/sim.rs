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

/// Largura (px físicos) de cada célula de acumulação. Células finas permitem
/// que a silhueta acompanhe irregularidades pequenas; o renderer suaviza-as
/// numa única curva contínua.
const CELL_W: f32 = 4.0;
/// Número máximo de células por monte (ecrãs 8K continuam cobertos).
const MAX_CELLS: usize = 2048;
/// Altura máxima (px) de um monte sobre uma janela.
const MAX_PILE_H: f32 = 28.0;
/// Derretimento passivo (px/s) por célula. Muito lento: a neve acumulada
/// persiste e forma camadas, em vez de desaparecer ao ritmo da deposição.
const MELT_RATE: f32 = 0.05;
/// Derretimento acelerado (px/s) quando o utilizador escolhe "Melt Snow".
const FAST_MELT_RATE: f32 = 11.0;
/// Teto absoluto de partículas, independentemente da resolução.
pub const MAX_PARTICLES: usize = 3000;

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
    /// Retângulo da superfície (apenas o topo e a largura são usados).
    pub rect: Rect,
    /// Altura de neve por célula, em px.
    pub cells: Vec<f32>,
    /// Buffer auxiliar para a difusão (evita alocar por frame).
    scratch: Vec<f32>,
    pub cell_w: f32,
    /// Altura máxima admissível (a barra de tarefas acumula um pouco mais).
    max_h: f32,
    /// Ordem Z da superfície de origem.
    pub z: i32,
    state: PileState,
    pub slide_y: f32,
    slide_v: f32,
    fade: f32,
    melting: bool,
}

impl Pile {
    fn new(s: Surface) -> Self {
        let n = ((s.rect.w / CELL_W).ceil() as usize).clamp(1, MAX_CELLS);
        // Em ecrãs muito largos o limite de células é atingido; nesse caso a
        // largura da célula cresce para que a última coincida com a borda.
        let cell_w = s.rect.w / n as f32;
        Self {
            id: s.id,
            rect: s.rect,
            cells: vec![0.0; n],
            scratch: vec![0.0; n],
            cell_w,
            max_h: if s.kind == SurfaceKind::Taskbar {
                32.0
            } else {
                MAX_PILE_H
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

    /// Deposição de um floco de raio `r` na abcissa `x`.
    ///
    /// O volume é distribuído por uma gaussiana estreita (em vez de uma coluna
    /// única), o que produz grumos arredondados em vez de torres alinhadas à
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
        for i in lo..=hi {
            let d = (i as f32 + 0.5 - center) / sigma;
            let w = (-0.5 * d * d).exp() / wsum;
            let idx = i as usize;
            self.cells[idx] = (self.cells[idx] + amount * w).min(self.max_h);
        }
    }

    /// Difusão + deslizamento (ângulo de repouso). A difusão alisa o monte ao
    /// longo de ~1 s; o deslizamento move neve de cristas demasiado íngremes
    /// para as células vizinhas, criando encostas naturais em vez de degraus.
    fn relax(&mut self, dt: f32) {
        let n = self.cells.len();
        if n < 3 {
            return;
        }
        let k = (dt * 2.4).min(0.28);
        for i in 0..n {
            let l = self.cells[i.saturating_sub(1)];
            let r = self.cells[(i + 1).min(n - 1)];
            self.scratch[i] = self.cells[i] + k * (0.5 * (l + r) - self.cells[i]);
        }
        std::mem::swap(&mut self.cells, &mut self.scratch);

        // Inclinação máxima admissível (px por célula). Com células de 4 px,
        // 2,2 px correspondem a ~29°, próximo do ângulo de repouso da neve.
        let max_step = 2.2;
        let slip_k = (dt * 5.0).min(0.5) * 0.5;
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
            PileState::Settled => 0.93,
            PileState::Sliding => self.fade * 0.93,
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

pub struct World {
    pub width: f32,
    pub height: f32,
    pub time: f32,
    pub particles: Vec<Particle>,
    pub piles: Vec<Pile>,
    spawn_acc: f32,
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
            spawn_acc: 0.0,
            rng: 0x9E37_79B9,
        }
    }

    /// Muda o tamanho do "mundo" (resolução/ecrãs alterados).
    pub fn resize(&mut self, width: f32, height: f32) {
        self.width = width;
        self.height = height;
        self.particles
            .retain(|p| p.x >= -40.0 && p.x <= width + 40.0 && p.y < height + 40.0);
        // Montes antigos deixam de fazer sentido: desaparecem suavemente.
        for p in &mut self.piles {
            p.detach();
        }
    }

    /// Limpa tudo (usado ao desligar a neve).
    pub fn reset(&mut self) {
        self.particles.clear();
        self.piles.clear();
        self.spawn_acc = 0.0;
        self.time = 0.0;
    }

    /// Força o derretimento de toda a neve acumulada (ação "Melt Snow").
    pub fn melt_all(&mut self) {
        for p in &mut self.piles {
            p.melting = true;
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
}
