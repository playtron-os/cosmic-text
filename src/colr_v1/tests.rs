// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Tests for the COLR v1 software rasterizer.
//
// The first group is font-independent: gradient color interpolation, the
// two-point conical solve, affine inversion, sRGB conversion, and the
// composite/blend modes are validated directly. The second group drives the
// full paint graph end-to-end against a tiny embedded COLR v1 font that
// exercises solid / linear / radial / sweep paints.

use super::*;

#[test]
fn mul_u8_fractional() {
    assert_eq!(mul_u8(255, 255), 255);
    assert_eq!(mul_u8(255, 0), 0);
    assert_eq!(mul_u8(0, 255), 0);
    assert_eq!(mul_u8(128, 255), 128);
    assert_eq!(mul_u8(128, 128), 64);
}

#[test]
fn src_over_premultiplied() {
    // Opaque source replaces destination.
    let mut dst = [10, 20, 30, 40];
    src_over(&mut dst, 100, 110, 120, 255);
    assert_eq!(dst, [100, 110, 120, 255]);

    // Fully transparent source leaves destination untouched.
    let mut dst = [10, 20, 30, 40];
    src_over(&mut dst, 200, 200, 200, 0);
    assert_eq!(dst, [10, 20, 30, 40]);

    // Half-opaque red (premultiplied r=128, a=128) over opaque black.
    let mut dst = [0, 0, 0, 255];
    src_over(&mut dst, 128, 0, 0, 128);
    assert_eq!(dst[0], 128);
    assert_eq!(dst[3], 255);
}

#[test]
fn unpremultiply_round_trips_straight_alpha() {
    let mut buf = [64u8, 0, 0, 128];
    unpremultiply_rgba(&mut buf);
    assert_eq!(buf, [128, 0, 0, 128]);

    let mut buf = [200u8, 100, 50, 255];
    unpremultiply_rgba(&mut buf);
    assert_eq!(buf, [200, 100, 50, 255]);

    let mut buf = [99u8, 99, 99, 0];
    unpremultiply_rgba(&mut buf);
    assert_eq!(buf, [0, 0, 0, 0]);

    let mut buf = [200u8, 0, 0, 100];
    unpremultiply_rgba(&mut buf);
    assert_eq!(buf[0], 255);
}

#[test]
fn apply_extend_modes() {
    assert!((apply_extend(-0.5, Extend::Pad) - 0.0).abs() < 1e-6);
    assert!((apply_extend(1.5, Extend::Pad) - 1.0).abs() < 1e-6);
    assert!((apply_extend(1.25, Extend::Repeat) - 0.25).abs() < 1e-6);
    assert!((apply_extend(-0.25, Extend::Repeat) - 0.75).abs() < 1e-6);
    assert!((apply_extend(1.25, Extend::Reflect) - 0.75).abs() < 1e-6);
    assert!((apply_extend(2.5, Extend::Reflect) - 0.5).abs() < 1e-6);
}

#[test]
fn transform_point_and_inverse() {
    let id = PaintTransform {
        xx: 1.0,
        yx: 0.0,
        xy: 0.0,
        yy: 1.0,
        dx: 0.0,
        dy: 0.0,
    };
    assert_eq!(transform_point(&id, 3.0, 4.0), (3.0, 4.0));

    // Non-trivial: scale + shear + rotation-ish + translation.
    let t = PaintTransform {
        xx: 2.0,
        yx: 0.5,
        xy: -1.0,
        yy: 3.0,
        dx: 5.0,
        dy: -2.0,
    };
    let inv = invert_transform(&t).expect("invertible");
    // inverse(forward(p)) ~= p
    let (px, py) = (7.0f32, -3.0f32);
    let (fx, fy) = transform_point(&t, px, py);
    let (bx, by) = transform_point(&inv, fx, fy);
    assert!((bx - px).abs() < 1e-3 && (by - py).abs() < 1e-3);

    // Singular transform is rejected.
    let singular = PaintTransform {
        xx: 1.0,
        yx: 2.0,
        xy: 2.0,
        yy: 4.0,
        dx: 0.0,
        dy: 0.0,
    };
    assert!(invert_transform(&singular).is_none());
}

#[test]
fn radial_t_concentric() {
    // Concentric gradient r0=0..r1=10 centered at origin: a point at distance d
    // maps to t = d / 10.
    let t = radial_t((0.0, 0.0), 0.0, (0.0, 0.0), 10.0, (5.0, 0.0)).expect("has a valid root");
    assert!((t - 0.5).abs() < 1e-4, "t = {t}");
    let t = radial_t((0.0, 0.0), 0.0, (0.0, 0.0), 10.0, (0.0, 8.0)).expect("has a valid root");
    assert!((t - 0.8).abs() < 1e-4, "t = {t}");

    // Fully degenerate (identical circles) yields no valid root.
    assert!(radial_t((0.0, 0.0), 5.0, (0.0, 0.0), 5.0, (3.0, 4.0)).is_none());
}

#[test]
fn color_line_interpolation_is_premultiplied_srgb() {
    // Opaque red -> opaque blue, interpolated in premultiplied sRGB: endpoints
    // exact, midpoint the sRGB average (~128) of each channel.
    let red = ResolvedStop {
        offset: 0.0,
        lr: 1.0,
        lg: 0.0,
        lb: 0.0,
        a: 1.0,
    };
    let blue = ResolvedStop {
        offset: 1.0,
        lr: 0.0,
        lg: 0.0,
        lb: 1.0,
        a: 1.0,
    };
    let stops = [red, blue];
    let lo = sample_color_line(&stops, 0.0);
    assert_eq!((lo.r, lo.g, lo.b, lo.a), (255, 0, 0, 255));
    let hi = sample_color_line(&stops, 1.0);
    assert_eq!((hi.r, hi.g, hi.b, hi.a), (0, 0, 255, 255));
    let mid = sample_color_line(&stops, 0.5);
    assert_eq!(mid.r, mid.b);
    assert!(
        (i32::from(mid.r) - 128).abs() <= 1,
        "sRGB midpoint averages to ~128: {}",
        mid.r
    );

    // Opaque red -> transparent: premultiplied interpolation keeps full red hue
    // at the midpoint (no dark fringing); only alpha drops.
    let transparent = ResolvedStop {
        offset: 1.0,
        lr: 0.0,
        lg: 0.0,
        lb: 0.0,
        a: 0.0,
    };
    let mid = sample_color_line(&[red, transparent], 0.5);
    assert_eq!(mid.r, 255, "no fringing: red stays saturated");
    assert!(
        (i32::from(mid.a) - 128).abs() <= 1,
        "alpha halves: {}",
        mid.a
    );
}

#[test]
fn ramp_lookup_endpoints() {
    let red = ResolvedStop {
        offset: 0.0,
        lr: 1.0,
        lg: 0.0,
        lb: 0.0,
        a: 1.0,
    };
    let blue = ResolvedStop {
        offset: 1.0,
        lr: 0.0,
        lg: 0.0,
        lb: 1.0,
        a: 1.0,
    };
    let ramp = build_ramp(&[red, blue]);
    assert_eq!(ramp.len(), RAMP_LEN);
    let lo = ramp_lookup(&ramp, 0.0);
    assert_eq!((lo.r, lo.b), (255, 0));
    let hi = ramp_lookup(&ramp, 1.0);
    assert_eq!((hi.r, hi.b), (0, 255));
    // Out-of-range t is clamped.
    assert_eq!(ramp_lookup(&ramp, -1.0), lo);
    assert_eq!(ramp_lookup(&ramp, 2.0), hi);
}

#[test]
fn composite_pixel_porter_duff() {
    let red = [1.0, 0.0, 0.0, 1.0];
    let blue = [0.0, 0.0, 1.0, 1.0];

    // Clear → transparent.
    assert_eq!(composite_pixel(red, blue, CompositeMode::Clear), [0.0; 4]);
    // Src → source.
    assert_eq!(composite_pixel(red, blue, CompositeMode::Src), red);
    // Dest → backdrop.
    assert_eq!(composite_pixel(red, blue, CompositeMode::Dest), blue);
    // SrcOver of opaque source → source.
    let o = composite_pixel(red, blue, CompositeMode::SrcOver);
    assert!((o[0] - 1.0).abs() < 1e-4 && o[3] > 0.99);

    // DestOut with opaque source erases the backdrop.
    let o = composite_pixel(red, blue, CompositeMode::DestOut);
    assert!(o[3] < 1e-4, "DestOut erases dest under opaque src");

    // SrcIn with transparent backdrop → nothing.
    let o = composite_pixel(red, [0.0, 0.0, 0.0, 0.0], CompositeMode::SrcIn);
    assert!(o[3] < 1e-4);
}

#[test]
fn composite_pixel_blend_modes() {
    // Multiply of two opaque mid-grays → darker (0.5*0.5 = 0.25).
    let g = [0.5, 0.5, 0.5, 1.0];
    let o = composite_pixel(g, g, CompositeMode::Multiply);
    assert!((o[0] - 0.25).abs() < 1e-3, "multiply: {}", o[0]);

    // Screen of two opaque mid-grays → lighter (1-(0.5)(0.5)=0.75).
    let o = composite_pixel(g, g, CompositeMode::Screen);
    assert!((o[0] - 0.75).abs() < 1e-3, "screen: {}", o[0]);

    // Darken / Lighten pick min / max per channel.
    let a = [0.2, 0.8, 0.5, 1.0];
    let b = [0.6, 0.3, 0.5, 1.0];
    let dk = composite_pixel(b, a, CompositeMode::Darken);
    assert!((dk[0] - 0.2).abs() < 1e-3 && (dk[1] - 0.3).abs() < 1e-3);
    let lt = composite_pixel(b, a, CompositeMode::Lighten);
    assert!((lt[0] - 0.6).abs() < 1e-3 && (lt[1] - 0.8).abs() < 1e-3);

    // HslLuminosity keeps backdrop's luminosity with source's... luminosity.
    // Sanity: result alpha is opaque and channels are in range.
    let o = composite_pixel(
        [0.9, 0.1, 0.1, 1.0],
        [0.2, 0.2, 0.8, 1.0],
        CompositeMode::HslColor,
    );
    assert!(o[3] > 0.99 && o.iter().take(3).all(|&c| (0.0..=1.0).contains(&c)));
}

#[test]
fn composite_buffers_modes() {
    // SrcOver fast path: transparent src → backdrop shows through.
    let backdrop = [10u8, 20, 30, 255];
    let mut src = [0u8, 0, 0, 0];
    composite_buffers(&mut src, &backdrop, CompositeMode::SrcOver);
    assert_eq!(src, backdrop);

    // Opaque src over anything → src kept.
    let backdrop = [10u8, 20, 30, 255];
    let mut src = [100u8, 110, 120, 255];
    composite_buffers(&mut src, &backdrop, CompositeMode::SrcOver);
    assert_eq!(src, [100, 110, 120, 255]);

    // Clear wipes everything.
    let backdrop = [10u8, 20, 30, 255];
    let mut src = [100u8, 110, 120, 255];
    composite_buffers(&mut src, &backdrop, CompositeMode::Clear);
    assert_eq!(src, [0, 0, 0, 0]);

    // DestOver: opaque backdrop wins where it already covers.
    let backdrop = [255u8, 0, 0, 255]; // opaque red (premult)
    let mut src = [0u8, 0, 255, 255]; // opaque blue (premult)
    composite_buffers(&mut src, &backdrop, CompositeMode::DestOver);
    assert_eq!(
        src,
        [255, 0, 0, 255],
        "backdrop shows over fully-covered dest"
    );
}

// ─── End-to-end paint-graph rendering against an embedded COLR v1 font ───────
//
// `test_font.bin` is a tiny generated COLR v1 font (raw sfnt bytes, not
// git-lfs tracked) whose glyphs exercise each paint kind:
//   'A' solid red, 'B' linear red→blue (left→right), 'C' radial red(center)→
//   blue(edge), 'D' sweep red→blue. Assertions check semantic properties, not
//   exact pixels, so they are robust to anti-aliasing and rasterizer detail.

const TEST_FONT: &[u8] = include_bytes!("test_font.bin");

fn glyph_id_for(ch: char) -> u16 {
    use skrifa::{FontRef, MetadataProvider};
    let font = FontRef::from_index(TEST_FONT, 0).expect("valid test font");
    font.charmap()
        .map(ch)
        .expect("test font maps the char")
        .to_u32() as u16
}

fn cache_key_for(glyph_id: u16, size: f32) -> CacheKey {
    use crate::CacheKeyFlags;
    use fontdb::Weight;
    // `render_colr_v1` reads the font bytes directly and ignores `font_id`,
    // so a dummy id is sufficient (and avoids depending on fontdb indexing).
    CacheKey::new(
        fontdb::ID::dummy(),
        glyph_id,
        size,
        (0.0, 0.0),
        Weight::NORMAL,
        CacheKeyFlags::empty(),
    )
    .0
}

fn render_char(ch: char, size: f32) -> SwashImage {
    let gid = glyph_id_for(ch);
    let ck = cache_key_for(gid, size);
    render_colr_v1(TEST_FONT, 0, &ck, Color::rgb(0, 0, 0)).expect("COLR v1 glyph should render")
}

fn is_red(px: &[u8]) -> bool {
    px[3] > 200
        && px[0] > 150
        && i32::from(px[0]) > i32::from(px[1]) + 40
        && i32::from(px[0]) > i32::from(px[2]) + 40
}
fn is_blue(px: &[u8]) -> bool {
    px[3] > 200
        && px[2] > 150
        && i32::from(px[2]) > i32::from(px[0]) + 40
        && i32::from(px[2]) > i32::from(px[1]) + 40
}

#[test]
fn renders_colr_v1_image_shape() {
    let img = render_char('A', 64.0);
    assert_eq!(img.content, Content::Color);
    assert!(img.placement.width > 0 && img.placement.height > 0);
    assert!(img.placement.width <= MAX_DIM && img.placement.height <= MAX_DIM);
    assert_eq!(
        img.data.len(),
        (img.placement.width * img.placement.height * 4) as usize
    );
    assert!(
        img.data.chunks_exact(4).any(|p| p[3] > 0),
        "image must have visible pixels"
    );
    // A plain (non-COLR-v1) glyph index returns None so the caller falls back.
    let ck0 = cache_key_for(0, 64.0);
    assert!(render_colr_v1(TEST_FONT, 0, &ck0, Color::rgb(0, 0, 0)).is_none());
}

#[test]
fn solid_paint_uses_cpal_color() {
    let img = render_char('A', 64.0);
    let visible = img.data.chunks_exact(4).filter(|p| p[3] > 200).count();
    let red = img.data.chunks_exact(4).filter(|p| is_red(p)).count();
    let black = img
        .data
        .chunks_exact(4)
        .filter(|p| p[3] > 200 && p[0] < 40 && p[1] < 40 && p[2] < 40)
        .count();
    assert!(visible > 0);
    assert!(red * 2 > visible, "most opaque pixels should be CPAL red");
    assert!(black * 4 < visible, "must not render as a black mask");
}

#[test]
fn linear_gradient_red_to_blue_left_to_right() {
    let img = render_char('B', 64.0);
    let w = i32::try_from(img.placement.width).expect("width fits i32");
    let (mut rx, mut rn, mut bx, mut bn) = (0i64, 0i64, 0i64, 0i64);
    for (i, px) in img.data.chunks_exact(4).enumerate() {
        let x = i64::from(i as i32 % w);
        if is_red(px) {
            rx += x;
            rn += 1;
        }
        if is_blue(px) {
            bx += x;
            bn += 1;
        }
    }
    assert!(rn > 0 && bn > 0, "linear gradient needs both endpoints");
    let (rmean, bmean) = (rx as f64 / rn as f64, bx as f64 / bn as f64);
    assert!(
        rmean < bmean,
        "red ({rmean}) should be left of blue ({bmean})"
    );
}

#[test]
fn radial_gradient_red_center_blue_edge() {
    let img = render_char('C', 64.0);
    let w = i32::try_from(img.placement.width).expect("width fits i32");
    let h = i32::try_from(img.placement.height).expect("height fits i32");
    let (cx, cy) = (f64::from(w) / 2.0, f64::from(h) / 2.0);
    let dist = |i: i32| {
        let x = f64::from(i % w);
        let y = f64::from(i / w);
        ((x - cx).powi(2) + (y - cy).powi(2)).sqrt()
    };
    let (mut rd, mut rn, mut bd, mut bn) = (0f64, 0f64, 0f64, 0f64);
    for (i, px) in img.data.chunks_exact(4).enumerate() {
        if is_red(px) {
            rd += dist(i as i32);
            rn += 1.0;
        }
        if is_blue(px) {
            bd += dist(i as i32);
            bn += 1.0;
        }
    }
    assert!(rn > 0.0 && bn > 0.0, "radial gradient needs both endpoints");
    assert!(rd / rn < bd / bn, "red should be nearer center than blue");
}

#[test]
fn sweep_gradient_is_angular_not_flat() {
    // The previous implementation collapsed sweep gradients to one middle color.
    // A correct sweep contains both endpoint colors at different angles.
    let img = render_char('D', 64.0);
    let red = img.data.chunks_exact(4).filter(|p| is_red(p)).count();
    let blue = img.data.chunks_exact(4).filter(|p| is_blue(p)).count();
    assert!(
        red > 0 && blue > 0,
        "sweep must contain both red and blue (red={red} blue={blue})"
    );
}

#[test]
fn clip_mask_y_orientation() {
    // 'E' is solid red clipped to the TOP half of the em (font y 500..1000).
    // After the Y-flip into pixel space that is the top of the image (small y).
    // A vertical flip in the clip-mask blit would push the red to the bottom or
    // off-buffer, so this guards the placement.top handling in blit_mask_to_full.
    let img = render_char('E', 64.0);
    let w = i32::try_from(img.placement.width).expect("width fits i32");
    let h = i32::try_from(img.placement.height).expect("height fits i32");
    let half = h / 2;
    let (mut top, mut bot) = (0i64, 0i64);
    for (i, px) in img.data.chunks_exact(4).enumerate() {
        if is_red(px) {
            if i as i32 / w < half {
                top += 1;
            } else {
                bot += 1;
            }
        }
    }
    assert!(
        top > 0,
        "top-half clip must paint red in the top of the image"
    );
    assert!(
        top > bot * 4,
        "red should be concentrated in the top half (top={top} bot={bot})"
    );
}

#[test]
fn extreme_font_size_returns_none_not_panic() {
    // A pathological font size must not overflow/panic the size computation.
    let ck = cache_key_for(glyph_id_for('A'), 1.0e10);
    assert!(render_colr_v1(TEST_FONT, 0, &ck, Color::rgb(0, 0, 0)).is_none());
}
