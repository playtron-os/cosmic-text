use cosmic_text::{Attrs, AttrsList, AttrsOwned, Color, Decoration, DecorationStyle};

// Validates the paint-only decoration attributes (underline, strikethrough,
// highlight background) end to end at the attribute layer: builders set them,
// the owned<->borrowed conversion preserves them, and per-span `AttrsList`
// lookups return them only on the ranges they were applied to.
//
// The remaining hop (Attrs -> ShapeGlyph -> LayoutGlyph) is a straight
// field-for-field copy at three struct-literal sites, so the compiler already
// guarantees it: the build fails if any field is dropped. A full shape+render
// assertion needs a working FontSystem, which this checkout's sandbox cannot
// provide (the bundled-font tests fail identically without these changes); that
// path is covered later by the storybook spike where fonts load.
#[test]
fn decoration_attrs_round_trip_through_attrs_list() {
    let red = Color::rgb(0xff, 0x00, 0x00);
    let green = Color::rgb(0x00, 0xff, 0x00);
    let blue = Color::rgb(0x00, 0x00, 0xff);

    let underline = Decoration::new();
    let strike = Decoration::new().style(DecorationStyle::Wavy).color(blue);

    let base = Attrs::new();
    let deco = Attrs::new()
        .color(red)
        .underline(underline)
        .strikethrough(strike)
        .background(green);

    // Builders set the fields.
    assert_eq!(deco.underline_opt, Some(underline));
    assert_eq!(deco.strikethrough_opt, Some(strike));
    assert_eq!(deco.background_opt, Some(green));
    assert_eq!(base.underline_opt, None);

    // Decoration builder details.
    assert_eq!(underline.style, DecorationStyle::Solid);
    assert_eq!(underline.color_opt, None, "underline inherits text color");
    assert_eq!(strike.style, DecorationStyle::Wavy);
    assert_eq!(strike.color_opt, Some(blue));

    // AttrsOwned <-> Attrs conversion preserves decorations.
    let owned = AttrsOwned::new(&deco);
    assert_eq!(owned.underline_opt, Some(underline));
    assert_eq!(owned.strikethrough_opt, Some(strike));
    assert_eq!(owned.background_opt, Some(green));
    let back = owned.as_attrs();
    assert_eq!(back.underline_opt, Some(underline));
    assert_eq!(back.strikethrough_opt, Some(strike));
    assert_eq!(back.background_opt, Some(green));
    assert_eq!(back.color_opt, Some(red));

    // Per-span AttrsList: default range is plain, the [3..6) span is decorated.
    let mut list = AttrsList::new(&base);
    list.add_span(3..6, &deco);

    let plain = list.get_span(0);
    assert_eq!(plain.underline_opt, None);
    assert_eq!(plain.strikethrough_opt, None);
    assert_eq!(plain.background_opt, None);

    let got = list.get_span(4);
    assert_eq!(got.underline_opt, Some(underline));
    assert_eq!(got.strikethrough_opt, Some(strike));
    assert_eq!(got.background_opt, Some(green));
    assert_eq!(got.color_opt, Some(red));

    // Paint-only: decorations must not change shape-run compatibility, so a run
    // can freely mix decorated and undecorated glyphs in a single shaping pass.
    assert!(base.compatible(&deco));
}
