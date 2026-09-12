// A line must report the width its own glyphs add up to.
//
// `Hinting::Enabled` used to round each glyph's advance where it PLACED the glyph
// and nowhere else: `LayoutLine::w` came from `ShapeGlyph::width`, which never
// rounded, so hinted text was drawn with rounded advances inside a box measured
// with unrounded ones. A consumer that measures a paragraph and then draws it —
// iced feeds `min_bounds` straight back as the wrap width — got a box that
// disagreed with its contents by up to half a pixel per glyph.
//
// Both paths now go through `ShapeGlyph::advance_px`, so the invariant below holds
// in every mode. Before that change the `Hinting::Enabled` cases failed: Inter at
// 14px reported `line_w = 135.5455` for glyphs summing to `135.0`.
use cosmic_text::{
    fontdb::Database, Attrs, Buffer, Family, FontSystem, Hinting, Metrics, Shaping, Wrap,
};
use std::path::PathBuf;

fn font_system() -> FontSystem {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fonts");
    let mut db = Database::new();
    db.load_fonts_dir(dir);
    FontSystem::new_with_locale_and_db("en-US".into(), db)
}

/// Every string long enough to exercise a mix of advances, in both faces the
/// repository bundles, at sizes whose advances are decidedly fractional.
const TEXTS: &[&str] = &[
    "Added to references",
    "The quick brown fox jumps over the lazy dog",
    "iiii WWWW ....",
];

fn check(fs: &mut FontSystem, family: &str, size: f32, ls_px: Option<f32>, hinting: Hinting) {
    for text in TEXTS {
        let mut buf = Buffer::new(fs, Metrics::new(size, size * 1.5));
        buf.set_hinting(fs, hinting);
        buf.set_wrap(fs, Wrap::None);
        let mut attrs = Attrs::new().family(Family::Name(family));
        if let Some(px) = ls_px {
            // cosmic-text takes letter spacing in em units, the way iced converts it.
            attrs = attrs.letter_spacing(px / size);
        }
        buf.set_text(fs, text, &attrs, Shaping::Advanced, None);
        buf.shape_until_scroll(fs, false);

        for run in buf.layout_runs() {
            let sum: f32 = run.glyphs.iter().map(|g| g.w).sum();
            assert!(
                (sum - run.line_w).abs() < 0.01,
                "{family} {size}px ls={ls_px:?} {hinting:?} \"{text}\": \
                 line_w {} but the glyphs sum to {sum}",
                run.line_w,
            );
        }
    }
}

#[test]
fn line_width_is_the_sum_of_its_glyph_advances() {
    let mut fs = font_system();
    for hinting in [Hinting::Disabled, Hinting::Enabled] {
        for family in ["Inter", "Fira Mono"] {
            for size in [9.5f32, 10.5, 14.0, 18.0] {
                check(&mut fs, family, size, None, hinting);
                check(&mut fs, family, size, Some(1.14), hinting);
            }
        }
    }
}

// Letter spacing sits OUTSIDE the grid snap.
//
// CSS defines `letter-spacing` as space added between glyphs, not as part of a
// glyph's advance, so what snaps to the pixel grid is the glyph's own advance and
// the spacing rides on top of it unsnapped. `Hinting::Enabled` used to round the
// sum, because the spacing had already been folded into `ShapeGlyph::x_advance` at
// shape time and could not be told apart again.
//
// The numbers below are Chrome's, measured on Geist Mono at 9.5px with
// `letter-spacing: 0.12em` (= 1.14px): every glyph advances 7.1406 and the line
// total is 121.3906 — fractional, which it could not be if the spacing were inside
// the round. Fira Mono stands in for Geist Mono here because it is the monospace
// face this repository bundles; the arithmetic under test is the same.
#[test]
fn letter_spacing_is_added_after_the_grid_snap() {
    let mut fs = font_system();
    let size = 9.5f32;
    let spacing = 1.14f32;

    let mut exact = Buffer::new(&mut fs, Metrics::new(size, size * 1.5));
    exact.set_wrap(&mut fs, Wrap::None);
    let attrs = Attrs::new()
        .family(Family::Name("Fira Mono"))
        .letter_spacing(spacing / size);
    exact.set_text(&mut fs, "MMMMMMMMMM", &attrs, Shaping::Advanced, None);
    exact.shape_until_scroll(&mut fs, false);
    let unspaced_advance = {
        let run = exact.layout_runs().next().unwrap();
        run.glyphs[0].w - spacing
    };

    let mut hinted = Buffer::new(&mut fs, Metrics::new(size, size * 1.5));
    hinted.set_hinting(&mut fs, Hinting::Enabled);
    hinted.set_wrap(&mut fs, Wrap::None);
    hinted.set_text(&mut fs, "MMMMMMMMMM", &attrs, Shaping::Advanced, None);
    hinted.shape_until_scroll(&mut fs, false);

    let run = hinted.layout_runs().next().unwrap();
    let want = unspaced_advance.round() + spacing;
    for g in run.glyphs {
        assert!(
            (g.w - want).abs() < 0.001,
            "glyph advanced {} but round({unspaced_advance}) + {spacing} = {want}",
            g.w,
        );
    }
    // The tell: a line of snapped advances plus unsnapped spacing is fractional.
    assert!(
        (run.line_w - want * 10.0).abs() < 0.01,
        "line_w {} but 10 glyphs of {want} is {}",
        run.line_w,
        want * 10.0,
    );
    assert!(
        (run.line_w - run.line_w.round()).abs() > 0.01,
        "line_w {} is a whole number, so the spacing was rounded with the advance",
        run.line_w,
    );
}

// A snapped line stays inside the box it was laid out in, whatever its alignment.
//
// Two ways it did not. Aligning to the far edge puts the glyphs flush against it,
// and the line origin was then rounded to NEAREST — so a 178px run of snapped
// glyphs in a 200.5px box started at `round(22.5) = 23` and ended at 201, half a
// pixel outside. And justification expansion was applied INSIDE the snap, so the
// width a space actually gained was `round(a + J) - round(a)` rather than `J`, and
// a justified line missed the width it was justified into by up to `spaces / 2`
// px — 202 in a 200.5px box.
//
// Both predate the measurement fix above; they only became visible once the width
// a line reports was the width it draws.
#[test]
fn a_snapped_line_stays_inside_its_box() {
    use cosmic_text::Align;

    let mut fs = font_system();
    // Deliberately fractional, so rounding the origin has somewhere to go wrong.
    const W: f32 = 200.5;
    let cases: &[(&str, &str)] = &[
        (
            "Inter",
            "The quick brown fox jumps over the lazy dog and keeps on running far",
        ),
        // RTL, where the line is laid out from the right edge leftwards.
        (
            "Noto Sans Hebrew",
            "שלום עולם זה טקסט ארוך מאוד שצריך לעבור לשורה",
        ),
    ];

    for &(family, text) in cases {
        for align in [
            Align::Left,
            Align::Right,
            Align::Center,
            Align::End,
            Align::Justified,
        ] {
            for hinting in [Hinting::Disabled, Hinting::Enabled] {
                let mut buf = Buffer::new(&mut fs, Metrics::new(14.0, 21.0));
                buf.set_hinting(&mut fs, hinting);
                buf.set_wrap(&mut fs, Wrap::Word);
                buf.set_size(&mut fs, Some(W), Some(400.0));
                buf.set_text(
                    &mut fs,
                    text,
                    &Attrs::new().family(Family::Name(family)),
                    Shaping::Advanced,
                    Some(align),
                );
                buf.shape_until_scroll(&mut fs, false);

                for run in buf.layout_runs() {
                    let what = format!("{family} {align:?} {hinting:?}");
                    let left = run.glyphs.iter().map(|g| g.x).fold(f32::MAX, f32::min);
                    let right = run
                        .glyphs
                        .iter()
                        .map(|g| g.x + g.w)
                        .fold(f32::MIN, f32::max);
                    if run.glyphs.is_empty() {
                        continue;
                    }
                    assert!(
                        left >= -0.01 && right <= W + 0.01,
                        "{what}: glyphs span {left}..{right}, outside 0..{W}",
                    );
                    let sum: f32 = run.glyphs.iter().map(|g| g.w).sum();
                    assert!(
                        (sum - run.line_w).abs() < 0.01,
                        "{what}: line_w {} but the glyphs sum to {sum}",
                        run.line_w,
                    );
                }
            }
        }
    }
}

// A justified line lands exactly on the width it was justified into, snapped or not.
#[test]
fn justification_closes_on_the_line_width() {
    use cosmic_text::Align;

    let mut fs = font_system();
    const W: f32 = 200.5;
    let text = "The quick brown fox jumps over the lazy dog and keeps on running far";

    for hinting in [Hinting::Disabled, Hinting::Enabled] {
        let mut buf = Buffer::new(&mut fs, Metrics::new(14.0, 21.0));
        buf.set_hinting(&mut fs, hinting);
        buf.set_wrap(&mut fs, Wrap::Word);
        buf.set_size(&mut fs, Some(W), Some(400.0));
        buf.set_text(
            &mut fs,
            text,
            &Attrs::new().family(Family::Name("Inter")),
            Shaping::Advanced,
            Some(Align::Justified),
        );
        buf.shape_until_scroll(&mut fs, false);

        let runs: Vec<_> = buf.layout_runs().collect();
        // Every line but the last is stretched to the full measure.
        for run in &runs[..runs.len().saturating_sub(1)] {
            assert!(
                (run.line_w - W).abs() < 0.01,
                "{hinting:?}: a justified line came out {} wide in a {W} box",
                run.line_w,
            );
        }
    }
}
