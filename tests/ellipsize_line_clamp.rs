// A paragraph clamped to N lines by height, as CSS `-webkit-line-clamp` does: exactly N
// lines fit a box N line-heights tall, and the last one ends in "…" flush with its start.
use cosmic_text::{
    fontdb::Database, Attrs, Buffer, Ellipsize, EllipsizeHeightLimit, Family, FontSystem, Metrics,
    Shaping, Wrap,
};
use std::path::PathBuf;

fn font_system() -> FontSystem {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fonts");
    let mut db = Database::new();
    db.load_fonts_dir(dir);
    FontSystem::new_with_locale_and_db("en-US".into(), db)
}

const TEXT: &str =
    "The quick brown fox jumps over the lazy dog and keeps running past the fence, across the \
     field and down to the river, where it finally stops to drink.";

/// Each visual line's text, with the ellipsis glyph (an empty source range) as "…".
fn clamp(fs: &mut FontSystem, wrap: Wrap, line_height: f32, height: f32) -> Vec<String> {
    let mut buf = Buffer::new(fs, Metrics::new(12.0, line_height));
    buf.set_wrap(fs, wrap);
    buf.set_size(fs, Some(150.0), Some(height));
    buf.set_ellipsize(fs, Ellipsize::End(EllipsizeHeightLimit::Height(height)));
    buf.set_text(
        fs,
        TEXT,
        &Attrs::new().family(Family::Name("Inter")),
        Shaping::Advanced,
        None,
    );
    buf.shape_until_scroll(fs, false);
    buf.layout_runs()
        .map(|run| {
            run.glyphs
                .iter()
                .map(|g| {
                    if g.start == g.end {
                        "…"
                    } else {
                        &run.text[g.start..g.end]
                    }
                })
                .collect()
        })
        .collect()
}

#[test]
fn an_exact_fit_clamps_to_that_many_lines() {
    let mut fs = font_system();
    for wrap in [Wrap::Word, Wrap::WordOrGlyph] {
        for (line_height, lines) in [(18.0, 2usize), (18.0, 3), (16.0, 2), (15.0, 4)] {
            let got = clamp(&mut fs, wrap, line_height, line_height * lines as f32);
            assert_eq!(got.len(), lines, "{wrap:?} {line_height}x{lines}: {got:?}");
            assert!(
                got.last().is_some_and(|l| l.ends_with('…')),
                "{wrap:?} {line_height}x{lines}: the last line has no ellipsis: {got:?}"
            );
        }
    }
}

#[test]
fn the_ellipsized_line_starts_at_its_first_word() {
    let mut fs = font_system();
    for wrap in [Wrap::Word, Wrap::WordOrGlyph] {
        let got = clamp(&mut fs, wrap, 18.0, 35.0);
        let last = got.last().expect("a line");
        assert!(
            !last.starts_with(char::is_whitespace),
            "{wrap:?}: the clamped line starts with the wrap's blank: {last:?}"
        );
    }
}
