use cosmic_text::{fontdb, Attrs, Buffer, Family, FontSystem, Metrics, Shaping, Wrap};

/// Geist at 16px in a buffer `width` wide, wrapping `wrap`.
fn buffer(text: &str, width: f32, wrap: Wrap) -> (FontSystem, Buffer) {
    let mut db = fontdb::Database::new();
    db.load_font_data(include_bytes!("fonts/Geist-Regular.ttf").to_vec());
    db.set_sans_serif_family("Geist");
    let mut font_system = FontSystem::new_with_locale_and_db("en-US".into(), db);

    let mut buffer = Buffer::new(&mut font_system, Metrics::new(16.0, 24.0));
    buffer.set_wrap(&mut font_system, wrap);
    buffer.set_size(&mut font_system, Some(width), None);
    buffer.set_text(
        &mut font_system,
        text,
        &Attrs::new().family(Family::Name("Geist")),
        Shaping::Advanced,
        None,
    );
    buffer.shape_until_scroll(&mut font_system, false);
    (font_system, buffer)
}

/// Each visual line's words.
fn lines(buffer: &Buffer) -> Vec<String> {
    buffer
        .layout_runs()
        .map(|run| {
            let start = run
                .glyphs
                .iter()
                .map(|glyph| glyph.start)
                .min()
                .unwrap_or(0);
            let end = run.glyphs.iter().map(|glyph| glyph.end).max().unwrap_or(0);
            run.text[start..end].trim().to_owned()
        })
        .collect()
}

/// Lays `buffer` out again after its first line's wrap breaks change.
fn break_at(font_system: &mut FontSystem, buffer: &mut Buffer, breaks: &[&str]) {
    let text = buffer.lines[0].text().to_owned();
    let offsets = breaks.iter().map(|word| text.find(word).unwrap()).collect();
    assert!(buffer.lines[0].set_wrap_breaks(offsets));
    buffer.shape_until_scroll(font_system, false);
}

#[test]
fn a_line_starts_at_each_wrap_break() {
    let (mut font_system, mut buffer) = buffer("one two three four five", 1000.0, Wrap::Word);
    assert_eq!(lines(&buffer), ["one two three four five"]);

    break_at(&mut font_system, &mut buffer, &["three", "five"]);
    assert_eq!(lines(&buffer), ["one two", "three four", "five"]);
}

#[test]
fn the_width_still_wraps_between_breaks() {
    let (mut font_system, mut buffer) = buffer("one two three four five", 80.0, Wrap::Word);
    let greedy = lines(&buffer);
    assert!(greedy.len() > 2, "{greedy:?}");

    break_at(&mut font_system, &mut buffer, &["two"]);
    let broken = lines(&buffer);
    assert_eq!(broken[0], "one");
    assert!(broken.iter().all(|line| !line.is_empty()), "{broken:?}");
    assert_eq!(broken.join(" "), "one two three four five");
}

#[test]
fn a_break_inside_a_word_or_at_the_start_does_nothing() {
    let (mut font_system, mut buffer) = buffer("one two three", 1000.0, Wrap::Word);
    assert!(buffer.lines[0].set_wrap_breaks(vec![0, 5]));
    buffer.shape_until_scroll(&mut font_system, false);
    assert_eq!(lines(&buffer), ["one two three"]);
}

#[test]
fn word_or_glyph_wrapping_honors_breaks_too() {
    let (mut font_system, mut buffer) = buffer("one two three", 1000.0, Wrap::WordOrGlyph);
    break_at(&mut font_system, &mut buffer, &["three"]);
    assert_eq!(lines(&buffer), ["one two", "three"]);
}

#[test]
fn glyph_wrapping_ignores_breaks() {
    let (mut font_system, mut buffer) = buffer("one two three", 1000.0, Wrap::Glyph);
    break_at(&mut font_system, &mut buffer, &["three"]);
    assert_eq!(lines(&buffer), ["one two three"]);
}

#[test]
fn new_text_forgets_the_breaks() {
    let (mut font_system, mut buffer) = buffer("one two three", 1000.0, Wrap::Word);
    break_at(&mut font_system, &mut buffer, &["three"]);
    assert_eq!(lines(&buffer), ["one two", "three"]);

    buffer.set_text(
        &mut font_system,
        "one two three",
        &Attrs::new()
            .family(Family::Name("Geist"))
            .weight(cosmic_text::Weight::BOLD),
        Shaping::Advanced,
        None,
    );
    buffer.shape_until_scroll(&mut font_system, false);
    assert!(buffer.lines[0].wrap_breaks().is_empty());
    assert_eq!(lines(&buffer), ["one two three"]);
}

#[test]
fn setting_the_same_breaks_keeps_the_layout() {
    let (mut font_system, mut buffer) = buffer("one two three", 1000.0, Wrap::Word);
    break_at(&mut font_system, &mut buffer, &["three"]);
    assert!(!buffer.lines[0].set_wrap_breaks(vec![8]));
    assert!(buffer.lines[0].layout_opt().is_some());
}
