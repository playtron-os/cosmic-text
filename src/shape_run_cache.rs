#[cfg(not(feature = "std"))]
use alloc::{string::String, vec::Vec};
use core::hash::{Hash, Hasher};
use core::ops::Range;

use crate::{AttrsList, AttrsOwned, HashMap, ShapeGlyph};

/// Key for caching shape runs.
///
/// Everything that decides a run's glyphs: its text, the attributes over it and the
/// direction it is shaped in. The glyphs are stored in em units and relative to the
/// run, so the same key is valid at any font size and at any offset in a line.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct ShapeRunKey {
    pub text: String,
    pub default_attrs: AttrsOwned,
    pub attrs_spans: Vec<(Range<usize>, AttrsOwned)>,
    /// Whether the run is shaped right to left, where mirrored characters such as
    /// `(` take different glyphs.
    pub rtl: bool,
}

impl ShapeRunKey {
    /// The key for `line[run]` shaped with `attrs_list`.
    pub fn new(line: &str, attrs_list: &AttrsList, run: Range<usize>, rtl: bool) -> Self {
        Self {
            text: String::from(&line[run.clone()]),
            default_attrs: attrs_list.defaults_owned().clone(),
            attrs_spans: run_spans(attrs_list, &run)
                .map(|(range, attrs)| (range, attrs.clone()))
                .collect(),
            rtl,
        }
    }

    fn hash_value(&self) -> u64 {
        hash_parts(
            &self.text,
            &self.default_attrs,
            self.attrs_spans
                .iter()
                .map(|(range, attrs)| (range.clone(), attrs)),
            self.rtl,
        )
    }

    #[cfg(feature = "shape-run-cache")]
    fn matches(&self, line: &str, attrs_list: &AttrsList, run: &Range<usize>, rtl: bool) -> bool {
        self.rtl == rtl
            && self.text == line[run.clone()]
            && self.default_attrs == *attrs_list.defaults_owned()
            && self
                .attrs_spans
                .iter()
                .map(|(range, attrs)| (range.clone(), attrs))
                .eq(run_spans(attrs_list, run))
    }
}

/// The spans over `run` that differ from the defaults, relative to the run.
fn run_spans<'a>(
    attrs_list: &'a AttrsList,
    run: &Range<usize>,
) -> impl Iterator<Item = (Range<usize>, &'a AttrsOwned)> + 'a {
    let (start, end) = (run.start, run.end);
    let defaults = attrs_list.defaults_owned();
    attrs_list
        .spans
        .overlapping(run.clone())
        .filter(move |(_, attrs)| *attrs != defaults)
        .filter_map(move |(range, attrs)| {
            let from = range.start.max(start) - start;
            let to = range.end.min(end).saturating_sub(start);
            (to > from).then_some((from..to, attrs))
        })
}

fn hash_parts<'a>(
    text: &str,
    defaults: &AttrsOwned,
    spans: impl Iterator<Item = (Range<usize>, &'a AttrsOwned)>,
    rtl: bool,
) -> u64 {
    let mut hasher = rustc_hash::FxHasher::default();
    text.hash(&mut hasher);
    defaults.hash(&mut hasher);
    for (range, attrs) in spans {
        range.hash(&mut hasher);
        attrs.hash(&mut hasher);
    }
    rtl.hash(&mut hasher);
    hasher.finish()
}

#[derive(Clone)]
struct Entry {
    key: ShapeRunKey,
    glyphs: Vec<ShapeGlyph>,
    /// The [`ShapeRunCache::trim`] generation the entry was last used in.
    age: u64,
    /// When the entry was last used, for evicting the least recently used.
    tick: u64,
}

/// A cache of shaped runs, which are words or the parts of a word that share
/// attributes.
///
/// Bounded: it holds at most [`Self::max_glyphs`] glyphs and drops the least
/// recently used half of its runs when it would exceed that. [`crate::FontSystem`]
/// clears it whenever its font database is changed.
#[derive(Clone)]
pub struct ShapeRunCache {
    age: u64,
    tick: u64,
    glyphs: usize,
    max_glyphs: usize,
    // Keyed by the key's hash so a lookup needs no allocation; the entry holds the
    // key itself to rule out collisions.
    cache: HashMap<u64, Entry>,
}

impl Default for ShapeRunCache {
    fn default() -> Self {
        Self {
            age: 0,
            tick: 0,
            glyphs: 0,
            max_glyphs: Self::DEFAULT_MAX_GLYPHS,
            cache: HashMap::default(),
        }
    }
}

impl ShapeRunCache {
    /// The default for [`Self::max_glyphs`]: 16384 glyphs, at most about 3 MB.
    pub const DEFAULT_MAX_GLYPHS: usize = 16 * 1024;

    /// Runs longer than this many bytes are shaped every time rather than cached:
    /// long unbroken runs (paths, hashes, links) rarely repeat.
    pub const MAX_RUN_LEN: usize = 128;

    /// The most glyphs the cache holds.
    pub const fn max_glyphs(&self) -> usize {
        self.max_glyphs
    }

    /// Sets the most glyphs the cache holds, evicting to fit. Zero disables it.
    pub fn set_max_glyphs(&mut self, max_glyphs: usize) {
        self.max_glyphs = max_glyphs;
        self.evict();
    }

    /// The number of cached runs.
    pub fn len(&self) -> usize {
        self.cache.len()
    }

    /// Whether the cache holds no runs.
    pub fn is_empty(&self) -> bool {
        self.cache.is_empty()
    }

    /// The number of cached glyphs.
    pub const fn glyph_count(&self) -> usize {
        self.glyphs
    }

    /// Drops every cached run.
    pub fn clear(&mut self) {
        self.cache.clear();
        self.glyphs = 0;
    }

    /// Get cache item, updating age if found
    pub fn get(&mut self, key: &ShapeRunKey) -> Option<&Vec<ShapeGlyph>> {
        let (age, tick) = (self.age, self.next_tick());
        self.cache
            .get_mut(&key.hash_value())
            .filter(|entry| entry.key == *key)
            .map(|entry| {
                entry.age = age;
                entry.tick = tick;
                &entry.glyphs
            })
    }

    /// The glyphs cached for `line[run]`, relative to the run.
    #[cfg(feature = "shape-run-cache")]
    pub(crate) fn lookup(
        &mut self,
        line: &str,
        attrs_list: &AttrsList,
        run: &Range<usize>,
        rtl: bool,
    ) -> Option<&[ShapeGlyph]> {
        if self.cache.is_empty() {
            return None;
        }
        let hash = hash_parts(
            &line[run.clone()],
            attrs_list.defaults_owned(),
            run_spans(attrs_list, run),
            rtl,
        );
        let (age, tick) = (self.age, self.next_tick());
        self.cache
            .get_mut(&hash)
            .filter(|entry| entry.key.matches(line, attrs_list, run, rtl))
            .map(|entry| {
                entry.age = age;
                entry.tick = tick;
                &*entry.glyphs
            })
    }

    /// Insert cache item with current age
    pub fn insert(&mut self, key: ShapeRunKey, glyphs: Vec<ShapeGlyph>) {
        if glyphs.len() > self.max_glyphs {
            return;
        }
        let entry = Entry {
            key,
            age: self.age,
            tick: self.next_tick(),
            glyphs,
        };
        self.glyphs += entry.glyphs.len();
        if let Some(old) = self.cache.insert(entry.key.hash_value(), entry) {
            self.glyphs -= old.glyphs.len();
        }
        if self.glyphs > self.max_glyphs {
            self.evict();
        }
    }

    /// Remove anything in the cache with an age older than `keep_ages`
    pub fn trim(&mut self, keep_ages: u64) {
        let age = self.age;
        self.cache
            .retain(|_key, entry| entry.age + keep_ages >= age);
        self.glyphs = self.cache.values().map(|entry| entry.glyphs.len()).sum();
        // Increase age
        self.age += 1;
    }

    fn next_tick(&mut self) -> u64 {
        self.tick += 1;
        self.tick
    }

    /// Drops the least recently used half of the runs until the glyphs fit.
    fn evict(&mut self) {
        while self.glyphs > self.max_glyphs {
            if self.max_glyphs == 0 || self.cache.len() < 2 {
                self.clear();
                return;
            }
            let mut ticks: Vec<u64> = self.cache.values().map(|entry| entry.tick).collect();
            let half = ticks.len() / 2;
            let cutoff = *ticks.select_nth_unstable(half).1;
            self.cache.retain(|_key, entry| entry.tick >= cutoff);
            self.glyphs = self.cache.values().map(|entry| entry.glyphs.len()).sum();
        }
    }
}

impl core::fmt::Debug for ShapeRunCache {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ShapeRunCache")
            .field("runs", &self.cache.len())
            .field("glyphs", &self.glyphs)
            .field("max_glyphs", &self.max_glyphs)
            .finish()
    }
}
