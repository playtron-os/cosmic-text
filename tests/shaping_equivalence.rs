//! Shaping through the shape-run cache or the kern-only fast path must give exactly
//! the glyphs harfrust gives when it shapes every run afresh: the same glyph ids,
//! fonts, clusters and advances, bit for bit, in each of the fonts below and over a
//! corpus of UI text.
#![cfg(any(feature = "shape-run-cache", feature = "kern-fast-path"))]

use cosmic_text::{
    fontdb, Attrs, AttrsList, Buffer, Color, Family, FeatureTag, FontFeatures, FontSystem, Metrics,
    ShapeLine, Shaping, Weight, Wrap,
};

/// The Kora and Humain faces, and Open Sans, which kerns with a legacy `kern` table.
const FONTS: &[(&str, &[u8])] = &[
    ("Geist", include_bytes!("fonts/Geist-Regular.ttf")),
    ("Inter", include_bytes!("fonts/Inter-Regular.ttf")),
    (
        "Instrument Serif",
        include_bytes!("fonts/InstrumentSerif-Regular.ttf"),
    ),
    ("Geist Mono", include_bytes!("fonts/GeistMono-Regular.ttf")),
    ("Open Sans", include_bytes!("fonts/OpenSans-Regular.ttf")),
];

const HEBREW: &[u8] = include_bytes!("../fonts/NotoSansHebrew.ttf");

const IPSUM: &str = "Lorem ipsum dolor sit amet, consectetur adipiscing elit. Curabitur at elit mollis, dictum nunc non, tempus metus. Sed iaculis ac mauris eu lobortis. Integer elementum venenatis eros, id placerat odio feugiat vel. Maecenas consequat convallis tincidunt. Nunc eu lorem justo. Praesent quis ornare sapien. Aliquam interdum tortor ut rhoncus faucibus.";

/// UI text of the kind the Kora and Humain apps show, plus text that exercises
/// ligatures, contextual alternates, fallback, bidi and tabs.
const CORPUS: &[&str] = &[
    "Move to Trash",
    "Display brightness and color temperature",
    "Network connection settings for Wi-Fi",
    "Bluetooth devices nearby",
    "Sound output and input volume",
    "Keyboard shortcuts",
    "Storage: 12.4 GB of 256 GB used",
    "Downloads — 3 files, 1.2 MB",
    "Shared with you · Yesterday",
    "Updated 5 minutes ago",
    "firefox - PID 1007 - 12.3% CPU - 512 MB",
    "kora-sync  cosmic-comp  pipewire  Xwayland",
    "systemd-network  Sleeping  0.0%  8 MB",
    "WAVE AVATAR Toyota Type LTA Yes, Wo, To, Vo, P. F. T.",
    "office effect affine fjord fifty flow ffl ft",
    "-> => != <= >= == === !== :: // /* */ <!-- --> |> <| <> ~= ~> <~",
    "10:30 3x4 1920x1080 1/2 3/4 12:00:00 x2 2x",
    "(A) [B] {C} (x) [y] {z} (ABC) [DEF]",
    "e.g. i.e. U.S.A. A.V. don't it's 5 o'clock \"quoted\" 'single'",
    "user@example.com https://example.com/path?q=1&x=2 ~/Documents C:\\Windows",
    "snake_case kebab-case CamelCase ALL CAPS TITLE $9.99 #42 @user 100%",
    "a*b+c=d x^2 `code` |pipe| ... -- --- !! ?? ?! #hash",
    "0123456789 00 11 22 1.0 2,000 3.14159 -5 +7 ±3",
    "Café naïve façade Straße can’t won’t “quoted” — … · → ✓",
    "Ελληνικά Кириллица 日本語テキスト",
    "tab\tseparated\tcolumns\t1\t2",
    " leading and trailing spaces ",
    "!\"#$%&'()*+,-./:;<=>?@[\\]^_`{|}~",
    "ABCDEFGHIJKLMNOPQRSTUVWXYZ abcdefghijklmnopqrstuvwxyz",
    IPSUM,
];

/// A glyph as a comparable value: its cluster, glyph, font and every float bit.
type Signature = (bool, usize, usize, u16, fontdb::ID, [u32; 5]);

fn font_system() -> FontSystem {
    let mut db = fontdb::Database::new();
    for (_, data) in FONTS {
        db.load_font_data(data.to_vec());
    }
    db.load_font_data(HEBREW.to_vec());
    FontSystem::new_with_locale_and_db("en-US".into(), db)
}

/// A font system that shapes with the cache and fast path as asked. The reference
/// is one with neither: harfrust shaping every run.
fn configured(cache: bool, fast_path: bool) -> FontSystem {
    let mut font_system = font_system();
    #[cfg(feature = "shape-run-cache")]
    if !cache {
        font_system.shape_run_cache.set_max_glyphs(0);
    }
    #[cfg(feature = "kern-fast-path")]
    font_system.set_kern_fast_path(fast_path);
    let _ = (cache, fast_path);
    font_system
}

fn reference() -> FontSystem {
    configured(false, false)
}

fn shape(font_system: &mut FontSystem, text: &str, attrs_list: &AttrsList) -> Vec<Signature> {
    let line = ShapeLine::new(font_system, text, attrs_list, Shaping::Advanced, 8);
    let mut out = Vec::new();
    for span in &line.spans {
        for word in &span.words {
            for g in &word.glyphs {
                out.push((
                    word.blank,
                    g.start,
                    g.end,
                    g.glyph_id,
                    g.font_id,
                    [
                        g.x_advance.to_bits(),
                        g.x_advance_unspaced.to_bits(),
                        g.y_advance.to_bits(),
                        g.x_offset.to_bits(),
                        g.y_offset.to_bits(),
                    ],
                ));
            }
        }
    }
    out
}

/// The attribute lists each corpus line is shaped with: plain, heavier,
/// letter-spaced without ligatures (as iced spaces text), tabular figures, and a
/// span in the middle of the line that differs in color and weight.
fn attrs_lists(family: &'static str, text: &str) -> Vec<AttrsList> {
    let plain = Attrs::new().family(Family::Name(family));
    let mut no_ligatures = FontFeatures::new();
    no_ligatures
        .disable(FeatureTag::STANDARD_LIGATURES)
        .disable(FeatureTag::CONTEXTUAL_LIGATURES)
        .disable(FeatureTag::CONTEXTUAL_ALTERNATES);
    let spaced = plain
        .clone()
        .letter_spacing(0.12)
        .font_features(no_ligatures);
    let mut tabular = FontFeatures::new();
    tabular.enable(FeatureTag::new(b"tnum"));

    let mut with_span = AttrsList::new(&plain);
    let mid = text.len() / 2;
    if let (Some(from), Some(to)) = (
        text.char_indices().map(|(i, _)| i).find(|&i| i >= mid / 2),
        text.char_indices().map(|(i, _)| i).find(|&i| i >= mid),
    ) {
        with_span.add_span(
            from..to,
            &plain
                .clone()
                .color(Color::rgb(0xff, 0, 0))
                .weight(Weight::BOLD),
        );
    }

    vec![
        AttrsList::new(&plain),
        AttrsList::new(&plain.clone().weight(Weight::SEMIBOLD)),
        AttrsList::new(&spaced),
        AttrsList::new(&plain.clone().font_features(tabular)),
        with_span,
    ]
}

/// Every corpus line in every font and attribute list.
fn cases() -> Vec<(String, AttrsList)> {
    let mut cases = Vec::new();
    for (family, _) in FONTS {
        for text in CORPUS {
            for attrs_list in attrs_lists(family, text) {
                cases.push(((*text).to_string(), attrs_list));
            }
        }
    }
    cases
}

/// Every ordered pair of printable ASCII characters as a word, and a spread of
/// longer words, in every font: the kern-only path must agree with harfrust on
/// each pair, and on runs of pairs.
fn words() -> Vec<(String, AttrsList)> {
    let printable: Vec<char> = ('!'..='~').collect();
    let mut seed = 0x2545_F491_4F6C_DD1D_u64;
    let mut next = move |n: usize| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed % n as u64) as usize
    };
    let mut lines: Vec<String> = printable
        .iter()
        .map(|&a| {
            printable
                .iter()
                .map(|&b| format!("{a}{b}"))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    for _ in 0..60 {
        let line = (0..50)
            .map(|_| {
                let len = 3 + next(5);
                (0..len)
                    .map(|_| printable[next(printable.len())])
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join(" ");
        lines.push(line);
    }
    let mut cases = Vec::new();
    for (family, _) in FONTS {
        let attrs = AttrsList::new(&Attrs::new().family(Family::Name(family)));
        for line in &lines {
            cases.push((line.clone(), attrs.clone()));
        }
    }
    cases
}

fn shape_all(font_system: &mut FontSystem, cases: &[(String, AttrsList)]) -> Vec<Vec<Signature>> {
    cases
        .iter()
        .map(|(text, attrs_list)| shape(font_system, text, attrs_list))
        .collect()
}

fn assert_same(
    expected: &[Vec<Signature>],
    actual: &[Vec<Signature>],
    cases: &[(String, AttrsList)],
) {
    for ((e, a), (text, attrs_list)) in expected.iter().zip(actual).zip(cases) {
        assert_eq!(
            e,
            a,
            "{text:?} in {:?} shaped differently",
            attrs_list.defaults()
        );
    }
}

#[cfg(feature = "shape-run-cache")]
#[test]
fn cached_shaping_matches_uncached_shaping() {
    let cases = cases();
    let uncached = shape_all(&mut reference(), &cases);

    let mut font_system = configured(true, false);
    let cold = shape_all(&mut font_system, &cases);
    assert!(!font_system.shape_run_cache.is_empty());
    let warm = shape_all(&mut font_system, &cases);
    // Backwards too, so runs are fetched in a different order than they were stored.
    let mut backwards: Vec<_> = cases.iter().cloned().rev().collect::<Vec<_>>();
    let mut reversed = shape_all(&mut font_system, &backwards);
    reversed.reverse();
    backwards.reverse();

    assert_same(&uncached, &cold, &cases);
    assert_same(&uncached, &warm, &cases);
    assert_same(&uncached, &reversed, &backwards);
}

#[cfg(feature = "kern-fast-path")]
#[test]
fn the_kern_fast_path_matches_harfrust() {
    for cases in [cases(), words()] {
        let expected = shape_all(&mut reference(), &cases);
        let mut font_system = configured(false, true);
        let actual = shape_all(&mut font_system, &cases);
        assert_same(&expected, &actual, &cases);

        let (taken, declined) = font_system.kern_fast_path_counts();
        println!("fast path took {taken} runs and sent {declined} to harfrust");
        assert!(taken > declined, "the fast path should take most runs");
        // A table that found the font doing more than its analysis allowed turns
        // itself off, which keeps the output right but must not happen.
        for (family, _) in FONTS {
            let attrs = Attrs::new().family(Family::Name(family));
            let (chars, _) = font_system.kern_fast_path_rules(&attrs, true).unwrap();
            assert!(chars.contains('e'), "{family} gave up on the fast path");
        }
    }
}

#[cfg(all(feature = "shape-run-cache", feature = "kern-fast-path"))]
#[test]
fn the_cache_and_the_fast_path_together_match_harfrust() {
    let mut cases = cases();
    cases.extend(words());
    let expected = shape_all(&mut reference(), &cases);
    let mut font_system = configured(true, true);
    for _ in 0..2 {
        assert_same(&expected, &shape_all(&mut font_system, &cases), &cases);
    }
}

/// What each font sends to harfrust: the characters its GSUB or GPOS could act on
/// alone, and the adjacent pairs a ligature or contextual rule would start on.
#[cfg(feature = "kern-fast-path")]
#[test]
fn the_fast_path_leaves_substitutions_to_harfrust() {
    let printable: String = (' '..='~').collect();
    let mut font_system = configured(false, true);
    for (family, _) in FONTS {
        let attrs = Attrs::new().family(Family::Name(family));
        let (chars, pairs) = font_system.kern_fast_path_rules(&attrs, true).unwrap();
        let (script_less, _) = font_system.kern_fast_path_rules(&attrs, false).unwrap();
        let alone: String = printable.chars().filter(|&c| !chars.contains(c)).collect();
        println!(
            "{family}: harfrust shapes runs with any of {alone:?} or of the pairs {}",
            pairs.join(" ")
        );
        assert!(chars.contains('e') && chars.contains('W') && chars.contains('o'));
        assert!(script_less.chars().all(|c| !c.is_ascii_alphabetic()));
    }

    // Geist, Instrument Serif and Open Sans ligate "fi", Geist Mono draws "->" as
    // an arrow, and Inter's contextual alternates turn the x of "2x3" into a times.
    for (family, pair) in [
        ("Geist", "fi"),
        ("Instrument Serif", "fi"),
        ("Open Sans", "fi"),
        ("Geist Mono", "->"),
        ("Inter", "x3"),
    ] {
        let attrs = Attrs::new().family(Family::Name(family));
        let (chars, pairs) = font_system.kern_fast_path_rules(&attrs, true).unwrap();
        let [a, b] = [pair.as_bytes()[0] as char, pair.as_bytes()[1] as char];
        assert!(
            !(chars.contains(a) && chars.contains(b)) || pairs.iter().any(|p| p == pair),
            "{family} {pair}"
        );
    }
}

#[cfg(feature = "shape-run-cache")]
#[test]
fn a_bounded_cache_stays_within_its_bound_and_correct() {
    let cases = cases();
    let uncached = shape_all(&mut reference(), &cases);

    let mut font_system = configured(true, true);
    font_system.shape_run_cache.set_max_glyphs(300);
    for _ in 0..2 {
        let bounded = shape_all(&mut font_system, &cases);
        assert!(font_system.shape_run_cache.glyph_count() <= 300);
        assert_same(&uncached, &bounded, &cases);
    }
}

/// A mirrored character takes a different glyph in a right-to-left run, so a run
/// cached left to right must not be served to the same text right to left.
#[test]
fn the_same_text_right_to_left_is_a_different_run() {
    let attrs = AttrsList::new(&Attrs::new().family(Family::Name("Noto Sans Hebrew")));
    let lines = ["(", "שלום (", "שלום (עולם)", "(abc)", "שלום (abc) עולם"];

    let mut reference = reference();
    let expected: Vec<_> = lines
        .iter()
        .map(|line| shape(&mut reference, line, &attrs))
        .collect();

    let mut font_system = configured(true, true);
    for _ in 0..2 {
        for (line, expected) in lines.iter().zip(&expected) {
            assert_eq!(expected, &shape(&mut font_system, line, &attrs), "{line:?}");
        }
    }
}

/// Changing the font database drops every shaped run: a run shaped before a font
/// was loaded fell back to another font.
#[test]
fn loading_a_font_invalidates_what_was_shaped() {
    let mut db = fontdb::Database::new();
    db.load_font_data(FONTS[1].1.to_vec());
    let mut font_system = FontSystem::new_with_locale_and_db("en-US".into(), db);
    let attrs = AttrsList::new(&Attrs::new().family(Family::Name("Geist")));

    let before = shape(&mut font_system, "Kerning", &attrs);
    #[cfg(feature = "shape-run-cache")]
    assert!(!font_system.shape_run_cache.is_empty());

    font_system.db_mut().load_font_data(FONTS[0].1.to_vec());
    #[cfg(feature = "shape-run-cache")]
    assert!(font_system.shape_run_cache.is_empty());
    let after = shape(&mut font_system, "Kerning", &attrs);

    let geist = font_system
        .db()
        .faces()
        .find(|face| face.families.iter().any(|(name, _)| name == "Geist"))
        .unwrap()
        .id;
    assert_ne!(before[0].4, geist);
    assert!(after.iter().all(|glyph| glyph.4 == geist));
    assert_eq!(
        after,
        shape(
            &mut reference_with(FONTS[1].1, FONTS[0].1, geist),
            "Kerning",
            &attrs
        )
    );
}

/// A reference font system holding `first` and then `second`, whose face ids match
/// those of one that loaded `second` later.
fn reference_with(first: &[u8], second: &[u8], expected: fontdb::ID) -> FontSystem {
    let mut db = fontdb::Database::new();
    db.load_font_data(first.to_vec());
    db.load_font_data(second.to_vec());
    let mut font_system = FontSystem::new_with_locale_and_db("en-US".into(), db);
    assert!(font_system.db().face(expected).is_some());
    #[cfg(feature = "shape-run-cache")]
    font_system.shape_run_cache.set_max_glyphs(0);
    #[cfg(feature = "kern-fast-path")]
    font_system.set_kern_fast_path(false);
    font_system
}

/// Cached runs hold advances in em, so one run serves every font size: laid-out
/// glyphs match uncached shaping at each size, in every font.
#[test]
fn one_shaped_run_serves_every_size() {
    type Placed = (usize, u16, fontdb::ID, [u32; 4]);
    fn layout(font_system: &mut FontSystem, family: &'static str, size: f32) -> Vec<Placed> {
        let mut buffer = Buffer::new(font_system, Metrics::new(size, size * 1.25));
        buffer.set_wrap(font_system, Wrap::Word);
        buffer.set_size(font_system, Some(320.0), None);
        buffer.set_text(
            font_system,
            IPSUM,
            &Attrs::new().family(Family::Name(family)),
            Shaping::Advanced,
            None,
        );
        buffer.shape_until_scroll(font_system, false);
        buffer
            .layout_runs()
            .flat_map(|run| {
                run.glyphs.iter().map(move |g| {
                    (
                        g.start,
                        g.glyph_id,
                        g.font_id,
                        [
                            g.x.to_bits(),
                            g.y.to_bits(),
                            g.w.to_bits(),
                            run.line_y.to_bits(),
                        ],
                    )
                })
            })
            .collect()
    }

    let mut reference = reference();
    let mut font_system = configured(true, true);
    for (family, _) in FONTS {
        for size in [11.0, 14.0, 17.5, 32.0] {
            let expected = layout(&mut reference, family, size);
            assert_eq!(
                expected,
                layout(&mut font_system, family, size),
                "{family} at {size}px"
            );
        }
    }
}
