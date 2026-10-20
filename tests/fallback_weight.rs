use cosmic_text::{fontdb, Attrs, Buffer, Fallback, Family, FontSystem, Metrics, Shaping, Weight};
use unicode_script::Script;

/// Falls back to `common` and nothing else.
struct Common(&'static [&'static str]);

impl Fallback for Common {
    fn common_fallback(&self) -> &[&'static str] {
        self.0
    }

    fn forbidden_fallback(&self) -> &[&'static str] {
        &[]
    }

    fn script_fallback(&self, _script: Script, _locale: &str) -> &[&'static str] {
        &[]
    }
}

/// Loads each `(file, weight)` in order, as a face of that weight.
fn font_system(fonts: &[(&str, u16)], common: &'static [&'static str]) -> FontSystem {
    let mut db = fontdb::Database::new();
    for (file, weight) in fonts {
        let data = std::fs::read(format!("fonts/{file}")).unwrap();
        let id = db.load_font_source(fontdb::Source::Binary(std::sync::Arc::new(data)))[0];
        let mut face = db.face(id).unwrap().clone();
        db.remove_face(id);
        face.weight = fontdb::Weight(*weight);
        db.push_face_info(face);
    }
    FontSystem::new_with_locale_and_db_and_fallback("en-US".into(), db, Common(common))
}

/// The family the first glyph of `text` is drawn in.
fn family_of(font_system: &mut FontSystem, family: &str, weight: u16, text: &str) -> String {
    let mut buffer = Buffer::new(font_system, Metrics::new(14.0, 20.0));
    let attrs = Attrs::new()
        .family(Family::Name(family))
        .weight(Weight(weight));
    buffer.set_text(font_system, text, &attrs, Shaping::Advanced, None);
    buffer.shape_until_scroll(font_system, false);
    let id = buffer.layout_runs().next().unwrap().glyphs[0].font_id;
    font_system.db().face(id).unwrap().families[0].0.clone()
}

// A fallback family that has only a regular face still serves medium text, as it does
// regular text, rather than being passed over for whatever font comes next.
#[test]
fn fallback_family_serves_every_weight() {
    let mut font_system = font_system(
        &[("NotoSans-Regular.ttf", 400), ("Inter-Regular.ttf", 400)],
        &["Inter"],
    );
    for weight in [400, 500, 700] {
        assert_eq!(
            family_of(&mut font_system, "Missing", weight, "A"),
            "Inter",
            "weight {weight}"
        );
    }
}

// A named family is used at its nearest weight before any fallback family is tried.
#[test]
fn named_family_serves_every_weight() {
    let mut font_system = font_system(
        &[("Inter-Regular.ttf", 400), ("NotoSans-Regular.ttf", 500)],
        &["Noto Sans"],
    );
    for weight in [400, 500, 700] {
        assert_eq!(
            family_of(&mut font_system, "Inter", weight, "A"),
            "Inter",
            "weight {weight}"
        );
    }
}
