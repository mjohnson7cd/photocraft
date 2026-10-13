//! Mixer Brush: a reservoir of loaded paint plus a pickup well that mixes in canvas colour.
//!
//! Model (behavioural, our own): the pickup well is tip-sized: each tip pixel `P` moves towards the
//! canvas colour under it by `wet`, so a wet brush drags the canvas detail along rather than laying
//! down the footprint's average. Each pixel's deposit mixes the reservoir `R` (weight
//! `amount × (1 − mix·wet)`) with its pickup (weight `mix·wet`); it lands at `flow` times the total
//! weight. The reservoir amount drains per dab unless `load` is 100 %, so low Load strokes dry out.
//! A dry brush (`wet = 0`) paints only reservoir colour; an empty, wet brush smears canvas colour.

use photocraft_raster::{Surface, from_rgba, to_rgba};
use serde::{Deserialize, Serialize};

use crate::Stroke;
use crate::retouch::{alpha_index, apply_dab_stroke, over_native};
use photocraft_geom::Rect;

/// Fraction of the way the pickup colour moves towards the canvas colour per dab at 100 % Wet.
pub const PICKUP_RATE: f32 = 0.25;

/// Mixer Brush options (all 0..1).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct MixerSettings {
    pub wet: f32,
    pub load: f32,
    pub mix: f32,
    pub flow: f32,
    /// Pick up colour from all visible layers instead of the target layer only.
    pub sample_all_layers: bool,
}

impl Default for MixerSettings {
    fn default() -> Self {
        Self { wet: 0.5, load: 0.5, mix: 0.5, flow: 1.0, sample_all_layers: false }
    }
}

/// Paint carried between strokes (session tool state).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct MixerState {
    /// Loaded paint (straight RGBA), None = clean/unloaded.
    pub reservoir: Option<[f32; 4]>,
    /// Remaining reservoir amount 0..1.
    pub amount: f32,
    /// Colour picked up from the canvas (straight RGBA), None = clean.
    pub pickup: Option<[f32; 4]>,
}

impl MixerState {
    /// Load the reservoir with a colour (full).
    pub fn load(&mut self, color: [f32; 4]) {
        self.reservoir = Some(color);
        self.amount = 1.0;
    }
    /// Clean the brush (drop the picked-up colour).
    pub fn clean(&mut self) {
        self.pickup = None;
    }
}

/// Paint a Mixer Brush stroke. `sample` is the surface colour is picked up from (the layer itself
/// when None; a flattened composite for Sample All Layers). Returns the damaged rectangle.
pub fn apply_mixer_stroke(
    target: &mut Surface,
    sample: Option<&Surface>,
    stroke: &Stroke,
    m: &MixerSettings,
    state: &mut MixerState,
    selection: Option<&Surface>,
    lock_transparency: bool,
) -> Rect {
    let fmt = target.format();
    let n = fmt.channels();
    let a = alpha_index(&fmt);
    let nc = fmt.mode.color_channels();
    let native = |c: [f32; 4]| {
        let mut v = from_rgba(&fmt, c);
        if let Some(ai) = a {
            v[ai] = 1.0;
        }
        v
    };
    let res = state.reservoir.map(native);
    let mut amount = if res.is_some() { state.amount.clamp(0.0, 1.0) } else { 0.0 };
    let (wet, mix, flow, load) = (m.wet.clamp(0.0, 1.0), m.mix.clamp(0.0, 1.0), m.flow.clamp(0.0, 1.0), m.load.clamp(0.0, 1.0));
    // Pickup well centred on the dab, one native pixel per tip pixel; `have` = it holds paint.
    // Paint carried over from the last stroke fills it evenly.
    let r = (stroke.brush.size.max(1.0) / 2.0).ceil() as i32 + 2;
    // Cap to stroke bounds: cells beyond the canvas are never read, so no need to allocate them.
    let ds = crate::dabs(stroke);
    let ctx = crate::BrushContext::new(&stroke.brush);
    let bounds = ds.iter().fold(Rect::EMPTY, |r, d| r.union(&ctx.dab_rect(d, false)));
    let rad = r.min(bounds.width().max(bounds.height()) as i32);
    let side = (2 * rad + 1) as usize;
    let (mut well, mut have) = match state.pickup.map(native) {
        Some(p) => (p.repeat(side * side), vec![true; side * side]),
        None => (vec![0.0f32; side * side * n], vec![false; side * side]),
    };
    let mut st = stroke.clone();
    st.brush.opacity = 1.0;
    let mut paint = vec![0.0f32; n];
    let dmg = apply_dab_stroke(target, &st, selection, lock_transparency, 0, |work, fp| {
        // Transfer › Wetness / Mix Jitter scale the settings per dab.
        let (wet, mix) = (wet * fp.dab.wet.clamp(0.0, 1.0), mix * fp.dab.mix.clamp(0.0, 1.0));
        let wr = if res.is_some() { amount * (1.0 - mix * wet) } else { 0.0 };
        let (cx, cy) = (fp.dab.center.x.floor() as i32, fp.dab.center.y.floor() as i32);
        let r = fp.rect.intersect(&work.rect);
        for y in r.y0..r.y1 {
            for x in r.x0..r.x1 {
                let w = fp.at(x, y);
                if w <= 0.0 {
                    continue;
                }
                let (qx, qy) = (x - cx + rad, y - cy + rad);
                let cell = (qx >= 0 && qy >= 0 && qx < side as i32 && qy < side as i32).then(|| qy as usize * side + qx as usize);
                // Pick up the canvas under this tip pixel.
                if let Some(i) = cell
                    && wet > 0.0
                {
                    let px: Vec<f32> = match sample {
                        Some(s) => {
                            let v = s.pixel(x, y);
                            if s.format() == fmt { v } else { from_rgba(&fmt, s.rgba(x, y)) }
                        }
                        None => work.px(x, y).to_vec(),
                    };
                    let al = a.map_or(1.0, |ai| px[ai]);
                    let q = &mut well[i * n..(i + 1) * n];
                    if al > 0.0 && !have[i] {
                        q[..nc].copy_from_slice(&px[..nc]);
                        if let Some(ai) = a {
                            q[ai] = 1.0;
                        }
                        have[i] = true;
                    } else if al > 0.0 {
                        let t = PICKUP_RATE * wet * al * w;
                        for c in 0..nc {
                            q[c] += (px[c] - q[c]) * t;
                        }
                    }
                }
                let wp = if cell.is_some_and(|i| have[i]) { mix * wet } else { 0.0 };
                let total = wr + wp;
                if total <= 1e-6 {
                    continue;
                }
                let q = cell.filter(|_| wp > 0.0).map(|i| &well[i * n..(i + 1) * n]);
                for c in 0..n {
                    let rv = res.as_ref().map_or(0.0, |v| v[c]);
                    let pv = q.map_or(0.0, |v| v[c]);
                    paint[c] = (rv * wr + pv * wp) / total;
                }
                over_native(&fmt, work.px_mut(x, y), &paint, w * total.min(1.0) * flow, lock_transparency);
            }
        }
        amount *= 1.0 - 0.08 * (1.0 - load);
    });
    state.amount = amount;
    // Carry the well's average colour to the next stroke.
    let held = have.iter().filter(|h| **h).count();
    state.pickup = (held > 0).then(|| {
        let mut avg = vec![0.0f32; n];
        for (cell, _) in well.chunks_exact(n).zip(&have).filter(|(_, h)| **h) {
            avg.iter_mut().zip(cell).for_each(|(s, v)| *s += v / held as f32);
        }
        to_rgba(&fmt, &avg)
    });
    dmg
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BrushSettings, StrokePoint};
    use photocraft_color::PixelFormat;

    fn stroke(y: f64) -> Stroke {
        Stroke {
            brush: BrushSettings { size: 8.0, hardness: 1.0, spacing: 0.25, pressure_size: false, ..Default::default() },
            points: vec![StrokePoint::new(4.0, y, 1.0), StrokePoint::new(196.0, y, 1.0)],
        }
    }

    #[test]
    fn low_load_runs_dry_and_full_load_does_not() {
        for (load, expect_dry) in [(0.0, true), (1.0, false)] {
            let mut s = Surface::new(PixelFormat::RGBA8);
            let mut st = MixerState::default();
            st.load([1.0, 0.0, 0.0, 1.0]);
            apply_mixer_stroke(&mut s, None, &stroke(10.0), &MixerSettings { wet: 0.0, load, mix: 0.0, flow: 1.0, ..Default::default() }, &mut st, None, false);
            let (start, end) = (s.rgba(10, 10)[3], s.rgba(190, 10)[3]);
            assert!(start > 0.95, "{start}");
            if expect_dry {
                assert!(end < start * 0.6, "load {load}: {start} → {end}");
                assert!(st.amount < 0.2);
            } else {
                assert!(end > 0.95, "{end}");
                assert!((st.amount - 1.0).abs() < 1e-6);
            }
        }
    }

    #[test]
    fn wet_brush_picks_up_and_smears_canvas_colour() {
        let mut s = Surface::new(PixelFormat::RGBA8);
        // Blue canvas on the left half, white on the right.
        s.fill_rect(Rect::new(0, 0, 100, 20), &[0.0, 0.0, 1.0, 1.0]);
        s.fill_rect(Rect::new(100, 0, 200, 20), &[1.0, 1.0, 1.0, 1.0]);
        let mut st = MixerState::default();
        st.load([1.0, 0.0, 0.0, 1.0]);
        apply_mixer_stroke(
            &mut s,
            None,
            &stroke(10.0),
            &MixerSettings { wet: 1.0, load: 0.3, mix: 0.9, flow: 1.0, ..Default::default() },
            &mut st,
            None,
            false,
        );
        // Blue is dragged into the white half.
        let c = s.rgba(115, 10);
        assert!(c[2] > c[1] + 0.05 && c[1] < 0.95, "{c:?}");
        // The pickup well now holds a canvas-contaminated colour.
        let p = st.pickup.unwrap();
        assert!(p[2] > 0.2, "{p:?}");
        st.clean();
        assert!(st.pickup.is_none());
    }

    #[test]
    fn wet_brush_smears_canvas_detail_instead_of_averaging_it() {
        // #5282: horizontal 2 px black/white stripes, stroked along the stripes with a wet, all-canvas
        // mix. The picked-up paint is the canvas under each tip pixel, so the stripes are dragged
        // along, not flattened into the footprint's average grey.
        let mut s = Surface::new(PixelFormat::RGBA8);
        for y in (0..20).step_by(4) {
            s.fill_rect(Rect::new(0, y, 200, y + 2), &[0.0, 0.0, 0.0, 1.0]);
            s.fill_rect(Rect::new(0, y + 2, 200, y + 4), &[1.0, 1.0, 1.0, 1.0]);
        }
        let mut st = MixerState::default();
        st.load([1.0, 0.0, 0.0, 1.0]);
        let mut k = stroke(10.0);
        k.brush.size = 12.0;
        apply_mixer_stroke(&mut s, None, &k, &MixerSettings { wet: 1.0, load: 1.0, mix: 1.0, flow: 1.0, ..Default::default() }, &mut st, None, false);
        let (dark, light) = (s.rgba(100, 8)[0], s.rgba(100, 10)[0]);
        assert!(light - dark > 0.5, "stripes flattened: {dark} vs {light}");
    }

    #[test]
    fn wet_brush_drags_colour_across_edges() {
        // #5282: stroke from black into white with an empty, wet brush. The black is carried into the
        // white area (proves dragging happens, not just that averaging is avoided).
        let mut s = Surface::new(PixelFormat::RGBA8);
        s.fill_rect(Rect::new(0, 0, 100, 20), &[0.0, 0.0, 0.0, 1.0]);
        s.fill_rect(Rect::new(100, 0, 200, 20), &[1.0, 1.0, 1.0, 1.0]);
        let mut st = MixerState::default();
        apply_mixer_stroke(
            &mut s,
            None,
            &stroke(10.0),
            &MixerSettings { wet: 1.0, load: 1.0, mix: 1.0, flow: 1.0, ..Default::default() },
            &mut st,
            None,
            false,
        );
        // Black dragged into the white side.
        assert!(s.rgba(110, 10)[0] < 0.3, "no dragging: {}", s.rgba(110, 10)[0]);
    }

    #[test]
    fn dry_empty_brush_paints_nothing() {
        let mut s = Surface::new(PixelFormat::RGBA8);
        let mut st = MixerState::default();
        let dmg = apply_mixer_stroke(&mut s, None, &stroke(10.0), &MixerSettings { wet: 0.0, ..Default::default() }, &mut st, None, false);
        assert!(!dmg.is_empty());
        assert_eq!(s.rgba(50, 10)[3], 0.0);
    }

    #[test]
    fn transfer_wetness_control_scales_pickup_per_dab() {
        // Wetness on Pen Pressure: no pressure means a dry brush (no pickup), full pressure picks up.
        let run = |pressure: f32| {
            let mut s = Surface::new(PixelFormat::RGBA8);
            s.fill_rect(Rect::new(0, 0, 200, 20), &[0.0, 0.0, 1.0, 1.0]);
            let mut st = MixerState::default();
            st.load([1.0, 0.0, 0.0, 1.0]);
            let mut k = stroke(10.0);
            k.brush.transfer = crate::Transfer { enabled: true, wetness: crate::Dynamic::controlled(crate::Control::PenPressure), ..Default::default() };
            for p in &mut k.points {
                p.pressure = pressure;
            }
            apply_mixer_stroke(&mut s, None, &k, &MixerSettings { wet: 1.0, load: 1.0, mix: 1.0, flow: 1.0, ..Default::default() }, &mut st, None, false);
            st.pickup
        };
        assert!(run(0.0).is_none());
        assert!(run(1.0).is_some_and(|p| p[2] > 0.2));
    }
}
