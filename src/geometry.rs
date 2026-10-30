//! Geometria mínima partilhada entre a simulação e o renderer.
//!
//! Todas as coordenadas do projeto estão em **píxeis físicos** (não em DIPs):
//! o overlay é um único HWND que cobre todo o ecrã virtual e o Direct2D é
//! configurado com `D2D1_UNIT_MODE_PIXELS`, portanto 1 unidade = 1 píxel real.
//! Assim a neve tem exatamente o mesmo tamanho físico em monitores com escalas
//! de DPI diferentes.

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }

    #[inline]
    pub fn right(&self) -> f32 {
        self.x + self.w
    }

    #[inline]
    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }
}
