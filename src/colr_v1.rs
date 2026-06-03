// SPDX-License-Identifier: MIT OR Apache-2.0

//! `COLRv1` color glyph rendering using skrifa's paint graph traversal and zeno
//! for outline rasterization.
//!
//! This implements the OpenType `COLR` version 1 paint graph:
//!
//! - Solid fills and linear / radial (two-point conical) / sweep (conical)
//!   gradients, with **premultiplied sRGB** color interpolation (premultiplied
//!   to avoid fringing across transparent stops, matching the COLR reference
//!   renderers FreeType/Skia/Chrome; the spec mandates premultiplication, not a
//!   gamma/linear color space).
//! - Gradients are evaluated in the untransformed paint coordinate space (each
//!   pixel is mapped back through the inverse of the current affine), so
//!   non-uniform scale, rotation and shear are handled correctly.
//! - The full set of `PaintComposite` modes: the 13 Porter-Duff operators plus
//!   the separable and non-separable (HSL) W3C blend modes.
//! - Anti-aliased glyph clips and transformed-quadrilateral box clips.
//!
//! Output is a swash `Content::Color` image in straight (non-premultiplied)
//! sRGB alpha, matching how cosmic-text consumes color glyphs.

#[cfg(not(feature = "std"))]
use alloc::vec::Vec;

// Float methods (powf/atan2/sqrt/floor/round/abs) are inherent under std; pull
// them in from core_maths for no_std builds.
#[cfg(not(feature = "std"))]
use core_maths::CoreFloat;

use skrifa::color::{
    Brush, ColorGlyphFormat, ColorPainter, ColorStop, CompositeMode, Extend,
    Transform as PaintTransform,
};
use skrifa::instance::{Location, Size};
use skrifa::outline::{DrawSettings, OutlinePen};
use skrifa::prelude::*;
use skrifa::raw::TableProvider;
use swash::scale::image::{Content, Image as SwashImage};
use swash::scale::Source as ImageSource;
use swash::zeno::{self, Command, Format, Mask, Placement};

use crate::{CacheKey, Color};

/// Special CPAL palette index meaning "use the foreground (text) color".
const FOREGROUND_COLOR_INDEX: u16 = 0xFFFF;

/// Number of samples in the precomputed gradient color ramp. The ramp lets us
/// resolve and interpolate stops once per fill rather than once per pixel;
/// 256 entries is below 8-bit output banding.
const RAMP_LEN: usize = 256;

/// Largest color-glyph buffer dimension we will allocate (sanity guard).
const MAX_DIM: u32 = 8192;

// ─── Color helpers ─────────────────────────────────────────────────────────

/// A straight (non-premultiplied) sRGB color.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct Rgba {
    r: u8,
    g: u8,
    b: u8,
    a: u8,
}

impl Rgba {
    const BLACK: Self = Self {
        r: 0,
        g: 0,
        b: 0,
        a: 255,
    };
    const TRANSPARENT: Self = Self {
        r: 0,
        g: 0,
        b: 0,
        a: 0,
    };

    fn from_color(c: Color) -> Self {
        Self {
            r: c.r(),
            g: c.g(),
            b: c.b(),
            a: c.a(),
        }
    }

    /// Multiply the alpha channel by `alpha` (0..1), keeping straight color.
    fn with_alpha(self, alpha: f32) -> Self {
        Self {
            a: (f32::from(self.a) * alpha).round().clamp(0.0, 255.0) as u8,
            ..self
        }
    }
}

/// Multiply two bytes as fractional values (0..255 → 0.0..1.0).
fn mul_u8(a: u8, b: u8) -> u8 {
    ((u16::from(a) * u16::from(b) + 128) / 255) as u8
}

/// `SrcOver` compositing of a single premultiplied pixel onto a premultiplied
/// destination. `r`, `g`, `b`, `a` must already be premultiplied.
fn src_over(dst: &mut [u8], r: u8, g: u8, b: u8, a: u8) {
    if a == 0 {
        return;
    }
    if a == 255 {
        dst[0] = r;
        dst[1] = g;
        dst[2] = b;
        dst[3] = 255;
        return;
    }
    let inv_a = 255 - u16::from(a);
    dst[0] = (u16::from(r) + (inv_a * u16::from(dst[0]) + 128) / 255) as u8;
    dst[1] = (u16::from(g) + (inv_a * u16::from(dst[1]) + 128) / 255) as u8;
    dst[2] = (u16::from(b) + (inv_a * u16::from(dst[2]) + 128) / 255) as u8;
    dst[3] = (u16::from(a) + (inv_a * u16::from(dst[3]) + 128) / 255) as u8;
}

/// Convert an RGBA buffer from premultiplied alpha to straight (non-premultiplied)
/// alpha in place. Fully transparent pixels are zeroed.
fn unpremultiply_rgba(buffer: &mut [u8]) {
    for px in buffer.chunks_exact_mut(4) {
        let a = px[3];
        if a == 0 {
            px[0] = 0;
            px[1] = 0;
            px[2] = 0;
        } else if a != 255 {
            let a = u16::from(a);
            px[0] = (((u16::from(px[0]) * 255) + a / 2) / a).min(255) as u8;
            px[1] = (((u16::from(px[1]) * 255) + a / 2) / a).min(255) as u8;
            px[2] = (((u16::from(px[2]) * 255) + a / 2) / a).min(255) as u8;
        }
    }
}

// ─── Affine transform helpers ──────────────────────────────────────────────

/// Convert a skrifa `PaintTransform` to a `zeno::Transform`.
fn to_zeno_transform(t: &PaintTransform) -> zeno::Transform {
    zeno::Transform {
        xx: t.xx,
        yx: t.yx,
        xy: t.xy,
        yy: t.yy,
        x: t.dx,
        y: t.dy,
    }
}

/// Transform a 2D point through a `PaintTransform`
/// (`x' = xx*x + xy*y + dx`, `y' = yx*x + yy*y + dy`).
fn transform_point(t: &PaintTransform, x: f32, y: f32) -> (f32, f32) {
    (t.xx * x + t.xy * y + t.dx, t.yx * x + t.yy * y + t.dy)
}

/// Invert an affine transform. Returns `None` if it is singular (degenerate).
fn invert_transform(t: &PaintTransform) -> Option<PaintTransform> {
    let det = t.xx * t.yy - t.xy * t.yx;
    if det.abs() < 1e-12 {
        return None;
    }
    let inv_det = 1.0 / det;
    Some(PaintTransform {
        xx: t.yy * inv_det,
        yx: -t.yx * inv_det,
        xy: -t.xy * inv_det,
        yy: t.xx * inv_det,
        dx: (t.xy * t.dy - t.yy * t.dx) * inv_det,
        dy: (t.yx * t.dx - t.xx * t.dy) * inv_det,
    })
}

// ─── Gradient color line ───────────────────────────────────────────────────

/// A gradient color stop resolved to premultiplied sRGB RGB plus straight alpha,
/// ready for interpolation.
#[derive(Copy, Clone, Debug)]
struct ResolvedStop {
    offset: f32,
    /// sRGB RGB premultiplied by `a`.
    lr: f32,
    lg: f32,
    lb: f32,
    /// Straight alpha (0..1), combining CPAL alpha and the per-stop alpha.
    a: f32,
}

/// Resolve a palette/foreground color for a given index into straight sRGB.
fn resolve_base_color(palette_index: u16, palette: &[Rgba], foreground: Rgba) -> Rgba {
    if palette_index == FOREGROUND_COLOR_INDEX {
        foreground
    } else if (palette_index as usize) < palette.len() {
        palette[palette_index as usize]
    } else {
        Rgba::BLACK
    }
}

/// Resolve a solid color stop / fill to a straight sRGB color with combined alpha.
fn resolve_color(palette_index: u16, alpha: f32, palette: &[Rgba], foreground: Rgba) -> Rgba {
    resolve_base_color(palette_index, palette, foreground).with_alpha(alpha)
}

/// Resolve the gradient color line to sorted [`ResolvedStop`]s in premultiplied
/// sRGB. skrifa already sorts and normalizes stop offsets to `[0, 1]`, but we
/// sort defensively.
fn resolve_color_line(
    stops: &[ColorStop],
    palette: &[Rgba],
    foreground: Rgba,
) -> Vec<ResolvedStop> {
    let mut out: Vec<ResolvedStop> = stops
        .iter()
        .map(|s| {
            let base = resolve_base_color(s.palette_index, palette, foreground);
            // Combined alpha = CPAL alpha * per-stop alpha.
            let a = (f32::from(base.a) / 255.0) * s.alpha;
            // Premultiplied sRGB (gamma-encoded) channels. Interpolating in
            // premultiplied space avoids dark fringing across transparent stops;
            // staying in sRGB (rather than linear-light) matches the COLR
            // reference renderers (FreeType/Skia/Chrome) and is consistent with
            // the sRGB compositing elsewhere in this file. The spec mandates
            // premultiplication, not a particular gamma/linear color space.
            let pr = (f32::from(base.r) / 255.0) * a;
            let pg = (f32::from(base.g) / 255.0) * a;
            let pb = (f32::from(base.b) / 255.0) * a;
            ResolvedStop {
                offset: s.offset,
                lr: pr,
                lg: pg,
                lb: pb,
                a,
            }
        })
        .collect();
    out.sort_by(|a, b| {
        a.offset
            .partial_cmp(&b.offset)
            .unwrap_or(core::cmp::Ordering::Equal)
    });
    out
}

/// Sample the resolved color line at normalized parameter `t` in `[0, 1]`,
/// returning a straight sRGB color. Interpolation is in premultiplied sRGB
/// (matching the COLR reference renderers), then un-premultiplied.
fn sample_color_line(stops: &[ResolvedStop], t: f32) -> Rgba {
    if stops.is_empty() {
        return Rgba::TRANSPARENT;
    }
    let first = stops[0];
    let last = stops[stops.len() - 1];

    let (lr, lg, lb, a) = if t <= first.offset {
        (first.lr, first.lg, first.lb, first.a)
    } else if t >= last.offset {
        (last.lr, last.lg, last.lb, last.a)
    } else {
        let mut seg = (first, last);
        for w in stops.windows(2) {
            if t >= w[0].offset && t <= w[1].offset {
                seg = (w[0], w[1]);
                break;
            }
        }
        let (s0, s1) = seg;
        let span = s1.offset - s0.offset;
        // Duplicate offsets (span ~ 0) form a hard edge: take the upper stop.
        let lt = if span > 1e-9 {
            (t - s0.offset) / span
        } else {
            1.0
        };
        let inv = 1.0 - lt;
        (
            s0.lr * inv + s1.lr * lt,
            s0.lg * inv + s1.lg * lt,
            s0.lb * inv + s1.lb * lt,
            s0.a * inv + s1.a * lt,
        )
    };

    if a <= 0.0 {
        return Rgba::TRANSPARENT;
    }
    // Un-premultiply back to straight sRGB (channels are already sRGB-encoded).
    let to8 = |srgb_premul: f32| {
        let straight = (srgb_premul / a).clamp(0.0, 1.0);
        (straight * 255.0).round().clamp(0.0, 255.0) as u8
    };
    Rgba {
        r: to8(lr),
        g: to8(lg),
        b: to8(lb),
        a: (a * 255.0).round().clamp(0.0, 255.0) as u8,
    }
}

/// Precompute a `RAMP_LEN`-entry straight-sRGB color ramp from the resolved
/// color line so per-pixel evaluation is an O(1) table lookup.
fn build_ramp(stops: &[ResolvedStop]) -> Vec<Rgba> {
    (0..RAMP_LEN)
        .map(|i| sample_color_line(stops, i as f32 / (RAMP_LEN as f32 - 1.0)))
        .collect()
}

/// Look up the ramp at normalized `t` in `[0, 1]`.
fn ramp_lookup(ramp: &[Rgba], t: f32) -> Rgba {
    let idx = (t.clamp(0.0, 1.0) * (ramp.len() as f32 - 1.0)).round() as usize;
    ramp[idx.min(ramp.len() - 1)]
}

/// Apply a gradient extend mode to parameter `t`, folding it into `[0, 1]`.
fn apply_extend(t: f32, extend: Extend) -> f32 {
    match extend {
        Extend::Repeat => t - t.floor(),
        Extend::Reflect => {
            let u = t - 2.0 * (t * 0.5).floor();
            if u > 1.0 {
                2.0 - u
            } else {
                u
            }
        }
        // Pad (and any unknown future mode).
        _ => t.clamp(0.0, 1.0),
    }
}

/// Solve the two-point conical (radial) gradient parameter at point `p`.
///
/// Returns the largest `t` such that `p` lies on the circle with center
/// `lerp(c0, c1, t)` and radius `r(t) = lerp(r0, r1, t) >= 0`, per the COLR /
/// Canvas / Skia semantics. Returns `None` when the pixel is outside the cone
/// (no valid root) — such pixels must not be painted.
fn radial_t(c0: (f32, f32), r0: f32, c1: (f32, f32), r1: f32, p: (f32, f32)) -> Option<f32> {
    let dcx = c1.0 - c0.0;
    let dcy = c1.1 - c0.1;
    let dr = r1 - r0;
    let px = p.0 - c0.0;
    let py = p.1 - c0.1;

    let a = dcx * dcx + dcy * dcy - dr * dr;
    let b = -2.0 * (px * dcx + py * dcy + r0 * dr);
    let c = px * px + py * py - r0 * r0;

    let r_ok = |t: f32| (r0 + dr * t) >= 0.0;

    if a.abs() < 1e-9 {
        // Degenerate (cylinder/tangent) → linear in t.
        if b.abs() < 1e-9 {
            return None;
        }
        let t = -c / b;
        return if r_ok(t) { Some(t) } else { None };
    }

    let disc = b * b - 4.0 * a * c;
    if disc < 0.0 {
        return None;
    }
    let sq = disc.sqrt();
    let t0 = (-b + sq) / (2.0 * a);
    let t1 = (-b - sq) / (2.0 * a);
    let (hi, lo) = if t0 >= t1 { (t0, t1) } else { (t1, t0) };
    if r_ok(hi) {
        Some(hi)
    } else if r_ok(lo) {
        Some(lo)
    } else {
        None
    }
}

// ─── Compositing (PaintComposite blend / Porter-Duff modes) ────────────────

#[inline]
fn clamp01(x: f32) -> f32 {
    x.clamp(0.0, 1.0)
}

// Separable blend functions B(Cb, Cs) on straight color channels.
#[inline]
fn b_multiply(cb: f32, cs: f32) -> f32 {
    cb * cs
}
#[inline]
fn b_screen(cb: f32, cs: f32) -> f32 {
    cb + cs - cb * cs
}
#[inline]
fn b_hardlight(cb: f32, cs: f32) -> f32 {
    if cs <= 0.5 {
        2.0 * cb * cs
    } else {
        1.0 - 2.0 * (1.0 - cb) * (1.0 - cs)
    }
}
#[inline]
fn b_overlay(cb: f32, cs: f32) -> f32 {
    b_hardlight(cs, cb)
}
#[inline]
fn b_colordodge(cb: f32, cs: f32) -> f32 {
    if cb == 0.0 {
        0.0
    } else if cs >= 1.0 {
        1.0
    } else {
        (cb / (1.0 - cs)).min(1.0)
    }
}
#[inline]
fn b_colorburn(cb: f32, cs: f32) -> f32 {
    if cb >= 1.0 {
        1.0
    } else if cs == 0.0 {
        0.0
    } else {
        1.0 - ((1.0 - cb) / cs).min(1.0)
    }
}
#[inline]
fn b_softlight(cb: f32, cs: f32) -> f32 {
    let d = if cb <= 0.25 {
        ((16.0 * cb - 12.0) * cb + 4.0) * cb
    } else {
        cb.sqrt()
    };
    if cs <= 0.5 {
        cb - (1.0 - 2.0 * cs) * cb * (1.0 - cb)
    } else {
        cb + (2.0 * cs - 1.0) * (d - cb)
    }
}

// Non-separable HSL helpers operating on the straight RGB triple.
#[inline]
fn lum(c: [f32; 3]) -> f32 {
    0.3 * c[0] + 0.59 * c[1] + 0.11 * c[2]
}
fn clip_color(mut c: [f32; 3]) -> [f32; 3] {
    let l = lum(c);
    let n = c[0].min(c[1]).min(c[2]);
    let x = c[0].max(c[1]).max(c[2]);
    if n < 0.0 && (l - n).abs() > 1e-9 {
        for v in &mut c {
            *v = l + (*v - l) * l / (l - n);
        }
    }
    if x > 1.0 && (x - l).abs() > 1e-9 {
        for v in &mut c {
            *v = l + (*v - l) * (1.0 - l) / (x - l);
        }
    }
    c
}
fn set_lum(mut c: [f32; 3], l: f32) -> [f32; 3] {
    let d = l - lum(c);
    for v in &mut c {
        *v += d;
    }
    clip_color(c)
}
#[inline]
fn sat(c: [f32; 3]) -> f32 {
    c[0].max(c[1]).max(c[2]) - c[0].min(c[1]).min(c[2])
}
fn set_sat(c: [f32; 3], s: f32) -> [f32; 3] {
    let mut idx = [0usize, 1, 2];
    idx.sort_by(|&i, &j| {
        c[i].partial_cmp(&c[j])
            .unwrap_or(core::cmp::Ordering::Equal)
    });
    let (imin, imid, imax) = (idx[0], idx[1], idx[2]);
    let mut out = [0.0f32; 3];
    if c[imax] > c[imin] {
        out[imid] = (c[imid] - c[imin]) * s / (c[imax] - c[imin]);
        out[imax] = s;
    }
    out[imin] = 0.0;
    out
}

/// Evaluate the blend color `B(Cb, Cs)` for a blend-mode composite. Returns the
/// straight RGB triple; `cb`/`cs` are straight backdrop/source colors.
fn blend_color(cb: [f32; 3], cs: [f32; 3], mode: CompositeMode) -> [f32; 3] {
    use CompositeMode::*;
    let sep = |f: fn(f32, f32) -> f32| [f(cb[0], cs[0]), f(cb[1], cs[1]), f(cb[2], cs[2])];
    match mode {
        Multiply => sep(b_multiply),
        Screen => sep(b_screen),
        Overlay => sep(b_overlay),
        Darken => sep(|a, b| a.min(b)),
        Lighten => sep(|a, b| a.max(b)),
        ColorDodge => sep(b_colordodge),
        ColorBurn => sep(b_colorburn),
        HardLight => sep(b_hardlight),
        SoftLight => sep(b_softlight),
        Difference => sep(|a, b| (a - b).abs()),
        Exclusion => sep(|a, b| a + b - 2.0 * a * b),
        HslHue => set_lum(set_sat(cs, sat(cb)), lum(cb)),
        HslSaturation => set_lum(set_sat(cb, sat(cs)), lum(cb)),
        HslColor => set_lum(cs, lum(cb)),
        HslLuminosity => set_lum(cb, lum(cs)),
        // Not a blend mode → identity (the SrcOver source color).
        _ => cs,
    }
}

/// Composite straight-alpha `src` over straight-alpha `dst` using `mode`.
fn composite_pixel(src: [f32; 4], dst: [f32; 4], mode: CompositeMode) -> [f32; 4] {
    use CompositeMode::*;
    let cs = [src[0], src[1], src[2]];
    let ass = src[3];
    let cb = [dst[0], dst[1], dst[2]];
    let ab = dst[3];

    // Porter-Duff: co = as*Fa*Cs + ab*Fb*Cb (premultiplied), ao = as*Fa + ab*Fb.
    let pd = |fa: f32, fb: f32| -> [f32; 4] {
        let ao = ass * fa + ab * fb;
        if ao <= 0.0 {
            return [0.0; 4];
        }
        let mk = |i: usize| (ass * fa * cs[i] + ab * fb * cb[i]) / ao;
        [clamp01(mk(0)), clamp01(mk(1)), clamp01(mk(2)), clamp01(ao)]
    };

    match mode {
        Clear => [0.0; 4],
        Src => src,
        Dest => dst,
        SrcOver => pd(1.0, 1.0 - ass),
        DestOver => pd(1.0 - ab, 1.0),
        SrcIn => pd(ab, 0.0),
        DestIn => pd(0.0, ass),
        SrcOut => pd(1.0 - ab, 0.0),
        DestOut => pd(0.0, 1.0 - ass),
        SrcAtop => pd(ab, 1.0 - ass),
        DestAtop => pd(1.0 - ab, ass),
        Xor => pd(1.0 - ab, 1.0 - ass),
        Plus => {
            let ao = clamp01(ass + ab);
            if ao <= 0.0 {
                return [0.0; 4];
            }
            let mk = |i: usize| clamp01(ass * cs[i] + ab * cb[i]) / ao;
            [clamp01(mk(0)), clamp01(mk(1)), clamp01(mk(2)), ao]
        }
        // Separable + non-separable blend modes: mix the source color with a
        // backdrop-influenced color, then SrcOver-composite.
        _ => {
            let bc = blend_color(cb, cs, mode);
            let cs_mixed = [
                (1.0 - ab) * cs[0] + ab * bc[0],
                (1.0 - ab) * cs[1] + ab * bc[1],
                (1.0 - ab) * cs[2] + ab * bc[2],
            ];
            let ao = ass + ab * (1.0 - ass);
            if ao <= 0.0 {
                return [0.0; 4];
            }
            let mk = |i: usize| (ass * cs_mixed[i] + ab * (1.0 - ass) * cb[i]) / ao;
            [clamp01(mk(0)), clamp01(mk(1)), clamp01(mk(2)), clamp01(ao)]
        }
    }
}

/// Composite premultiplied-sRGB `src` over premultiplied-sRGB `backdrop` using
/// `mode`, writing the result back into `src`.
fn composite_buffers(src: &mut [u8], backdrop: &[u8], mode: CompositeMode) {
    // SrcOver fast path on premultiplied bytes (the overwhelmingly common case).
    if matches!(mode, CompositeMode::SrcOver) {
        for i in (0..src.len()).step_by(4) {
            let sa = u16::from(src[i + 3]);
            if sa == 0 {
                src[i..i + 4].copy_from_slice(&backdrop[i..i + 4]);
            } else if sa < 255 {
                let inv = 255 - sa;
                for k in 0..4 {
                    src[i + k] = (u16::from(src[i + k])
                        + (inv * u16::from(backdrop[i + k]) + 128) / 255)
                        as u8;
                }
            }
        }
        return;
    }

    let to_straight = |px: &[u8]| -> [f32; 4] {
        let a = f32::from(px[3]) / 255.0;
        if a <= 0.0 {
            [0.0; 4]
        } else {
            [
                f32::from(px[0]) / 255.0 / a,
                f32::from(px[1]) / 255.0 / a,
                f32::from(px[2]) / 255.0 / a,
                a,
            ]
        }
    };

    for i in (0..src.len()).step_by(4) {
        let s = to_straight(&src[i..i + 4]);
        let d = to_straight(&backdrop[i..i + 4]);
        let o = composite_pixel(s, d, mode);
        let a = o[3];
        src[i] = (o[0] * a * 255.0).round().clamp(0.0, 255.0) as u8;
        src[i + 1] = (o[1] * a * 255.0).round().clamp(0.0, 255.0) as u8;
        src[i + 2] = (o[2] * a * 255.0).round().clamp(0.0, 255.0) as u8;
        src[i + 3] = (a * 255.0).round().clamp(0.0, 255.0) as u8;
    }
}

// ─── Outline → path collection ─────────────────────────────────────────────

/// Collect skrifa outline pen commands into zeno path commands.
struct PathCollector {
    commands: Vec<Command>,
}

impl PathCollector {
    fn new() -> Self {
        Self {
            commands: Vec::new(),
        }
    }
}

impl OutlinePen for PathCollector {
    fn move_to(&mut self, x: f32, y: f32) {
        self.commands.push(Command::MoveTo(zeno::Vector::new(x, y)));
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.commands.push(Command::LineTo(zeno::Vector::new(x, y)));
    }
    fn quad_to(&mut self, cx0: f32, cy0: f32, x: f32, y: f32) {
        self.commands.push(Command::QuadTo(
            zeno::Vector::new(cx0, cy0),
            zeno::Vector::new(x, y),
        ));
    }
    fn curve_to(&mut self, cx0: f32, cy0: f32, cx1: f32, cy1: f32, x: f32, y: f32) {
        self.commands.push(Command::CurveTo(
            zeno::Vector::new(cx0, cy0),
            zeno::Vector::new(cx1, cy1),
            zeno::Vector::new(x, y),
        ));
    }
    fn close(&mut self) {
        self.commands.push(Command::Close);
    }
}

/// Read CPAL palette colors from the font for the given palette index.
fn read_palette(font: &skrifa::FontRef<'_>, palette_index: u16) -> Vec<Rgba> {
    let cpal = match font.cpal() {
        Ok(cpal) => cpal,
        Err(_) => return Vec::new(),
    };
    let records = match cpal.color_records_array() {
        Some(Ok(records)) => records,
        _ => return Vec::new(),
    };
    let num_entries = cpal.num_palette_entries() as usize;
    let start = cpal
        .color_record_indices()
        .get(palette_index as usize)
        .map(|idx| idx.get() as usize)
        .unwrap_or(0);

    (0..num_entries)
        .map(|i| {
            let idx = start + i;
            if idx < records.len() {
                let r = records[idx];
                Rgba {
                    r: r.red(),
                    g: r.green(),
                    b: r.blue(),
                    a: r.alpha(),
                }
            } else {
                Rgba::BLACK
            }
        })
        .collect()
}

// ─── Painter ───────────────────────────────────────────────────────────────

/// `COLRv1` painter that renders the paint graph to a premultiplied sRGB buffer.
struct ColrV1Painter<'a> {
    font: skrifa::FontRef<'a>,
    location: Location,
    palette: Vec<Rgba>,
    /// Resolved foreground (text) color for palette index 0xFFFF.
    foreground: Rgba,
    /// Output buffer (premultiplied sRGB, width × height × 4 bytes).
    buffer: Vec<u8>,
    width: u32,
    height: u32,
    /// Transform stack; bottom element maps font units → pixel coordinates.
    transform_stack: Vec<PaintTransform>,
    /// Clip mask stack. Each entry is an alpha coverage mask (width × height).
    clip_stack: Vec<Vec<u8>>,
    /// Layer stack for compositing: (saved backdrop buffer, composite mode).
    layer_stack: Vec<(Vec<u8>, CompositeMode)>,
}

impl<'a> ColrV1Painter<'a> {
    fn new(
        font: skrifa::FontRef<'a>,
        location: Location,
        palette: Vec<Rgba>,
        foreground: Rgba,
        width: u32,
        height: u32,
        base_transform: PaintTransform,
    ) -> Self {
        Self {
            font,
            location,
            palette,
            foreground,
            buffer: vec![0u8; (width * height * 4) as usize],
            width,
            height,
            transform_stack: vec![base_transform],
            clip_stack: Vec::new(),
            layer_stack: Vec::new(),
        }
    }

    fn current_transform(&self) -> PaintTransform {
        self.transform_stack
            .last()
            .copied()
            .unwrap_or(PaintTransform::default())
    }

    /// Rasterize an alpha mask from the given zeno path commands (already in
    /// pixel space) and blit it into a full-size coverage mask.
    fn rasterize_mask(&self, commands: &[Command], transform: Option<zeno::Transform>) -> Vec<u8> {
        let mut mask = Mask::new(commands);
        mask.format(Format::Alpha);
        if let Some(t) = transform {
            mask.transform(Some(t));
        }
        let (data, placement) = mask.render();
        self.blit_mask_to_full(&placement, &data)
    }

    /// Copy a placed alpha mask into a full-size (width×height) coverage buffer.
    fn blit_mask_to_full(&self, placement: &Placement, mask_data: &[u8]) -> Vec<u8> {
        let mut full = vec![0u8; (self.width * self.height) as usize];
        let pw = placement.width as i32;
        let ph = placement.height as i32;
        // zeno renders with Origin::TopLeft, so `placement.top` is the path's
        // minimum (top) y in the Y-down pixel buffer and rows run top-to-bottom
        // — symmetric to `placement.left`. (Do NOT negate it; that flips partial
        // clip masks off-buffer. The final swash image placement uses swash's
        // own positive-up convention separately, in `into_image`.)
        let px = placement.left;
        let py = placement.top;

        for row in 0..ph {
            let dst_y = py + row;
            if dst_y < 0 || dst_y >= self.height as i32 {
                continue;
            }
            for col in 0..pw {
                let dst_x = px + col;
                if dst_x < 0 || dst_x >= self.width as i32 {
                    continue;
                }
                let src_idx = (row * pw + col) as usize;
                let dst_idx = (dst_y * self.width as i32 + dst_x) as usize;
                if src_idx < mask_data.len() && dst_idx < full.len() {
                    full[dst_idx] = mask_data[src_idx];
                }
            }
        }
        full
    }

    /// Rasterize a glyph outline (under the current transform) as a coverage mask.
    fn rasterize_glyph_mask(&self, glyph_id: GlyphId) -> Option<Vec<u8>> {
        let outlines = self.font.outline_glyphs();
        let outline_glyph = outlines.get(glyph_id)?;
        let mut collector = PathCollector::new();
        outline_glyph
            .draw(
                DrawSettings::unhinted(Size::unscaled(), &self.location),
                &mut collector,
            )
            .ok()?;
        if collector.commands.is_empty() {
            return None;
        }
        let zt = to_zeno_transform(&self.current_transform());
        Some(self.rasterize_mask(&collector.commands, Some(zt)))
    }

    /// Effective clip coverage at a pixel index (product of the clip stack).
    fn clip_alpha_at(&self, idx: usize) -> u8 {
        let mut alpha = 255u8;
        for clip in &self.clip_stack {
            alpha = mul_u8(alpha, clip[idx]);
            if alpha == 0 {
                return 0;
            }
        }
        alpha
    }

    /// Write one straight-sRGB color, premultiplied by the per-pixel coverage,
    /// over the buffer at `idx`.
    fn blend_pixel(&mut self, idx: usize, color: Rgba, clip_alpha: u8) {
        let a = mul_u8(color.a, clip_alpha);
        if a == 0 {
            return;
        }
        let r = mul_u8(color.r, a);
        let g = mul_u8(color.g, a);
        let b = mul_u8(color.b, a);
        let p = idx * 4;
        src_over(&mut self.buffer[p..p + 4], r, g, b, a);
    }

    /// Fill the current clip region with a solid straight-sRGB color.
    fn fill_solid(&mut self, color: Rgba) {
        if color.a == 0 {
            return;
        }
        let (w, h) = (self.width, self.height);
        for y in 0..h {
            for x in 0..w {
                let idx = (y * w + x) as usize;
                let ca = self.clip_alpha_at(idx);
                if ca == 0 {
                    continue;
                }
                self.blend_pixel(idx, color, ca);
            }
        }
    }

    /// Build the inverse of the current transform (pixel → paint space).
    fn inverse_or_skip(&self) -> Option<PaintTransform> {
        invert_transform(&self.current_transform())
    }

    fn fill_linear_gradient(
        &mut self,
        p0: (f32, f32),
        p1: (f32, f32),
        color_stops: &[ColorStop],
        extend: Extend,
    ) {
        if color_stops.is_empty() {
            return;
        }
        if color_stops.len() == 1 {
            let c = resolve_color(
                color_stops[0].palette_index,
                color_stops[0].alpha,
                &self.palette,
                self.foreground,
            );
            self.fill_solid(c);
            return;
        }

        let dx = p1.0 - p0.0;
        let dy = p1.1 - p0.1;
        let len_sq = dx * dx + dy * dy;
        if len_sq < 1e-12 {
            // Degenerate gradient line: paint nothing (per spec).
            return;
        }
        let Some(inv) = self.inverse_or_skip() else {
            return;
        };
        let ramp = build_ramp(&resolve_color_line(
            color_stops,
            &self.palette,
            self.foreground,
        ));

        let (w, h) = (self.width, self.height);
        for y in 0..h {
            for x in 0..w {
                let idx = (y * w + x) as usize;
                let ca = self.clip_alpha_at(idx);
                if ca == 0 {
                    continue;
                }
                let (fx, fy) = transform_point(&inv, x as f32 + 0.5, y as f32 + 0.5);
                let t = ((fx - p0.0) * dx + (fy - p0.1) * dy) / len_sq;
                let color = ramp_lookup(&ramp, apply_extend(t, extend));
                self.blend_pixel(idx, color, ca);
            }
        }
    }

    fn fill_radial_gradient(
        &mut self,
        c0: (f32, f32),
        r0: f32,
        c1: (f32, f32),
        r1: f32,
        color_stops: &[ColorStop],
        extend: Extend,
    ) {
        if color_stops.is_empty() {
            return;
        }
        if color_stops.len() == 1 {
            let c = resolve_color(
                color_stops[0].palette_index,
                color_stops[0].alpha,
                &self.palette,
                self.foreground,
            );
            self.fill_solid(c);
            return;
        }
        let Some(inv) = self.inverse_or_skip() else {
            return;
        };
        let ramp = build_ramp(&resolve_color_line(
            color_stops,
            &self.palette,
            self.foreground,
        ));

        let (w, h) = (self.width, self.height);
        for y in 0..h {
            for x in 0..w {
                let idx = (y * w + x) as usize;
                let ca = self.clip_alpha_at(idx);
                if ca == 0 {
                    continue;
                }
                let (fx, fy) = transform_point(&inv, x as f32 + 0.5, y as f32 + 0.5);
                // No valid root → pixel is outside the gradient cone, unpainted.
                let Some(t) = radial_t(c0, r0, c1, r1, (fx, fy)) else {
                    continue;
                };
                let color = ramp_lookup(&ramp, apply_extend(t, extend));
                self.blend_pixel(idx, color, ca);
            }
        }
    }

    fn fill_sweep_gradient(
        &mut self,
        center: (f32, f32),
        start_angle: f32,
        end_angle: f32,
        color_stops: &[ColorStop],
        extend: Extend,
    ) {
        if color_stops.is_empty() {
            return;
        }
        if color_stops.len() == 1 {
            let c = resolve_color(
                color_stops[0].palette_index,
                color_stops[0].alpha,
                &self.palette,
                self.foreground,
            );
            self.fill_solid(c);
            return;
        }
        let sector = end_angle - start_angle;
        if sector.abs() < 1e-9 {
            return;
        }
        let Some(inv) = self.inverse_or_skip() else {
            return;
        };
        let ramp = build_ramp(&resolve_color_line(
            color_stops,
            &self.palette,
            self.foreground,
        ));

        let (w, h) = (self.width, self.height);
        for y in 0..h {
            for x in 0..w {
                let idx = (y * w + x) as usize;
                let ca = self.clip_alpha_at(idx);
                if ca == 0 {
                    continue;
                }
                let (fx, fy) = transform_point(&inv, x as f32 + 0.5, y as f32 + 0.5);
                // skrifa delivers start/end as CLOCKWISE degrees from +x; in our
                // Y-up paint space the clockwise angle is atan2(-dy, dx). skrifa's
                // sweep stop normalization can place start/end outside [0,360)
                // (see Brush::SweepGradient docs), so fold the per-pixel angle
                // into the gradient's window [start_angle, start_angle+360)
                // rather than a fixed [0,360) — the latter would pin the seam to
                // the +x axis and paint the wrong endpoint for wedges across +x.
                let mut deg = (-(fy - center.1)).atan2(fx - center.0).to_degrees();
                deg -= 360.0 * ((deg - start_angle) / 360.0).floor();
                let t = (deg - start_angle) / sector;
                let color = ramp_lookup(&ramp, apply_extend(t, extend));
                self.blend_pixel(idx, color, ca);
            }
        }
    }

    fn into_image(mut self, placement_left: i32, placement_top: i32) -> SwashImage {
        // Compositing runs in premultiplied alpha; swash `Content::Color` images
        // use straight (non-premultiplied) alpha.
        unpremultiply_rgba(&mut self.buffer);
        SwashImage {
            source: ImageSource::ColorOutline(0),
            placement: Placement {
                left: placement_left,
                top: placement_top,
                width: self.width,
                height: self.height,
            },
            content: Content::Color,
            data: self.buffer,
        }
    }
}

impl ColorPainter for ColrV1Painter<'_> {
    fn push_transform(&mut self, transform: PaintTransform) {
        // child = parent * local (local applied first), matching skrifa's Mul.
        let new = self.current_transform() * transform;
        self.transform_stack.push(new);
    }

    fn pop_transform(&mut self) {
        if self.transform_stack.len() > 1 {
            self.transform_stack.pop();
        }
    }

    fn push_clip_glyph(&mut self, glyph_id: GlyphId) {
        let mask = self
            .rasterize_glyph_mask(glyph_id)
            .unwrap_or_else(|| vec![0u8; (self.width * self.height) as usize]);
        self.clip_stack.push(mask);
    }

    fn push_clip_box(&mut self, clip_box: skrifa::raw::types::BoundingBox<f32>) {
        // The clip rectangle becomes a (possibly rotated/sheared) parallelogram
        // under the current transform. Rasterize that quad as an AA mask.
        let t = self.current_transform();
        let corners = [
            (clip_box.x_min, clip_box.y_min),
            (clip_box.x_max, clip_box.y_min),
            (clip_box.x_max, clip_box.y_max),
            (clip_box.x_min, clip_box.y_max),
        ];
        let mut commands = Vec::with_capacity(6);
        for (i, &(fx, fy)) in corners.iter().enumerate() {
            let (px, py) = transform_point(&t, fx, fy);
            let v = zeno::Vector::new(px, py);
            if i == 0 {
                commands.push(Command::MoveTo(v));
            } else {
                commands.push(Command::LineTo(v));
            }
        }
        commands.push(Command::Close);
        // Corners are already in pixel space → render with identity transform.
        let mask = self.rasterize_mask(&commands, None);
        self.clip_stack.push(mask);
    }

    fn pop_clip(&mut self) {
        self.clip_stack.pop();
    }

    fn fill(&mut self, brush: Brush<'_>) {
        match brush {
            Brush::Solid {
                palette_index,
                alpha,
            } => {
                let c = resolve_color(palette_index, alpha, &self.palette, self.foreground);
                self.fill_solid(c);
            }
            Brush::LinearGradient {
                p0,
                p1,
                color_stops,
                extend,
            } => {
                self.fill_linear_gradient((p0.x, p0.y), (p1.x, p1.y), color_stops, extend);
            }
            Brush::RadialGradient {
                c0,
                r0,
                c1,
                r1,
                color_stops,
                extend,
            } => {
                self.fill_radial_gradient((c0.x, c0.y), r0, (c1.x, c1.y), r1, color_stops, extend);
            }
            Brush::SweepGradient {
                c0,
                start_angle,
                end_angle,
                color_stops,
                extend,
            } => {
                self.fill_sweep_gradient((c0.x, c0.y), start_angle, end_angle, color_stops, extend);
            }
        }
    }

    fn push_layer(&mut self, composite_mode: CompositeMode) {
        // Save the current buffer as the backdrop and start the layer on a
        // fresh, cleared buffer; `pop_layer` composites them with `mode`.
        let cleared = vec![0u8; (self.width * self.height * 4) as usize];
        let saved = core::mem::replace(&mut self.buffer, cleared);
        self.layer_stack.push((saved, composite_mode));
    }

    fn pop_layer(&mut self) {
        if let Some((backdrop, mode)) = self.layer_stack.pop() {
            composite_buffers(&mut self.buffer, &backdrop, mode);
        }
    }
}

// ─── Entry point ───────────────────────────────────────────────────────────

/// Attempt to render a glyph using `COLRv1` via skrifa.
///
/// `foreground` is the text color used for the COLR foreground sentinel
/// (palette index `0xFFFF`). Returns `None` if the glyph has no `COLRv1` data
/// or rendering fails (callers fall back to swash for `COLRv0`/bitmap/outline).
pub fn render_colr_v1(
    font_data: &[u8],
    face_index: u32,
    cache_key: &CacheKey,
    foreground: Color,
) -> Option<SwashImage> {
    let font = skrifa::FontRef::from_index(font_data, face_index).ok()?;

    let glyph_id = GlyphId::new(u32::from(cache_key.glyph_id));
    let font_size = f32::from_bits(cache_key.font_size_bits);

    // Only handle COLRv1 (COLRv0 is handled by swash).
    let color_glyphs = font.color_glyphs();
    let color_glyph = color_glyphs.get_with_format(glyph_id, ColorGlyphFormat::ColrV1)?;

    // Variation location. Only the weight axis is currently represented in the
    // cache key; other axes resolve at their defaults.
    let location = font
        .axes()
        .location([(Tag::new(b"wght"), f32::from(cache_key.font_weight.0))]);

    let metrics = font.metrics(Size::unscaled(), &location);
    let upem = f32::from(metrics.units_per_em);
    if upem <= 0.0 {
        return None;
    }
    let scale = font_size / upem;

    // Pixel-space bounds: prefer the glyph clip box, else the font vertical
    // metrics with a horizontal em extent.
    let bbox = color_glyph
        .bounding_box(&location, Size::new(font_size))
        .unwrap_or(skrifa::raw::types::BoundingBox {
            x_min: 0.0,
            y_min: metrics.descent * scale,
            x_max: font_size,
            y_max: metrics.ascent * scale,
        });

    let pad: f32 = 1.0;
    let pixel_x_min = bbox.x_min.floor();
    let pixel_x_max = bbox.x_max.ceil();
    let pixel_y_min = bbox.y_min.floor(); // bottom in font coords
    let pixel_y_max = bbox.y_max.ceil(); // top in font coords

    if pixel_x_max <= pixel_x_min || pixel_y_max <= pixel_y_min {
        return None;
    }

    // Compute padded extents in f32 and validate BEFORE casting to u32.
    // `f32 as u32` saturates to u32::MAX for huge/non-finite spans (e.g. an
    // extreme font_size), so adding padding after the cast could overflow (a
    // debug panic) or wrap past the MAX_DIM guard (release). This is a fallible
    // API, so reject such input with None. The `.is_finite()` checks also reject
    // NaN/Inf font sizes that slip past the `<=` comparison above.
    let fw = (pixel_x_max - pixel_x_min) + 2.0 * pad;
    let fh = (pixel_y_max - pixel_y_min) + 2.0 * pad;
    if !fw.is_finite()
        || !fh.is_finite()
        || fw < 1.0
        || fh < 1.0
        || fw > MAX_DIM as f32
        || fh > MAX_DIM as f32
    {
        return None;
    }
    let width = fw as u32;
    let height = fh as u32;

    // Base transform: font units (Y up) → pixel buffer (Y down) with padding.
    let base_transform = PaintTransform {
        xx: scale,
        yx: 0.0,
        xy: 0.0,
        yy: -scale,
        dx: -pixel_x_min + pad,
        dy: pixel_y_max + pad,
    };

    let palette = read_palette(&font, 0);
    let foreground = Rgba::from_color(foreground);

    let mut painter = ColrV1Painter::new(
        font,
        location,
        palette,
        foreground,
        width,
        height,
        base_transform,
    );

    if color_glyph
        .paint(&painter.location.clone(), &mut painter)
        .is_err()
    {
        return None;
    }

    let placement_left = pixel_x_min as i32 - pad as i32;
    let placement_top = pixel_y_max as i32 + pad as i32;
    Some(painter.into_image(placement_left, placement_top))
}

#[cfg(test)]
mod tests;
