use unicode_bidi::{BidiInfo, Level as BidiLevel};
use unicode_script::{Script, UnicodeScript};
use xi_unicode::LineBreakIterator;

use typst::model::Key;

use super::{HNode, RepeatNode, Spacing};
use crate::layout::AlignNode;
use crate::prelude::*;
use crate::text::{
    shape, LinebreakNode, Quoter, Quotes, ShapedText, SmartQuoteNode, SpaceNode, TextNode,
};

/// # Paragraph
/// Arrange text, spacing and inline-level nodes into a paragraph.
///
/// ## Parameters
/// - body: Content (positional, required)
///   The contents of the paragraph.
///
/// ## Category
/// layout
#[func]
#[capable]
#[derive(Hash)]
pub struct ParNode(pub StyleVec<Content>);

#[node]
impl ParNode {
    /// The indent the first line of a consecutive paragraph should have.
    #[property(resolve)]
    pub const INDENT: Length = Length::zero();
    /// The spacing between lines.
    #[property(resolve)]
    pub const LEADING: Length = Em::new(0.65).into();
    /// Whether to justify text in its line.
    pub const JUSTIFY: bool = false;
    /// How to determine line breaks.
    pub const LINEBREAKS: Smart<Linebreaks> = Smart::Auto;

    fn construct(_: &Vm, args: &mut Args) -> SourceResult<Content> {
        // The paragraph constructor is special: It doesn't create a paragraph
        // node. Instead, it just ensures that the passed content lives in a
        // separate paragraph and styles it.
        Ok(Content::sequence(vec![
            ParbreakNode.pack(),
            args.expect("body")?,
            ParbreakNode.pack(),
        ]))
    }
}

impl ParNode {
    /// Layout the paragraph into a collection of lines.
    pub fn layout(
        &self,
        vt: &mut Vt,
        styles: StyleChain,
        consecutive: bool,
        width: Abs,
        base: Size,
        expand: bool,
    ) -> SourceResult<Fragment> {
        #[comemo::memoize]
        fn cached(
            par: &ParNode,
            world: Tracked<dyn World>,
            provider: TrackedMut<StabilityProvider>,
            introspector: Tracked<Introspector>,
            styles: StyleChain,
            consecutive: bool,
            width: Abs,
            base: Size,
            expand: bool,
        ) -> SourceResult<Fragment> {
            let mut vt = Vt { world, provider, introspector };

            // Collect all text into one string for BiDi analysis.
            let (text, segments) = collect(par, &styles, consecutive);

            // Perform BiDi analysis and then prepare paragraph layout by building a
            // representation on which we can do line breaking without layouting
            // each and every line from scratch.
            let p = prepare(&mut vt, par, &text, segments, styles, width, base)?;

            // Break the paragraph into lines.
            let lines = linebreak(&vt, &p, width);

            // Stack the lines into one frame per region.
            finalize(&mut vt, &p, &lines, width, base, expand)
        }

        cached(
            self,
            vt.world,
            TrackedMut::reborrow_mut(&mut vt.provider),
            vt.introspector,
            styles,
            consecutive,
            width,
            base,
            expand,
        )
    }
}

impl Debug for ParNode {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        f.write_str("Par ")?;
        self.0.fmt(f)
    }
}

/// A horizontal alignment.
#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
pub struct HorizontalAlign(pub GenAlign);

castable! {
    HorizontalAlign,
    align: GenAlign => match align.axis() {
        Axis::X => Self(align),
        Axis::Y => Err("must be horizontal")?,
    },
}

impl Resolve for HorizontalAlign {
    type Output = Align;

    fn resolve(self, styles: StyleChain) -> Self::Output {
        self.0.resolve(styles)
    }
}

/// How to determine line breaks in a paragraph.
#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
pub enum Linebreaks {
    /// Determine the linebreaks in a simple first-fit style.
    Simple,
    /// Optimize the linebreaks for the whole paragraph.
    Optimized,
}

castable! {
    Linebreaks,
    /// Determine the linebreaks in a simple first-fit style.
    "simple" => Self::Simple,
    /// Optimize the linebreaks for the whole paragraph.
    "optimized" => Self::Optimized,
}

/// # Paragraph Break
/// A paragraph break.
///
/// This starts a new paragraph. Especially useful when used within code like
/// [for loops](/docs/reference/concepts#for-loop). Multiple consecutive
/// paragraph breaks collapse into a single one.
///
/// ## Example
/// ```
/// #for i in range(3) {
///   [Blind text #i: ]
///   lorem(5)
///   parbreak()
/// }
/// ```
///
/// ## Category
/// layout
#[func]
#[capable(Unlabellable)]
#[derive(Debug, Hash)]
pub struct ParbreakNode;

#[node]
impl ParbreakNode {
    fn construct(_: &Vm, _: &mut Args) -> SourceResult<Content> {
        Ok(Self.pack())
    }
}

impl Unlabellable for ParbreakNode {}

/// Range of a substring of text.
type Range = std::ops::Range<usize>;

// The characters by which spacing, inline content and pins are replaced in the
// paragraph's full text.
const SPACING_REPLACE: char = ' '; // Space
const NODE_REPLACE: char = '\u{FFFC}'; // Object Replacement Character

/// A paragraph representation in which children are already layouted and text
/// is already preshaped.
///
/// In many cases, we can directly reuse these results when constructing a line.
/// Only when a line break falls onto a text index that is not safe-to-break per
/// rustybuzz, we have to reshape that portion.
struct Preparation<'a> {
    /// Bidirectional text embedding levels for the paragraph.
    bidi: BidiInfo<'a>,
    /// Text runs, spacing and layouted nodes.
    items: Vec<Item<'a>>,
    /// The styles shared by all children.
    styles: StyleChain<'a>,
    /// Whether to hyphenate if it's the same for all children.
    hyphenate: Option<bool>,
    /// The text language if it's the same for all children.
    lang: Option<Lang>,
    /// The paragraph's resolved alignment.
    align: Align,
    /// Whether to justify the paragraph.
    justify: bool,
}

impl<'a> Preparation<'a> {
    /// Find the item that contains the given `text_offset`.
    fn find(&self, text_offset: usize) -> Option<&Item<'a>> {
        let mut cursor = 0;
        for item in &self.items {
            let end = cursor + item.len();
            if (cursor..end).contains(&text_offset) {
                return Some(item);
            }
            cursor = end;
        }
        None
    }

    /// Return the items that intersect the given `text_range`.
    ///
    /// Returns the expanded range around the items and the items.
    fn slice(&self, text_range: Range) -> (Range, &[Item<'a>]) {
        let mut cursor = 0;
        let mut start = 0;
        let mut end = 0;
        let mut expanded = text_range.clone();

        for (i, item) in self.items.iter().enumerate() {
            if cursor <= text_range.start {
                start = i;
                expanded.start = cursor;
            }

            let len = item.len();
            if cursor < text_range.end || cursor + len <= text_range.end {
                end = i + 1;
                expanded.end = cursor + len;
            } else {
                break;
            }

            cursor += len;
        }

        (expanded, &self.items[start..end])
    }
}

/// A segment of one or multiple collapsed children.
#[derive(Debug, Copy, Clone)]
enum Segment<'a> {
    /// One or multiple collapsed text or text-equivalent children. Stores how
    /// long the segment is (in bytes of the full text string).
    Text(usize),
    /// Horizontal spacing between other segments.
    Spacing(Spacing),
    /// Arbitrary inline-level content.
    Inline(&'a Content),
}

impl Segment<'_> {
    /// The text length of the item.
    fn len(&self) -> usize {
        match *self {
            Self::Text(len) => len,
            Self::Spacing(_) => SPACING_REPLACE.len_utf8(),
            Self::Inline(_) => NODE_REPLACE.len_utf8(),
        }
    }
}

/// A prepared item in a paragraph layout.
#[derive(Debug)]
enum Item<'a> {
    /// A shaped text run with consistent style and direction.
    Text(ShapedText<'a>),
    /// Absolute spacing between other items.
    Absolute(Abs),
    /// Fractional spacing between other items.
    Fractional(Fr),
    /// Layouted inline-level content.
    Frame(Frame),
    /// A repeating node that fills the remaining space in a line.
    Repeat(&'a RepeatNode, StyleChain<'a>),
}

impl<'a> Item<'a> {
    /// If this a text item, return it.
    fn text(&self) -> Option<&ShapedText<'a>> {
        match self {
            Self::Text(shaped) => Some(shaped),
            _ => None,
        }
    }

    /// The text length of the item.
    fn len(&self) -> usize {
        match self {
            Self::Text(shaped) => shaped.text.len(),
            Self::Absolute(_) | Self::Fractional(_) => SPACING_REPLACE.len_utf8(),
            Self::Frame(_) | Self::Repeat(_, _) => NODE_REPLACE.len_utf8(),
        }
    }

    /// The natural layouted width of the item.
    fn width(&self) -> Abs {
        match self {
            Self::Text(shaped) => shaped.width,
            Self::Absolute(v) => *v,
            Self::Frame(frame) => frame.width(),
            Self::Fractional(_) | Self::Repeat(_, _) => Abs::zero(),
        }
    }
}

/// A layouted line, consisting of a sequence of layouted paragraph items that
/// are mostly borrowed from the preparation phase. This type enables you to
/// measure the size of a line in a range before comitting to building the
/// line's frame.
///
/// At most two paragraph items must be created individually for this line: The
/// first and last one since they may be broken apart by the start or end of the
/// line, respectively. But even those can partially reuse previous results when
/// the break index is safe-to-break per rustybuzz.
struct Line<'a> {
    /// Bidi information about the paragraph.
    bidi: &'a BidiInfo<'a>,
    /// The trimmed range the line spans in the paragraph.
    trimmed: Range,
    /// The untrimmed end where the line ends.
    end: usize,
    /// A reshaped text item if the line sliced up a text item at the start.
    first: Option<Item<'a>>,
    /// Inner items which don't need to be reprocessed.
    inner: &'a [Item<'a>],
    /// A reshaped text item if the line sliced up a text item at the end. If
    /// there is only one text item, this takes precedence over `first`.
    last: Option<Item<'a>>,
    /// The width of the line.
    width: Abs,
    /// Whether the line should be justified.
    justify: bool,
    /// Whether the line ends with a hyphen or dash, either naturally or through
    /// hyphenation.
    dash: bool,
}

impl<'a> Line<'a> {
    /// Iterate over the line's items.
    fn items(&self) -> impl Iterator<Item = &Item<'a>> {
        self.first.iter().chain(self.inner).chain(&self.last)
    }

    /// Return items that intersect the given `text_range`.
    fn slice(&self, text_range: Range) -> impl Iterator<Item = &Item<'a>> {
        let mut cursor = self.trimmed.start;
        let mut start = 0;
        let mut end = 0;

        for (i, item) in self.items().enumerate() {
            if cursor <= text_range.start {
                start = i;
            }

            let len = item.len();
            if cursor < text_range.end || cursor + len <= text_range.end {
                end = i + 1;
            } else {
                break;
            }

            cursor += len;
        }

        self.items().skip(start).take(end - start)
    }

    /// How many justifiable glyphs the line contains.
    fn justifiables(&self) -> usize {
        let mut count = 0;
        for shaped in self.items().filter_map(Item::text) {
            count += shaped.justifiables();
        }
        count
    }

    /// How much of the line is stretchable spaces.
    fn stretch(&self) -> Abs {
        let mut stretch = Abs::zero();
        for shaped in self.items().filter_map(Item::text) {
            stretch += shaped.stretch();
        }
        stretch
    }

    /// The sum of fractions in the line.
    fn fr(&self) -> Fr {
        self.items()
            .filter_map(|item| match item {
                Item::Fractional(fr) => Some(*fr),
                Item::Repeat(_, _) => Some(Fr::one()),
                _ => None,
            })
            .sum()
    }
}

/// Collect all text of the paragraph into one string. This also performs
/// string-level preprocessing like case transformations.
fn collect<'a>(
    par: &'a ParNode,
    styles: &'a StyleChain<'a>,
    consecutive: bool,
) -> (String, Vec<(Segment<'a>, StyleChain<'a>)>) {
    let mut full = String::new();
    let mut quoter = Quoter::new();
    let mut segments = vec![];
    let mut iter = par.0.iter().peekable();

    if consecutive {
        let indent = styles.get(ParNode::INDENT);
        if !indent.is_zero()
            && par
                .0
                .items()
                .find_map(|child| {
                    if child.is::<TextNode>() || child.is::<SmartQuoteNode>() {
                        Some(true)
                    } else if child.has::<dyn Inline>() {
                        Some(false)
                    } else {
                        None
                    }
                })
                .unwrap_or_default()
        {
            full.push(SPACING_REPLACE);
            segments.push((Segment::Spacing(indent.into()), *styles));
        }
    }

    while let Some((child, map)) = iter.next() {
        let styles = styles.chain(map);
        let segment = if child.is::<SpaceNode>() {
            full.push(' ');
            Segment::Text(1)
        } else if let Some(node) = child.to::<TextNode>() {
            let prev = full.len();
            if let Some(case) = styles.get(TextNode::CASE) {
                full.push_str(&case.apply(&node.0));
            } else {
                full.push_str(&node.0);
            }
            Segment::Text(full.len() - prev)
        } else if let Some(node) = child.to::<LinebreakNode>() {
            let c = if node.justify { '\u{2028}' } else { '\n' };
            full.push(c);
            Segment::Text(c.len_utf8())
        } else if let Some(node) = child.to::<SmartQuoteNode>() {
            let prev = full.len();
            if styles.get(SmartQuoteNode::ENABLED) {
                let lang = styles.get(TextNode::LANG);
                let region = styles.get(TextNode::REGION);
                let quotes = Quotes::from_lang(lang, region);
                let peeked = iter.peek().and_then(|(child, _)| {
                    if let Some(node) = child.to::<TextNode>() {
                        node.0.chars().next()
                    } else if child.is::<SmartQuoteNode>() {
                        Some('"')
                    } else if child.is::<SpaceNode>() || child.is::<HNode>() {
                        Some(SPACING_REPLACE)
                    } else {
                        Some(NODE_REPLACE)
                    }
                });

                full.push_str(quoter.quote(&quotes, node.double, peeked));
            } else {
                full.push(if node.double { '"' } else { '\'' });
            }
            Segment::Text(full.len() - prev)
        } else if let Some(&node) = child.to::<HNode>() {
            full.push(SPACING_REPLACE);
            Segment::Spacing(node.amount)
        } else if child.has::<dyn Inline>() {
            full.push(NODE_REPLACE);
            Segment::Inline(child)
        } else {
            panic!("unexpected par child: {child:?}");
        };

        if let Some(last) = full.chars().last() {
            quoter.last(last);
        }

        if let (Some((Segment::Text(last_len), last_styles)), Segment::Text(len)) =
            (segments.last_mut(), segment)
        {
            if *last_styles == styles {
                *last_len += len;
                continue;
            }
        }

        segments.push((segment, styles));
    }

    (full, segments)
}

/// Prepare paragraph layout by shaping the whole paragraph and layouting all
/// contained inline-level content.
fn prepare<'a>(
    vt: &mut Vt,
    par: &'a ParNode,
    text: &'a str,
    segments: Vec<(Segment<'a>, StyleChain<'a>)>,
    styles: StyleChain<'a>,
    width: Abs,
    base: Size,
) -> SourceResult<Preparation<'a>> {
    let bidi = BidiInfo::new(
        text,
        match styles.get(TextNode::DIR) {
            Dir::LTR => Some(BidiLevel::ltr()),
            Dir::RTL => Some(BidiLevel::rtl()),
            _ => None,
        },
    );

    let mut cursor = 0;
    let mut items = vec![];

    // Shape / layout the children and collect them into items.
    for (segment, styles) in segments {
        let end = cursor + segment.len();
        match segment {
            Segment::Text(_) => {
                shape_range(&mut items, vt, &bidi, cursor..end, styles);
            }
            Segment::Spacing(spacing) => match spacing {
                Spacing::Relative(v) => {
                    let resolved = v.resolve(styles).relative_to(base.x);
                    items.push(Item::Absolute(resolved));
                }
                Spacing::Fractional(v) => {
                    items.push(Item::Fractional(v));
                }
            },
            Segment::Inline(inline) => {
                if let Some(repeat) = inline.to::<RepeatNode>() {
                    items.push(Item::Repeat(repeat, styles));
                } else {
                    let size = Size::new(width, base.y);
                    let pod = Regions::one(size, base, Axes::splat(false));
                    let mut frame = inline.layout(vt, styles, pod)?.into_frame();
                    frame.translate(Point::with_y(styles.get(TextNode::BASELINE)));
                    items.push(Item::Frame(frame));
                }
            }
        }

        cursor = end;
    }

    Ok(Preparation {
        bidi,
        items,
        styles,
        hyphenate: shared_get(styles, &par.0, TextNode::HYPHENATE),
        lang: shared_get(styles, &par.0, TextNode::LANG),
        align: styles.get(AlignNode::ALIGNS).x.resolve(styles),
        justify: styles.get(ParNode::JUSTIFY),
    })
}

/// Group a range of text by BiDi level and script, shape the runs and generate
/// items for them.
fn shape_range<'a>(
    items: &mut Vec<Item<'a>>,
    vt: &Vt,
    bidi: &BidiInfo<'a>,
    range: Range,
    styles: StyleChain<'a>,
) {
    let mut process = |text, level: BidiLevel| {
        let dir = if level.is_ltr() { Dir::LTR } else { Dir::RTL };
        let shaped = shape(vt, text, styles, dir);
        items.push(Item::Text(shaped));
    };

    let mut prev_level = BidiLevel::ltr();
    let mut prev_script = Script::Unknown;
    let mut cursor = range.start;

    // Group by embedding level and script.
    for i in cursor..range.end {
        if !bidi.text.is_char_boundary(i) {
            continue;
        }

        let level = bidi.levels[i];
        let script =
            bidi.text[i..].chars().next().map_or(Script::Unknown, |c| c.script());

        if level != prev_level || !is_compatible(script, prev_script) {
            if cursor < i {
                process(&bidi.text[cursor..i], prev_level);
            }
            cursor = i;
            prev_level = level;
            prev_script = script;
        } else if is_generic_script(prev_script) {
            prev_script = script;
        }
    }

    process(&bidi.text[cursor..range.end], prev_level);
}

/// Whether this is not a specific script.
fn is_generic_script(script: Script) -> bool {
    matches!(script, Script::Unknown | Script::Common | Script::Inherited)
}

/// Whether these script can be part of the same shape run.
fn is_compatible(a: Script, b: Script) -> bool {
    is_generic_script(a) || is_generic_script(b) || a == b
}

/// Get a style property, but only if it is the same for all children of the
/// paragraph.
fn shared_get<'a, K: Key>(
    styles: StyleChain<'a>,
    children: &StyleVec<Content>,
    key: K,
) -> Option<K::Output<'a>> {
    children
        .styles()
        .all(|map| !map.contains(key))
        .then(|| styles.get(key))
}

/// Find suitable linebreaks.
fn linebreak<'a>(vt: &Vt, p: &'a Preparation<'a>, width: Abs) -> Vec<Line<'a>> {
    let linebreaks = p.styles.get(ParNode::LINEBREAKS).unwrap_or_else(|| {
        if p.styles.get(ParNode::JUSTIFY) {
            Linebreaks::Optimized
        } else {
            Linebreaks::Simple
        }
    });

    match linebreaks {
        Linebreaks::Simple => linebreak_simple(vt, p, width),
        Linebreaks::Optimized => linebreak_optimized(vt, p, width),
    }
}

/// Perform line breaking in simple first-fit style. This means that we build
/// lines greedily, always taking the longest possible line. This may lead to
/// very unbalanced line, but is fast and simple.
fn linebreak_simple<'a>(vt: &Vt, p: &'a Preparation<'a>, width: Abs) -> Vec<Line<'a>> {
    let mut lines = vec![];
    let mut start = 0;
    let mut last = None;

    for (end, mandatory, hyphen) in breakpoints(p) {
        // Compute the line and its size.
        let mut attempt = line(vt, p, start..end, mandatory, hyphen);

        // If the line doesn't fit anymore, we push the last fitting attempt
        // into the stack and rebuild the line from the attempt's end. The
        // resulting line cannot be broken up further.
        if !width.fits(attempt.width) {
            if let Some((last_attempt, last_end)) = last.take() {
                lines.push(last_attempt);
                start = last_end;
                attempt = line(vt, p, start..end, mandatory, hyphen);
            }
        }

        // Finish the current line if there is a mandatory line break (i.e.
        // due to "\n") or if the line doesn't fit horizontally already
        // since then no shorter line will be possible.
        if mandatory || !width.fits(attempt.width) {
            lines.push(attempt);
            start = end;
            last = None;
        } else {
            last = Some((attempt, end));
        }
    }

    if let Some((line, _)) = last {
        lines.push(line);
    }

    lines
}

/// Perform line breaking in optimized Knuth-Plass style. Here, we use more
/// context to determine the line breaks than in the simple first-fit style. For
/// example, we might choose to cut a line short even though there is still a
/// bit of space to improve the fit of one of the following lines. The
/// Knuth-Plass algorithm is based on the idea of "cost". A line which has a
/// very tight or very loose fit has a higher cost than one that is just right.
/// Ending a line with a hyphen incurs extra cost and endings two successive
/// lines with hyphens even more.
///
/// To find the layout with the minimal total cost the algorithm uses dynamic
/// programming: For each possible breakpoint it determines the optimal
/// paragraph layout _up to that point_. It walks over all possible start points
/// for a line ending at that point and finds the one for which the cost of the
/// line plus the cost of the optimal paragraph up to the start point (already
/// computed and stored in dynamic programming table) is minimal. The final
/// result is simply the layout determined for the last breakpoint at the end of
/// text.
fn linebreak_optimized<'a>(vt: &Vt, p: &'a Preparation<'a>, width: Abs) -> Vec<Line<'a>> {
    /// The cost of a line or paragraph layout.
    type Cost = f64;

    /// An entry in the dynamic programming table.
    struct Entry<'a> {
        pred: usize,
        total: Cost,
        line: Line<'a>,
    }

    // Cost parameters.
    const HYPH_COST: Cost = 0.5;
    const CONSECUTIVE_DASH_COST: Cost = 30.0;
    const MAX_COST: Cost = 1_000_000.0;
    const MIN_COST: Cost = -MAX_COST;
    const MIN_RATIO: f64 = -0.15;

    // Dynamic programming table.
    let mut active = 0;
    let mut table = vec![Entry {
        pred: 0,
        total: 0.0,
        line: line(vt, p, 0..0, false, false),
    }];

    let em = p.styles.get(TextNode::SIZE);

    for (end, mandatory, hyphen) in breakpoints(p) {
        let k = table.len();
        let eof = end == p.bidi.text.len();
        let mut best: Option<Entry> = None;

        // Find the optimal predecessor.
        for (i, pred) in table.iter_mut().enumerate().skip(active) {
            // Layout the line.
            let start = pred.line.end;
            let attempt = line(vt, p, start..end, mandatory, hyphen);

            // Determine how much the line's spaces would need to be stretched
            // to make it the desired width.
            let delta = width - attempt.width;
            let mut ratio = delta / attempt.stretch();
            if ratio.is_infinite() {
                ratio = delta / (em / 2.0);
            }

            // At some point, it doesn't matter any more.
            ratio = ratio.min(10.0);

            // Determine the cost of the line.
            let min_ratio = if attempt.justify { MIN_RATIO } else { 0.0 };
            let mut cost = if ratio < min_ratio {
                // The line is overfull. This is the case if
                // - justification is on, but we'd need to shrink too much
                // - justification is off and the line just doesn't fit
                // Since any longer line will also be overfull, we can deactive
                // this breakpoint.
                active = i + 1;
                MAX_COST
            } else if mandatory || eof {
                // This is a mandatory break and the line is not overfull, so it
                // has minimum cost. All breakpoints before this one become
                // inactive since no line can span above the mandatory break.
                active = k;
                MIN_COST
                    + if attempt.justify {
                        ratio.powi(3).abs()
                    } else {
                        0.0
                    }
            } else {
                // Normal line with cost of |ratio^3|.
                ratio.powi(3).abs()
            };

            // Penalize hyphens.
            if hyphen {
                cost += HYPH_COST;
            }

            // Penalize two consecutive dashes (not necessarily hyphens) extra.
            if attempt.dash && pred.line.dash {
                cost += CONSECUTIVE_DASH_COST;
            }

            // The total cost of this line and its chain of predecessors.
            let total = pred.total + cost;

            // If this attempt is better than what we had before, take it!
            if best.as_ref().map_or(true, |best| best.total >= total) {
                best = Some(Entry { pred: i, total, line: attempt });
            }
        }

        table.push(best.unwrap());
    }

    // Retrace the best path.
    let mut lines = vec![];
    let mut idx = table.len() - 1;
    while idx != 0 {
        table.truncate(idx + 1);
        let entry = table.pop().unwrap();
        lines.push(entry.line);
        idx = entry.pred;
    }

    lines.reverse();
    lines
}

/// Determine all possible points in the text where lines can broken.
///
/// Returns for each breakpoint the text index, whether the break is mandatory
/// (after `\n`) and whether a hyphen is required (when breaking inside of a
/// word).
fn breakpoints<'a>(p: &'a Preparation<'a>) -> Breakpoints<'a> {
    Breakpoints {
        p,
        linebreaks: LineBreakIterator::new(p.bidi.text),
        syllables: None,
        offset: 0,
        suffix: 0,
        end: 0,
        mandatory: false,
    }
}

/// An iterator over the line break opportunities in a text.
struct Breakpoints<'a> {
    /// The paragraph's items.
    p: &'a Preparation<'a>,
    /// The inner iterator over the unicode line break opportunities.
    linebreaks: LineBreakIterator<'a>,
    /// Iterator over syllables of the current word.
    syllables: Option<hypher::Syllables<'a>>,
    /// The current text offset.
    offset: usize,
    /// The trimmed end of the current word.
    suffix: usize,
    /// The untrimmed end of the current word.
    end: usize,
    /// Whether the break after the current word is mandatory.
    mandatory: bool,
}

impl Iterator for Breakpoints<'_> {
    type Item = (usize, bool, bool);

    fn next(&mut self) -> Option<Self::Item> {
        // If we're currently in a hyphenated "word", process the next syllable.
        if let Some(syllable) = self.syllables.as_mut().and_then(Iterator::next) {
            self.offset += syllable.len();
            if self.offset == self.suffix {
                self.offset = self.end;
            }

            // Filter out hyphenation opportunities where hyphenation was
            // actually disabled.
            let hyphen = self.offset < self.end;
            if hyphen && !self.hyphenate(self.offset) {
                return self.next();
            }

            return Some((self.offset, self.mandatory && !hyphen, hyphen));
        }

        // Get the next "word".
        (self.end, self.mandatory) = self.linebreaks.next()?;

        // Hyphenate the next word.
        if self.p.hyphenate != Some(false) {
            if let Some(lang) = self.lang(self.offset) {
                let word = &self.p.bidi.text[self.offset..self.end];
                let trimmed = word.trim_end_matches(|c: char| !c.is_alphabetic());
                if !trimmed.is_empty() {
                    self.suffix = self.offset + trimmed.len();
                    self.syllables = Some(hypher::hyphenate(trimmed, lang));
                    return self.next();
                }
            }
        }

        self.offset = self.end;
        Some((self.end, self.mandatory, false))
    }
}

impl Breakpoints<'_> {
    /// Whether hyphenation is enabled at the given offset.
    fn hyphenate(&self, offset: usize) -> bool {
        self.p
            .hyphenate
            .or_else(|| {
                let shaped = self.p.find(offset)?.text()?;
                Some(shaped.styles.get(TextNode::HYPHENATE))
            })
            .unwrap_or(false)
    }

    /// The text language at the given offset.
    fn lang(&self, offset: usize) -> Option<hypher::Lang> {
        let lang = self.p.lang.or_else(|| {
            let shaped = self.p.find(offset)?.text()?;
            Some(shaped.styles.get(TextNode::LANG))
        })?;

        let bytes = lang.as_str().as_bytes().try_into().ok()?;
        hypher::Lang::from_iso(bytes)
    }
}

/// Create a line which spans the given range.
fn line<'a>(
    vt: &Vt,
    p: &'a Preparation,
    mut range: Range,
    mandatory: bool,
    hyphen: bool,
) -> Line<'a> {
    let end = range.end;
    let mut justify = p.justify && end < p.bidi.text.len() && !mandatory;

    if range.is_empty() {
        return Line {
            bidi: &p.bidi,
            end,
            trimmed: range,
            first: None,
            inner: &[],
            last: None,
            width: Abs::zero(),
            justify,
            dash: false,
        };
    }

    // Slice out the relevant items.
    let (expanded, mut inner) = p.slice(range.clone());
    let mut width = Abs::zero();

    // Reshape the last item if it's split in half or hyphenated.
    let mut last = None;
    let mut dash = false;
    if let Some((Item::Text(shaped), before)) = inner.split_last() {
        // Compute the range we want to shape, trimming whitespace at the
        // end of the line.
        let base = expanded.end - shaped.text.len();
        let start = range.start.max(base);
        let text = &p.bidi.text[start..range.end];
        let trimmed = text.trim_end();
        range.end = start + trimmed.len();

        // Deal with hyphens, dashes and justification.
        let shy = trimmed.ends_with('\u{ad}');
        dash = hyphen || shy || trimmed.ends_with(['-', '–', '—']);
        justify |= text.ends_with('\u{2028}');

        // Usually, we don't want to shape an empty string because:
        // - We don't want the height of trimmed whitespace in a different
        //   font to be considered for the line height.
        // - Even if it's in the same font, its unnecessary.
        //
        // There is one exception though. When the whole line is empty, we
        // need the shaped empty string to make the line the appropriate
        // height. That is the case exactly if the string is empty and there
        // are no other items in the line.
        if hyphen || start + shaped.text.len() > range.end {
            if hyphen || start < range.end || before.is_empty() {
                let shifted = start - base..range.end - base;
                let mut reshaped = shaped.reshape(vt, shifted);
                if hyphen || shy {
                    reshaped.push_hyphen(vt);
                }
                width += reshaped.width;
                last = Some(Item::Text(reshaped));
            }

            inner = before;
        }
    }

    // Reshape the start item if it's split in half.
    let mut first = None;
    if let Some((Item::Text(shaped), after)) = inner.split_first() {
        // Compute the range we want to shape.
        let base = expanded.start;
        let end = range.end.min(base + shaped.text.len());

        // Reshape if necessary.
        if range.start + shaped.text.len() > end {
            if range.start < end {
                let shifted = range.start - base..end - base;
                let reshaped = shaped.reshape(vt, shifted);
                width += reshaped.width;
                first = Some(Item::Text(reshaped));
            }

            inner = after;
        }
    }

    // Measure the inner items.
    for item in inner {
        width += item.width();
    }

    Line {
        bidi: &p.bidi,
        trimmed: range,
        end,
        first,
        inner,
        last,
        width,
        justify,
        dash,
    }
}

/// Combine layouted lines into one frame per region.
fn finalize(
    vt: &mut Vt,
    p: &Preparation,
    lines: &[Line],
    mut width: Abs,
    base: Size,
    expand: bool,
) -> SourceResult<Fragment> {
    // Determine the paragraph's width: Full width of the region if we
    // should expand or there's fractional spacing, fit-to-width otherwise.
    if !expand && lines.iter().all(|line| line.fr().is_zero()) {
        width = lines.iter().map(|line| line.width).max().unwrap_or_default();
    }

    // Stack the lines into one frame per region.
    lines
        .iter()
        .map(|line| commit(vt, p, line, base, width))
        .collect::<SourceResult<_>>()
        .map(Fragment::frames)
}

/// Commit to a line and build its frame.
fn commit(
    vt: &mut Vt,
    p: &Preparation,
    line: &Line,
    base: Size,
    width: Abs,
) -> SourceResult<Frame> {
    let mut remaining = width - line.width;
    let mut offset = Abs::zero();

    // Reorder the line from logical to visual order.
    let reordered = reorder(line);

    // Handle hanging punctuation to the left.
    if let Some(Item::Text(text)) = reordered.first() {
        if let Some(glyph) = text.glyphs.first() {
            if !text.dir.is_positive()
                && text.styles.get(TextNode::OVERHANG)
                && (reordered.len() > 1 || text.glyphs.len() > 1)
            {
                let amount = overhang(glyph.c) * glyph.x_advance.at(text.size);
                offset -= amount;
                remaining += amount;
            }
        }
    }

    // Handle hanging punctuation to the right.
    if let Some(Item::Text(text)) = reordered.last() {
        if let Some(glyph) = text.glyphs.last() {
            if text.dir.is_positive()
                && text.styles.get(TextNode::OVERHANG)
                && (reordered.len() > 1 || text.glyphs.len() > 1)
            {
                let amount = overhang(glyph.c) * glyph.x_advance.at(text.size);
                remaining += amount;
            }
        }
    }

    // Determine how much to justify each space.
    let fr = line.fr();
    let mut justification = Abs::zero();
    if remaining < Abs::zero() || (line.justify && fr.is_zero()) {
        let justifiables = line.justifiables();
        if justifiables > 0 {
            justification = remaining / justifiables as f64;
            remaining = Abs::zero();
        }
    }

    let mut top = Abs::zero();
    let mut bottom = Abs::zero();

    // Build the frames and determine the height and baseline.
    let mut frames = vec![];
    for item in reordered {
        let mut push = |offset: &mut Abs, frame: Frame| {
            let width = frame.width();
            top.set_max(frame.baseline());
            bottom.set_max(frame.size().y - frame.baseline());
            frames.push((*offset, frame));
            *offset += width;
        };

        match item {
            Item::Absolute(v) => {
                offset += *v;
            }
            Item::Fractional(v) => {
                offset += v.share(fr, remaining);
            }
            Item::Text(shaped) => {
                let frame = shaped.build(vt, justification);
                push(&mut offset, frame);
            }
            Item::Frame(frame) => {
                push(&mut offset, frame.clone());
            }
            Item::Repeat(repeat, styles) => {
                let before = offset;
                let fill = Fr::one().share(fr, remaining);
                let size = Size::new(fill, base.y);
                let pod = Regions::one(size, base, Axes::new(false, false));
                let frame = repeat.layout(vt, *styles, pod)?.into_frame();
                let width = frame.width();
                let count = (fill / width).floor();
                let remaining = fill % width;
                let apart = remaining / (count - 1.0);
                if count == 1.0 {
                    offset += p.align.position(remaining);
                }
                if width > Abs::zero() {
                    for _ in 0..(count as usize).min(1000) {
                        push(&mut offset, frame.clone());
                        offset += apart;
                    }
                }
                offset = before + fill;
            }
        }
    }

    // Remaining space is distributed now.
    if !fr.is_zero() {
        remaining = Abs::zero();
    }

    let size = Size::new(width, top + bottom);
    let mut output = Frame::new(size);
    output.set_baseline(top);

    // Construct the line's frame.
    for (offset, frame) in frames {
        let x = offset + p.align.position(remaining);
        let y = top - frame.baseline();
        output.push_frame(Point::new(x, y), frame);
    }

    Ok(output)
}

/// Return a line's items in visual order.
fn reorder<'a>(line: &'a Line<'a>) -> Vec<&Item<'a>> {
    let mut reordered = vec![];

    // The bidi crate doesn't like empty lines.
    if line.trimmed.is_empty() {
        return line.slice(line.trimmed.clone()).collect();
    }

    // Find the paragraph that contains the line.
    let para = line
        .bidi
        .paragraphs
        .iter()
        .find(|para| para.range.contains(&line.trimmed.start))
        .unwrap();

    // Compute the reordered ranges in visual order (left to right).
    let (levels, runs) = line.bidi.visual_runs(para, line.trimmed.clone());

    // Collect the reordered items.
    for run in runs {
        // Skip reset L1 runs because handling them would require reshaping
        // again in some cases.
        if line.bidi.levels[run.start] != levels[run.start] {
            continue;
        }

        let prev = reordered.len();
        reordered.extend(line.slice(run.clone()));

        if levels[run.start].is_rtl() {
            reordered[prev..].reverse();
        }
    }

    reordered
}

/// How much a character should hang into the end margin.
///
/// For more discussion, see:
/// https://recoveringphysicist.com/21/
fn overhang(c: char) -> f64 {
    match c {
        // Dashes.
        '–' | '—' => 0.2,
        '-' => 0.55,

        // Punctuation.
        '.' | ',' => 0.8,
        ':' | ';' => 0.3,

        // Arabic and Ideographic
        '\u{60C}' | '\u{6D4}' => 0.4,
        '\u{3001}' | '\u{3002}' => 1.0,

        _ => 0.0,
    }
}
