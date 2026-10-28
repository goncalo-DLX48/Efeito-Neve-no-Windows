//! Preferências do utilizador e parametrização da intensidade de neve.
//!
//! A densidade é expressa em partículas por megapíxel de área do ecrã virtual,
//! para que um portátil 1080p e um monitor 4K tenham a mesma densidade visual
//! (e não a mesma contagem absoluta, o que tornaria a neve impercetível em 4K).

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Intensity {
    Off,
    Light,
    Steady,
    Heavy,
    Variable,
}

impl Intensity {
    pub const ALL: [Intensity; 5] = [
        Intensity::Off,
        Intensity::Light,
        Intensity::Steady,
        Intensity::Heavy,
        Intensity::Variable,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Intensity::Off => "Off",
            Intensity::Light => "Light",
            Intensity::Steady => "Steady",
            Intensity::Heavy => "Heavy",
            Intensity::Variable => "Variable",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|i| i.label().eq_ignore_ascii_case(s))
    }

    pub fn is_off(self) -> bool {
        matches!(self, Intensity::Off)
    }

    /// Partículas-alvo por megapíxel de ecrã virtual.
    fn density_per_mpx(self) -> f32 {
        match self {
            Intensity::Off => 0.0,
            Intensity::Light => 58.0,
            Intensity::Steady => 148.0,
            Intensity::Heavy => 340.0,
            Intensity::Variable => 148.0, // base; modulada em `target_particles`
        }
    }
}

#[derive(Clone, Debug)]
pub struct Settings {
    pub intensity: Intensity,
    /// Acumulação de neve sobre janelas / barra de tarefas.
    pub accumulate: bool,
    /// A neve pode pousar sobre janelas (a barra de tarefas é sempre tratada).
    pub snow_on_windows: bool,
    /// Desenhar o overlay atrás das janelas abertas (estilo papel de parede).
    pub behind_windows: bool,
    /// Estado espelhado da chave de arranque automático (só para o menu).
    pub autostart: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            intensity: Intensity::Steady,
            accumulate: true,
            snow_on_windows: true,
            behind_windows: false,
            autostart: false,
        }
    }
}

impl Settings {
    /// Número-alvo de partículas em função da área e do tempo.
    ///
    /// `Variable` alterna lentamente entre a densidade `Light` e `Heavy` com um
    /// período de ~45 s, usando uma sinusoide suavizada (nunca há saltos na
    /// taxa de emissão).
    pub fn target_particles(&self, area_mpx: f32, time: f32) -> usize {
        let base = match self.intensity {
            Intensity::Variable => {
                let wave = 0.5 + 0.5 * (time * 0.14).sin(); // 0..1, período ~45 s
                let light = Intensity::Light.density_per_mpx();
                let heavy = Intensity::Heavy.density_per_mpx();
                light + (heavy - light) * wave
            }
            other => other.density_per_mpx(),
        };
        (base * area_mpx).round().max(0.0) as usize
    }

    /// Taxa de emissão (partículas/segundo).
    ///
    /// Repõe ~28% do alvo por segundo: o campo enche em ~4 s e mantém-se estável
    /// quando as partículas pousam ou saem do ecrã.
    pub fn spawn_rate(&self, area_mpx: f32, time: f32) -> f32 {
        let target = self.target_particles(area_mpx, time) as f32;
        (target * 0.28).max(0.0)
    }
}
