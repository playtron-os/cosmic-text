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
