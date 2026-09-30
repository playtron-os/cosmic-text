// SPDX-License-Identifier: MIT OR Apache-2.0

//! A shortcut through advanced shaping for ASCII runs that a font does nothing to
//! but kern.
//!
//! For most Latin UI text, all that harfrust's GSUB and GPOS passes do is move
//! each glyph's advance by the kerning of it and its neighbours. When the font
//! proves that of a run, the run's glyphs are its characters' nominal glyphs, and
//! each glyph's position is its own plus what its pair with the next glyph and its
//! pair with the previous one add. Both come from harfrust itself: each character
//! is shaped once on its own, and each pair once, per font, so this path gives what
//! harfrust gives.
//!
//! The proof reads the font's GSUB and GPOS with `skrifa`, the parser harfrust is
//! built on, over every lookup harfrust could apply to Latin or script-less text.
//! A lookup that can only match a sequence of glyphs forbids the first adjacent
//! pair any such match needs; one that can act on a single glyph forbids its
//! character. A run takes this path when it has neither, so no substitution can
//! happen in it, and every positioning lookup that can act on it adjusts a pair of
//! adjacent glyphs by that pair alone, so its pairs add up. Everything else is
//! shaped by harfrust.

#![allow(clippy::too_many_arguments)]

#[cfg(not(feature = "std"))]
use alloc::{string::String, vec, vec::Vec};

use harfrust::{Direction, Script, UnicodeBuffer};
use skrifa::raw::tables::gpos::{PairPos, PositionSubtables, SinglePos};
use skrifa::raw::tables::gsub::{SingleSubst, SubstitutionSubtables};
use skrifa::raw::tables::layout::{
    ChainedSequenceContext, ClassDef, CoverageTable, FeatureList, FeatureVariations, LookupFlag,
    ScriptList, SequenceContext,
};
use skrifa::raw::types::{BigEndian, GlyphId16, Tag};
use skrifa::raw::{FontRef, ReadError, TableProvider};
use skrifa::MetadataProvider;

use crate::shape::{harfrust_features, set_cluster_ends, ShapePlans};
use crate::{Attrs, AttrsList, Font, FontFeatures, HashMap, ShapeGlyph};

/// The scripts harfrust may pick for Latin or script-less text without a
/// language: the text's own (`latn`, or `zzzz` for Unknown), then its fallbacks.
const SCRIPTS: [Tag; 4] = [
    Tag::new(b"latn"),
    Tag::new(b"zzzz"),
    Tag::new(b"DFLT"),
    Tag::new(b"dflt"),
];

/// The features harfrust turns on for horizontal left-to-right text in the
/// default shaper. `frac`, `numr` and `dnom` are left out: they only apply next
/// to a fraction slash, which is not ASCII.
const DEFAULT_FEATURES: [Tag; 23] = [
    Tag::new(b"rvrn"),
    Tag::new(b"ltra"),
    Tag::new(b"ltrm"),
    Tag::new(b"rand"),
    Tag::new(b"trak"),
    Tag::new(b"Harf"),
    Tag::new(b"HARF"),
    Tag::new(b"Buzz"),
    Tag::new(b"BUZZ"),
    Tag::new(b"abvm"),
    Tag::new(b"blwm"),
    Tag::new(b"ccmp"),
    Tag::new(b"locl"),
    Tag::new(b"mark"),
    Tag::new(b"mkmk"),
    Tag::new(b"rlig"),
    Tag::new(b"calt"),
    Tag::new(b"clig"),
    Tag::new(b"curs"),
    Tag::new(b"dist"),
    Tag::new(b"kern"),
    Tag::new(b"liga"),
    Tag::new(b"rclt"),
];

const KERN: Tag = Tag::new(b"kern");

/// Printable ASCII, by byte, as bits of a `u128`. A tab is shaped as a space.
const FIRST: u8 = b' ';
const SLOTS: usize = 95;

const fn slot(byte: u8) -> Option<usize> {
    match byte {
        b'\t' => Some(0),
        b' '..=b'~' => Some((byte - FIRST) as usize),
        _ => None,
    }
}

fn slots(set: u128) -> impl Iterator<Item = usize> {
    (0..SLOTS).filter(move |&slot| set & 1 << slot != 0)
}

fn character(slot: usize) -> char {
    char::from(FIRST + slot as u8)
}

/// The tables of the fonts runs were shaped with recently.
#[derive(Debug)]
pub(crate) struct KernTables {
    tables: Vec<KernTable>,
    enabled: bool,
    /// Runs shaped here, and runs sent on to harfrust.
    pub(crate) counts: (usize, usize),
}

impl Default for KernTables {
    fn default() -> Self {
        Self {
            tables: Vec::new(),
            enabled: true,
            counts: (0, 0),
        }
    }
}

impl KernTables {
    /// Enough for every face, weight and feature set a UI has on screen.
    const MAX_TABLES: usize = 32;

    pub(crate) fn clear(&mut self) {
        self.tables.clear();
    }

    pub(crate) fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        self.tables.clear();
        self.counts = (0, 0);
    }

    /// Shapes `line[start_run..end_run]`, whose attributes are `attrs`, in `font`
    /// into `glyphs` and returns true, or returns false without touching `glyphs`
    /// if the run needs full shaping.
    pub(crate) fn shape(
        &mut self,
        plans: &mut ShapePlans,
        glyphs: &mut Vec<ShapeGlyph>,
        font: &Font,
        attrs: &Attrs,
        line: &str,
        attrs_list: &AttrsList,
        start_run: usize,
        end_run: usize,
    ) -> bool {
        if !self.enabled {
            return false;
        }
        let shaped = self.try_shape(
            plans, glyphs, font, attrs, line, attrs_list, start_run, end_run,
        );
        if shaped {
            self.counts.0 += 1;
        } else {
            self.counts.1 += 1;
        }
        shaped
    }

    /// The printable ASCII characters runs in `font` with `attrs` may hold, and
    /// the adjacent pairs they may not, to take this path.
    pub(crate) fn describe(
        &mut self,
        plans: &mut ShapePlans,
        font: &Font,
        attrs: &Attrs,
        latin: bool,
    ) -> (String, Vec<String>) {
        let Some(table) = self.table(plans, font, attrs.weight, &attrs.font_features, latin) else {
            return (String::new(), Vec::new());
        };
        let chars = slots(table.chars).map(character).collect();
        let pairs = slots(table.chars)
            .flat_map(|a| {
                slots(table.forbidden[a] & table.chars)
                    .map(move |b| [character(a), character(b)].iter().collect())
            })
            .collect();
        (chars, pairs)
    }

    fn try_shape(
        &mut self,
        plans: &mut ShapePlans,
        glyphs: &mut Vec<ShapeGlyph>,
        font: &Font,
        attrs: &Attrs,
        line: &str,
        attrs_list: &AttrsList,
        start_run: usize,
        end_run: usize,
    ) -> bool {
        let text = &line.as_bytes()[start_run..end_run];
        if text.is_empty() || !text.iter().all(|&byte| slot(byte).is_some()) {
            return false;
        }
        let chars = || text.iter().filter_map(|&byte| slot(byte));
        let used = chars().fold(0_u128, |set, slot| set | 1 << slot);

        // harfrust gives a run the script of its first letter, and none without one.
        let latin = text.iter().any(u8::is_ascii_alphabetic);
        let Some(table) = self.table(plans, font, attrs.weight, &attrs.font_features, latin) else {
            return false;
        };
        if used & !table.chars != 0 {
            return false;
        }
        for (a, b) in chars().zip(chars().skip(1)) {
            if !table.allowed(a, b) {
                return false;
            }
            if table.rows & (1 << a | 1 << b) == 0 && !table.fill_row(plans, font, a) {
                return false;
            }
        }

        let glyph_start = glyphs.len();
        let mut prev: Option<usize> = None;
        let mut nexts = chars().skip(1);
        for (i, this) in chars().enumerate() {
            let [mut x_advance, mut x_offset, mut y_offset] = table.base[this];
            if let Some(next) = nexts.next() {
                let delta = table.delta(this, next);
                x_advance += delta[0];
                x_offset += delta[1];
                y_offset += delta[2];
            }
            if let Some(prev) = prev {
                let delta = table.delta(prev, this);
                x_advance += delta[3];
                x_offset += delta[4];
                y_offset += delta[5];
            }
            glyphs.push(ShapeGlyph::shaped(
                font,
                attrs_list,
                start_run + i,
                end_run,
                table.glyphs[this],
                [x_advance, 0, x_offset, y_offset],
            ));
            prev = Some(this);
        }
        set_cluster_ends(&mut glyphs[glyph_start..], false);
        true
    }

    fn table(
        &mut self,
        plans: &mut ShapePlans,
        font: &Font,
        weight: fontdb::Weight,
        features: &FontFeatures,
        latin: bool,
    ) -> Option<&mut KernTable> {
        let found = self.tables.iter().position(|table| {
            table.font_id == font.id()
                && table.weight == weight
                && table.latin == latin
                && table.features == *features
        });
        let index = match found {
            Some(index) => index,
            None => {
                if self.tables.len() >= Self::MAX_TABLES {
                    self.tables.remove(0);
                }
                self.tables
                    .push(KernTable::new(plans, font, weight, features.clone(), latin));
                self.tables.len() - 1
            }
        };
        let table = &mut self.tables[index];
        (table.chars != 0).then_some(table)
    }
}

/// Everything shaping ASCII in one font, weight and feature set comes down to.
struct KernTable {
    font_id: fontdb::ID,
    weight: fontdb::Weight,
    features: FontFeatures,
    latin: bool,
    /// Positioned by a legacy `kern` table, which splits each pair's value
    /// between both glyphs, rather than by GPOS, which adjusts the first.
    legacy: bool,
    /// The characters this table shapes. Zero when it shapes none.
    chars: u128,
    /// For each character, the characters that may not follow it.
    forbidden: [u128; SLOTS],
    glyphs: [u16; SLOTS],
    /// Each character's advance and offsets on its own, in font units.
    base: [[i32; 3]; SLOTS],
    /// The characters whose pairs with every other character are known.
    rows: u128,
    /// An index into `deltas` for each ordered pair of characters.
    pairs: Vec<u16>,
    /// What a pair adds to its first glyph's advance and offsets, then to its
    /// second's. The first entry is all zeros.
    deltas: Vec<[i32; 6]>,
    delta_index: HashMap<[i32; 6], u16>,
}

impl core::fmt::Debug for KernTable {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("KernTable")
            .field("font_id", &self.font_id)
            .field("latin", &self.latin)
            .field("chars", &self.chars)
            .finish_non_exhaustive()
    }
}

impl KernTable {
    fn new(
        plans: &mut ShapePlans,
        font: &Font,
        weight: fontdb::Weight,
        features: FontFeatures,
        latin: bool,
    ) -> Self {
        let mut table = Self {
            font_id: font.id(),
            weight,
            features,
            latin,
            legacy: false,
            chars: 0,
            forbidden: [0; SLOTS],
            glyphs: [0; SLOTS],
            base: [[0; 3]; SLOTS],
            rows: 0,
            pairs: Vec::new(),
            deltas: Vec::new(),
            delta_index: HashMap::default(),
        };
        let Some(font_ref) = font.font_ref() else {
            return table;
        };
        let Ok(Some(proof)) = prove(&font_ref, &table.features) else {
            return table;
        };
        table.legacy = proof.legacy;
        table.forbidden = proof.forbidden;
        // Without a script, a run holds no letters.
        let candidates = if latin {
            proof.chars
        } else {
            slots(proof.chars)
                .filter(|&slot| !character(slot).is_ascii_alphabetic())
                .fold(0, |set, slot| set | 1 << slot)
        };

        // Each character on its own gives its glyph and unkerned position.
        let charmap = font_ref.charmap();
        for slot in slots(candidates) {
            let Some(shaped) = table.shape(plans, font, &[slot]) else {
                continue;
            };
            let [(glyph_id, [x_advance, y_advance, x_offset, y_offset])] = shaped[..] else {
                continue;
            };
            let nominal = charmap.map(character(slot)).map(|glyph| glyph.to_u32());
            if glyph_id == 0 || nominal != Some(u32::from(glyph_id)) || y_advance != 0 {
                continue;
            }
            table.glyphs[slot] = glyph_id;
            table.base[slot] = [x_advance, x_offset, y_offset];
            table.chars |= 1 << slot;
        }
        table.pairs = vec![0; SLOTS * SLOTS];
        table.deltas = vec![[0; 6]];
        table.delta_index.insert([0; 6], 0);
        table
    }

    fn script(&self) -> Script {
        if self.latin {
            harfrust::script::LATIN
        } else {
            harfrust::script::UNKNOWN
        }
    }

    fn allowed(&self, first: usize, second: usize) -> bool {
        self.forbidden[first] & 1 << second == 0
    }

    /// Shapes `text` as a run with this table's script and features is shaped,
    /// returning each glyph's id and position, or nothing if the glyphs are not the
    /// characters one to one.
    fn shape(
        &self,
        plans: &mut ShapePlans,
        font: &Font,
        text: &[usize],
    ) -> Option<Vec<(u16, [i32; 4])>> {
        let features = harfrust_features(&self.features);
        let plan = plans.get(font, Direction::LeftToRight, self.script(), None, &features);
        let mut buffer = UnicodeBuffer::new();
        // ASCII: a byte offset, which harfrust's clusters are, is a character index.
        let string: String = text.iter().map(|&slot| character(slot)).collect();
        buffer.push_str(&string);
        buffer.set_direction(Direction::LeftToRight);
        buffer.set_script(self.script());
        let shaped = font.shaper().shape_with_plan(plan, buffer, &features);
        if shaped.len() != text.len() {
            return None;
        }
        shaped
            .glyph_infos()
            .iter()
            .zip(shaped.glyph_positions())
            .enumerate()
            .map(|(i, (info, pos))| {
                let glyph_id = u16::try_from(info.glyph_id).ok()?;
                (info.cluster as usize == i).then_some((
                    glyph_id,
                    [pos.x_advance, pos.y_advance, pos.x_offset, pos.y_offset],
                ))
            })
            .collect()
    }

    fn delta(&self, first: usize, second: usize) -> [i32; 6] {
        self.deltas[usize::from(self.pairs[first * SLOTS + second])]
    }

    /// Learns every allowed pair `slot` is in. Where both orders are allowed it
    /// shapes `slot` around each character in turn, `a b a c a ... a`, and each
    /// remaining pair on its own. Returns false, and disables the table, if what
    /// comes back is not the sum of independent pairs.
    fn fill_row(&mut self, plans: &mut ShapePlans, font: &Font, slot: usize) -> bool {
        let mut runs = vec![vec![slot]];
        for other in slots(self.chars) {
            match (self.allowed(slot, other), self.allowed(other, slot)) {
                (true, true) => runs[0].extend([other, slot]),
                (true, false) => runs.push(vec![slot, other]),
                (false, true) => runs.push(vec![other, slot]),
                (false, false) => {}
            }
        }

        let mut learned: HashMap<(usize, usize), [Option<[i32; 3]>; 2]> = HashMap::default();
        let consistent = runs.iter().all(|run| {
            let Some(shaped) = self.shape(plans, font, run) else {
                return false;
            };
            for (k, (&this, (glyph_id, [x_advance, y_advance, x_offset, y_offset]))) in
                run.iter().zip(shaped).enumerate()
            {
                if glyph_id != self.glyphs[this] || y_advance != 0 {
                    return false;
                }
                let base = self.base[this];
                let d = [x_advance - base[0], x_offset - base[1], y_offset - base[2]];
                // What this glyph owes to its pair with the next glyph, and to its pair
                // with the previous one.
                let (to_next, from_prev) = if self.legacy {
                    // A legacy kern moves the second glyph's advance and offset alike.
                    if d[2] != 0 {
                        return false;
                    }
                    ([d[0] - d[1], 0, 0], [d[1], d[1], 0])
                } else {
                    (d, [0, 0, 0])
                };
                let halves = [
                    (k.checked_sub(1).map(|p| (run[p], this)), from_prev, 1),
                    (run.get(k + 1).map(|&n| (this, n)), to_next, 0),
                ];
                for (pair, part, half) in halves {
                    match pair {
                        None if part != [0, 0, 0] => return false,
                        None => {}
                        Some(pair) => {
                            let known = &mut learned.entry(pair).or_default()[half];
                            if known.is_some_and(|known| known != part) {
                                return false;
                            }
                            *known = Some(part);
                        }
                    }
                }
            }
            true
        });

        let consistent = consistent
            && learned.into_iter().all(|((first, second), [head, tail])| {
                let (Some(head), Some(tail)) = (head, tail) else {
                    return false;
                };
                let delta = [head[0], head[1], head[2], tail[0], tail[1], tail[2]];
                let known = self.rows & (1 << first | 1 << second) != 0;
                if known && self.delta(first, second) != delta {
                    return false;
                }
                let Some(index) = self.intern(delta) else {
                    return false;
                };
                self.pairs[first * SLOTS + second] = index;
                true
            });

        if consistent {
            self.rows |= 1 << slot;
        } else {
            self.chars = 0;
        }
        consistent
    }

    fn intern(&mut self, delta: [i32; 6]) -> Option<u16> {
        if let Some(&index) = self.delta_index.get(&delta) {
            return Some(index);
        }
        let index = u16::try_from(self.deltas.len()).ok()?;
        self.deltas.push(delta);
        self.delta_index.insert(delta, index);
        Some(index)
    }
}

/// What reading a font's layout tables proved about ASCII runs in it.
struct Proof {
    /// The characters a run may hold.
    chars: u128,
    /// For each character, the characters that may not follow it.
    forbidden: [u128; SLOTS],
    /// Whether the font kerns with a legacy `kern` table.
    legacy: bool,
}

/// Reads what harfrust could do to printable ASCII in the font, or `None` when
/// it cannot be told: AAT layout, or a mix of GPOS and legacy kerning.
fn prove(font: &FontRef, features: &FontFeatures) -> Result<Option<Proof>, ReadError> {
    let has = |tag: &[u8; 4]| font.table_data(Tag::new(tag)).is_some();
    if has(b"morx") || has(b"mort") || has(b"kerx") || has(b"trak") {
        return Ok(None);
    }

    // The nominal glyph of each character that is a base glyph or has no class;
    // lookup flags skip marks, ligatures and components.
    let charmap = font.charmap();
    let classes = font
        .gdef()
        .ok()
        .and_then(|gdef| gdef.glyph_class_def())
        .transpose()?;
    let mut alphabet: Vec<(u16, u128)> = Vec::new();
    for slot in 0..SLOTS {
        let Some(glyph) = charmap
            .map(character(slot))
            .and_then(|glyph| u16::try_from(glyph.to_u32()).ok())
        else {
            continue;
        };
        if glyph == 0
            || classes
                .as_ref()
                .is_some_and(|c| c.get(GlyphId16::new(glyph)) > 1)
        {
            continue;
        }
        match alphabet.iter_mut().find(|(g, _)| *g == glyph) {
            Some((_, set)) => *set |= 1 << slot,
            None => alphabet.push((glyph, 1 << slot)),
        }
    }
    let mut proof = Rules {
        alphabet: &alphabet,
        skips_bases: false,
        chars: 0,
        forbidden: [0; SLOTS],
    };

    let mut gpos_kern_everywhere = false;
    let mut gpos_kern_somewhere = false;
    let mut gpos_pairs = Vec::new();
    if let Ok(gpos) = font.gpos() {
        let selection = select(
            gpos.script_list()?,
            gpos.feature_list()?,
            gpos.feature_variations().transpose()?,
            features,
        )?;
        gpos_kern_everywhere = selection.systems > 0 && selection.with_kern == selection.systems;
        gpos_kern_somewhere = selection.with_kern > 0;
        let lookups = gpos.lookup_list()?;
        for index in selection.lookups {
            let lookup = lookups.lookups().get(usize::from(index))?;
            proof.skips_bases = lookup
                .lookup_flag()
                .contains(LookupFlag::IGNORE_BASE_GLYPHS);
            match lookup.subtables()? {
                PositionSubtables::Pair(subtables) => {
                    for subtable in subtables.iter() {
                        let (coverage, second_record) = match subtable? {
                            PairPos::Format1(t) => (t.coverage()?, !t.value_format2().is_empty()),
                            PairPos::Format2(t) => (t.coverage()?, !t.value_format2().is_empty()),
                        };
                        // A second value record makes the lookup skip the pair's second
                        // glyph, and skipping base glyphs pairs glyphs that are apart.
                        if second_record || proof.skips_bases {
                            proof.single(&coverage);
                        } else {
                            gpos_pairs.push(coverage);
                        }
                    }
                }
                PositionSubtables::Single(subtables) => {
                    for subtable in subtables.iter() {
                        proof.single(&match subtable? {
                            SinglePos::Format1(t) => t.coverage()?,
                            SinglePos::Format2(t) => t.coverage()?,
                        });
                    }
                }
                PositionSubtables::Cursive(subtables) => {
                    for subtable in subtables.iter() {
                        proof.single(&subtable?.coverage()?);
                    }
                }
                PositionSubtables::MarkToBase(subtables) => {
                    for subtable in subtables.iter() {
                        proof.single(&subtable?.mark_coverage()?);
                    }
                }
                PositionSubtables::MarkToLig(subtables) => {
                    for subtable in subtables.iter() {
                        proof.single(&subtable?.mark_coverage()?);
                    }
                }
                PositionSubtables::MarkToMark(subtables) => {
                    for subtable in subtables.iter() {
                        proof.single(&subtable?.mark1_coverage()?);
                    }
                }
                PositionSubtables::Contextual(subtables) => {
                    for subtable in subtables.iter() {
                        proof.context(&subtable?)?;
                    }
                }
                PositionSubtables::ChainContextual(subtables) => {
                    for subtable in subtables.iter() {
                        proof.chain_context(&subtable?)?;
                    }
                }
            }
        }
    }

    // harfrust applies a legacy `kern` table only where GPOS has no `kern` feature.
    let legacy = match font.kern() {
        Err(_) => false,
        Ok(_) if gpos_kern_everywhere => false,
        Ok(_) if gpos_kern_somewhere => return Ok(None),
        Ok(kern) => {
            for subtable in kern.subtables() {
                let subtable = subtable?;
                if subtable.is_horizontal()
                    && (subtable.is_cross_stream() || subtable.is_state_machine())
                {
                    return Ok(None);
                }
            }
            true
        }
    };
    // GPOS pairs and a legacy table's would both move the same glyphs.
    if legacy {
        proof.skips_bases = false;
        for coverage in &gpos_pairs {
            proof.single(coverage);
        }
    }

    if let Ok(gsub) = font.gsub() {
        let selection = select(
            gsub.script_list()?,
            gsub.feature_list()?,
            gsub.feature_variations().transpose()?,
            features,
        )?;
        let lookups = gsub.lookup_list()?;
        for index in selection.lookups {
            let lookup = lookups.lookups().get(usize::from(index))?;
            proof.skips_bases = lookup
                .lookup_flag()
                .contains(LookupFlag::IGNORE_BASE_GLYPHS);
            match lookup.subtables()? {
                SubstitutionSubtables::Single(subtables) => {
                    for subtable in subtables.iter() {
                        proof.single(&match subtable? {
                            SingleSubst::Format1(t) => t.coverage()?,
                            SingleSubst::Format2(t) => t.coverage()?,
                        });
                    }
                }
                SubstitutionSubtables::Multiple(subtables) => {
                    for subtable in subtables.iter() {
                        proof.single(&subtable?.coverage()?);
                    }
                }
                SubstitutionSubtables::Alternate(subtables) => {
                    for subtable in subtables.iter() {
                        proof.single(&subtable?.coverage()?);
                    }
                }
                SubstitutionSubtables::Ligature(subtables) => {
                    for subtable in subtables.iter() {
                        let subtable = subtable?;
                        let coverage = subtable.coverage()?;
                        for &(glyph, first) in proof.alphabet {
                            let Some(index) = coverage.get(GlyphId16::new(glyph)) else {
                                continue;
                            };
                            let set = subtable.ligature_sets().get(usize::from(index))?;
                            for ligature in set.ligatures().iter() {
                                let components = ligature?.component_glyph_ids();
                                if proof.all(components) {
                                    let next = components.first().map(|g| proof.of(g.get()));
                                    proof.rule(first, next, None);
                                }
                            }
                        }
                    }
                }
                SubstitutionSubtables::Contextual(subtables) => {
                    for subtable in subtables.iter() {
                        proof.context(&subtable?)?;
                    }
                }
                SubstitutionSubtables::ChainContextual(subtables) => {
                    for subtable in subtables.iter() {
                        proof.chain_context(&subtable?)?;
                    }
                }
                SubstitutionSubtables::Reverse(subtables) => {
                    for subtable in subtables.iter() {
                        let subtable = subtable?;
                        if proof.every(subtable.backtrack_coverages().iter())?
                            && proof.every(subtable.lookahead_coverages().iter())?
                        {
                            proof.single(&subtable.coverage()?);
                        }
                    }
                }
            }
        }
    }

    let chars = alphabet.iter().fold(0, |set, (_, slots)| set | slots) & !proof.chars;
    Ok(Some(Proof {
        chars,
        forbidden: proof.forbidden,
        legacy,
    }))
}

/// The lookups harfrust could apply, and how many of the language systems they
/// come from turn on `kern`.
#[derive(Default)]
struct Selection {
    lookups: Vec<u16>,
    systems: usize,
    with_kern: usize,
}

fn enabled(tag: Tag, features: &FontFeatures) -> bool {
    // As in harfrust, the last setting of a feature wins over the default.
    match features
        .features
        .iter()
        .rev()
        .find(|feature| feature.tag.as_bytes() == &tag.to_be_bytes())
    {
        Some(feature) => feature.value != 0,
        None => DEFAULT_FEATURES.contains(&tag),
    }
}

fn select(
    scripts: ScriptList,
    feature_list: FeatureList,
    variations: Option<FeatureVariations>,
    features: &FontFeatures,
) -> Result<Selection, ReadError> {
    let mut selection = Selection::default();
    for record in scripts.script_records() {
        if !SCRIPTS.contains(&record.script_tag()) {
            continue;
        }
        let script = record.script(scripts.offset_data())?;
        // Without a language harfrust takes a language system tagged `dflt`, else
        // the script's default one; take both.
        let mut systems = Vec::new();
        if let Some(system) = script.default_lang_sys() {
            systems.push(system?);
        }
        for record in script.lang_sys_records() {
            if record.lang_sys_tag() == Tag::new(b"dflt") {
                systems.push(record.lang_sys(script.offset_data())?);
            }
        }
        for system in systems {
            selection.systems += 1;
            let required = system.required_feature_index();
            let mut indices: Vec<u16> = system
                .feature_indices()
                .iter()
                .map(BigEndian::get)
                .collect();
            if required != 0xFFFF {
                indices.push(required);
            }
            let mut with_kern = false;
            for index in indices {
                let record = feature_list
                    .feature_records()
                    .get(usize::from(index))
                    .ok_or(ReadError::OutOfBounds)?;
                let tag = record.feature_tag();
                if index != required && !enabled(tag, features) {
                    continue;
                }
                with_kern |= tag == KERN && index != required;
                let feature = record.feature(feature_list.offset_data())?;
                selection
                    .lookups
                    .extend(feature.lookup_list_indices().iter().map(BigEndian::get));
                // A variable font may swap the feature's lookups at some instance.
                let Some(variations) = &variations else {
                    continue;
                };
                for variation in variations.feature_variation_records() {
                    let Some(substitution) =
                        variation.feature_table_substitution(variations.offset_data())
                    else {
                        continue;
                    };
                    let substitution = substitution?;
                    for record in substitution.substitutions() {
                        if record.feature_index() == index {
                            let alternate = record.alternate_feature(substitution.offset_data())?;
                            selection
                                .lookups
                                .extend(alternate.lookup_list_indices().iter().map(BigEndian::get));
                        }
                    }
                }
            }
            if with_kern {
                selection.with_kern += 1;
            }
        }
    }
    selection.lookups.sort_unstable();
    selection.lookups.dedup();
    Ok(selection)
}

/// What the lookups read so far rule out, over runs made of the alphabet.
struct Rules<'a> {
    /// Each candidate glyph and the characters that map to it.
    alphabet: &'a [(u16, u128)],
    /// Whether the lookup being read skips base glyphs, so that a match need not
    /// be made of adjacent glyphs.
    skips_bases: bool,
    /// The characters no run may hold.
    chars: u128,
    /// For each character, the characters that may not follow it.
    forbidden: [u128; SLOTS],
}

impl Rules<'_> {
    /// The characters whose glyph is `glyph`.
    fn of(&self, glyph: GlyphId16) -> u128 {
        let glyph = glyph.to_u16();
        self.alphabet
            .iter()
            .find(|&&(g, _)| g == glyph)
            .map_or(0, |&(_, set)| set)
    }

    fn all(&self, glyphs: &[BigEndian<GlyphId16>]) -> bool {
        glyphs.iter().all(|glyph| self.of(glyph.get()) != 0)
    }

    /// The characters whose glyph is in `coverage`.
    fn covered(&self, coverage: &CoverageTable) -> u128 {
        self.alphabet
            .iter()
            .filter(|&&(glyph, _)| coverage.get(GlyphId16::new(glyph)).is_some())
            .fold(0, |acc, &(_, set)| acc | set)
    }

    /// The characters whose glyph `class_def` puts in `class`.
    fn in_class(&self, class_def: &ClassDef, class: u16) -> u128 {
        self.alphabet
            .iter()
            .filter(|&&(glyph, _)| class_def.get(GlyphId16::new(glyph)) == class)
            .fold(0, |acc, &(_, set)| acc | set)
    }

    /// The characters in each coverage.
    fn sets<'b>(
        &self,
        coverages: impl Iterator<Item = Result<CoverageTable<'b>, ReadError>>,
    ) -> Result<Vec<u128>, ReadError> {
        coverages
            .map(|coverage| Ok(self.covered(&coverage?)))
            .collect()
    }

    fn every<'b>(
        &self,
        mut coverages: impl Iterator<Item = Result<CoverageTable<'b>, ReadError>>,
    ) -> Result<bool, ReadError> {
        coverages.try_fold(true, |all, coverage| {
            Ok(all && self.covered(&coverage?) != 0)
        })
    }

    /// A lookup acts on the covered glyphs alone.
    fn single(&mut self, coverage: &CoverageTable) {
        self.chars |= self.covered(coverage);
    }

    /// A match starts on `first` and needs `next` right after it, or `prev` right
    /// before it, or neither. Only the pair is ruled out, unless the lookup skips
    /// base glyphs and so need not match adjacent ones.
    fn rule(&mut self, first: u128, next: Option<u128>, prev: Option<u128>) {
        match (next, prev) {
            _ if self.skips_bases => self.chars |= first,
            (Some(next), _) => {
                for a in slots(first) {
                    self.forbidden[a] |= next;
                }
            }
            (None, Some(prev)) => {
                for a in slots(prev) {
                    self.forbidden[a] |= first;
                }
            }
            (None, None) => self.chars |= first,
        }
    }

    fn context(&mut self, context: &SequenceContext) -> Result<(), ReadError> {
        match context {
            SequenceContext::Format1(t) => {
                let coverage = t.coverage()?;
                for &(glyph, first) in self.alphabet {
                    let Some(index) = coverage.get(GlyphId16::new(glyph)) else {
                        continue;
                    };
                    let Some(set) = t.seq_rule_sets().get(usize::from(index)).transpose()? else {
                        continue;
                    };
                    for rule in set.seq_rules().iter() {
                        let input = rule?.input_sequence();
                        if self.all(input) {
                            let next = input.first().map(|g| self.of(g.get()));
                            self.rule(first, next, None);
                        }
                    }
                }
            }
            SequenceContext::Format2(t) => {
                let coverage = t.coverage()?;
                let class_def = t.class_def()?;
                for &(glyph, first) in self.alphabet {
                    if coverage.get(GlyphId16::new(glyph)).is_none() {
                        continue;
                    }
                    let class = class_def.get(GlyphId16::new(glyph));
                    let Some(set) = t
                        .class_seq_rule_sets()
                        .get(usize::from(class))
                        .transpose()?
                    else {
                        continue;
                    };
                    for rule in set.class_seq_rules().iter() {
                        let input: Vec<u128> = rule?
                            .input_sequence()
                            .iter()
                            .map(|c| self.in_class(&class_def, c.get()))
                            .collect();
                        if input.iter().all(|&set| set != 0) {
                            self.rule(first, input.first().copied(), None);
                        }
                    }
                }
            }
            SequenceContext::Format3(t) => {
                let input = self.sets(t.coverages().iter())?;
                if input.iter().all(|&set| set != 0) {
                    self.rule(input[0], input.get(1).copied(), None);
                }
            }
        }
        Ok(())
    }

    fn chain_context(&mut self, context: &ChainedSequenceContext) -> Result<(), ReadError> {
        // The glyph after the first, which is the next input glyph or else the first
        // of the lookahead; else the glyph before, the first of the backtrack.
        let neighbours = |input: &[u128], lookahead: &[u128], backtrack: &[u128]| {
            let next = input.first().or(lookahead.first()).copied();
            let prev = backtrack.first().copied();
            (next, prev)
        };
        match context {
            ChainedSequenceContext::Format1(t) => {
                let coverage = t.coverage()?;
                for &(glyph, first) in self.alphabet {
                    let Some(index) = coverage.get(GlyphId16::new(glyph)) else {
                        continue;
                    };
                    let Some(set) = t
                        .chained_seq_rule_sets()
                        .get(usize::from(index))
                        .transpose()?
                    else {
                        continue;
                    };
                    for rule in set.chained_seq_rules().iter() {
                        let rule = rule?;
                        let sets = |glyphs: &[BigEndian<GlyphId16>]| -> Vec<u128> {
                            glyphs.iter().map(|g| self.of(g.get())).collect()
                        };
                        let input = sets(rule.input_sequence());
                        let lookahead = sets(rule.lookahead_sequence());
                        let backtrack = sets(rule.backtrack_sequence());
                        if [&input, &lookahead, &backtrack]
                            .iter()
                            .all(|sets| sets.iter().all(|&set| set != 0))
                        {
                            let (next, prev) = neighbours(&input, &lookahead, &backtrack);
                            self.rule(first, next, prev);
                        }
                    }
                }
            }
            ChainedSequenceContext::Format2(t) => {
                let coverage = t.coverage()?;
                let input_classes = t.input_class_def()?;
                let backtrack_classes = t.backtrack_class_def()?;
                let lookahead_classes = t.lookahead_class_def()?;
                for &(glyph, first) in self.alphabet {
                    if coverage.get(GlyphId16::new(glyph)).is_none() {
                        continue;
                    }
                    let class = input_classes.get(GlyphId16::new(glyph));
                    let Some(set) = t
                        .chained_class_seq_rule_sets()
                        .get(usize::from(class))
                        .transpose()?
                    else {
                        continue;
                    };
                    for rule in set.chained_class_seq_rules().iter() {
                        let rule = rule?;
                        let sets =
                            |classes: &[BigEndian<u16>], class_def: &ClassDef| -> Vec<u128> {
                                classes
                                    .iter()
                                    .map(|c| self.in_class(class_def, c.get()))
                                    .collect()
                            };
                        let input = sets(rule.input_sequence(), &input_classes);
                        let lookahead = sets(rule.lookahead_sequence(), &lookahead_classes);
                        let backtrack = sets(rule.backtrack_sequence(), &backtrack_classes);
                        if [&input, &lookahead, &backtrack]
                            .iter()
                            .all(|sets| sets.iter().all(|&set| set != 0))
                        {
                            let (next, prev) = neighbours(&input, &lookahead, &backtrack);
                            self.rule(first, next, prev);
                        }
                    }
                }
            }
            ChainedSequenceContext::Format3(t) => {
                let input = self.sets(t.input_coverages().iter())?;
                let lookahead = self.sets(t.lookahead_coverages().iter())?;
                let backtrack = self.sets(t.backtrack_coverages().iter())?;
                if !input.is_empty()
                    && [&input, &lookahead, &backtrack]
                        .iter()
                        .all(|sets| sets.iter().all(|&set| set != 0))
                {
                    let (next, prev) = neighbours(&input[1..], &lookahead, &backtrack);
                    self.rule(input[0], next, prev);
                }
            }
        }
        Ok(())
    }
}
