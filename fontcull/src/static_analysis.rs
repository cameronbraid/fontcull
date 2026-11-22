//! Static HTML/CSS analysis for font usage detection
//!
//! Parses HTML and CSS to determine which characters are used with which fonts,
//! without requiring a browser.

use scraper::{Html, Selector};
use std::collections::{HashMap, HashSet};

/// CSS custom properties (variables) map
type CssVariables = HashMap<String, String>;

/// A parsed @font-face rule
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FontFace {
    /// The font-family name declared in @font-face
    pub family: String,
    /// The URL to the font file (from src)
    pub src: String,
    /// Font weight (e.g., "400", "bold")
    pub weight: Option<String>,
    /// Font style (e.g., "normal", "italic")
    pub style: Option<String>,
}

/// Identifies a font variant: family + weight + style.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FontVariantKey {
    /// Font family name (e.g. "PT Sans")
    pub family: String,
    /// Numeric font weight (e.g. 400, 700)
    pub weight: u16,
    /// Font style: "normal" or "italic"
    pub style: String,
}

/// Result of analyzing CSS for font information
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FontAnalysis {
    /// Map of font-family name -> characters used (all variants combined)
    pub chars_per_font: HashMap<String, HashSet<char>>,
    /// Map of (family, weight, style) -> characters used with that specific variant
    pub chars_per_font_variant: HashMap<FontVariantKey, HashSet<char>>,
    /// Parsed @font-face rules
    pub font_faces: Vec<FontFace>,
}

/// Analyze HTML and CSS to collect font usage information
pub fn analyze_fonts(html: &str, css: &str) -> FontAnalysis {
    let (chars_per_font, chars_per_font_variant) = collect_chars_per_font(html, css);
    let font_faces = parse_font_face_rules(css);

    FontAnalysis {
        chars_per_font,
        chars_per_font_variant,
        font_faces,
    }
}

/// Extracts all text content and maps it to font-families based on CSS rules.
///
/// Returns both:
/// - A map of font-family name -> set of characters (all variants combined)
/// - A map of (family, weight, style) -> set of characters per variant
///
/// Each element's font is resolved with a simplified CSS cascade: user-agent
/// defaults, then author declarations ordered by `!important`, cascade layer,
/// specificity and source order, with inheritance and the CSS-wide keywords.
/// Characters from `::before`/`::after` `content` strings are included.
///
/// Rules that apply only under some condition (inside `@media`, `@supports`,
/// `@container` or `@scope`, or with `:hover`/`:focus`-style state
/// pseudo-classes) may or may not apply, independently of each other. An
/// element therefore has a set of possible fonts: the one the unconditional
/// rules give, plus each value a conditional rule could make win. Children
/// inherit from every possible parent font, and text counts for all of them, so
/// text whose font changes with the viewport or interaction stays covered.
pub fn collect_chars_per_font(
    html: &str,
    css: &str,
) -> (
    HashMap<String, HashSet<char>>,
    HashMap<FontVariantKey, HashSet<char>>,
) {
    let document = Html::parse_document(html);

    // First, parse CSS custom properties (variables)
    let css_vars = parse_css_custom_properties(css);

    // Parse font style rules with variable resolution
    let parsed = parse_css_rules(css, &css_vars);
    let (element_rules, pseudo_rules): (Vec<&FontStyleRule>, Vec<&FontStyleRule>) =
        parsed.rules.iter().partition(|rule| rule.pseudo.is_none());
    // Characters `open-quote`/`close-quote` can generate: the defaults and any `quotes`.
    let quote_chars = format!("{DEFAULT_QUOTES}{}", parsed.quote_chars);
    let marker_chars = format!("{DEFAULT_MARKERS}{}", parsed.marker_chars);

    let mut per_family: HashMap<String, HashSet<char>> = HashMap::new();
    let mut per_variant: HashMap<FontVariantKey, HashSet<char>> = HashMap::new();
    let mut add = |styles: &[ResolvedFontStyle], text: &str, transform: u8| {
        let mut chars: HashSet<char> = text.chars().collect();
        // text-transform renders other case forms of the source text
        if transform & TRANSFORM_UPPER != 0 {
            chars.extend(text.chars().flat_map(char::to_uppercase));
        }
        if transform & TRANSFORM_LOWER != 0 {
            chars.extend(text.chars().flat_map(char::to_lowercase));
        }
        for style in styles {
            let family_chars = per_family.entry(style.font_family.clone()).or_default();
            family_chars.extend(chars.iter().copied());
            let key = FontVariantKey {
                family: style.font_family.clone(),
                weight: style.font_weight,
                style: style.font_style.clone(),
            };
            per_variant
                .entry(key)
                .or_default()
                .extend(chars.iter().copied());
        }
    };

    // Elements are visited in document order, so a parent is always resolved
    // before its children.
    let root = (vec![ResolvedFontStyle::default()], 0u8);
    let mut resolved: HashMap<_, (Vec<ResolvedFontStyle>, u8)> = HashMap::new();
    let all_elements = Selector::parse("*").unwrap();
    for element in document.select(&all_elements) {
        let (parents, parent_transform) = element
            .ancestors()
            .find_map(scraper::ElementRef::wrap)
            .and_then(|parent| resolved.get(&parent.id()))
            .unwrap_or(&root);
        let inline = element
            .value()
            .attr("style")
            .and_then(|style| inline_style_rule(style, &css_vars));
        let matching: Vec<&FontStyleRule> = element_rules
            .iter()
            .copied()
            .filter(|rule| rule.selector_matches(&element))
            .chain(inline.as_ref())
            .collect();
        let tag = element.value().name();
        let styles = cascade(parents, Some(tag), &matching);
        let transform = matching.iter().fold(*parent_transform, |transform, rule| {
            transform | rule.text_transform
        });

        // Get all direct text nodes (not from children)
        let mut text: String = element
            .children()
            .filter_map(|child| child.value().as_text().map(|t| t.to_string()))
            .collect();
        if matches!(tag, "input" | "textarea") {
            // a form control's own value and placeholder text
            for attribute in ["value", "placeholder"] {
                if let Some(value) = element.value().attr(attribute) {
                    text.push_str(value);
                }
            }
        }
        if !text.trim().is_empty() {
            add(&styles, &text, transform);
        }

        for pseudo in [
            PseudoElement::Before,
            PseudoElement::After,
            PseudoElement::Marker,
        ] {
            let matching: Vec<&FontStyleRule> = pseudo_rules
                .iter()
                .copied()
                .filter(|rule| rule.pseudo == Some(pseudo) && rule.selector_matches(&element))
                .collect();
            // Conservatively union every matching content value instead of
            // resolving which one wins.
            let mut content = String::new();
            for generated in matching.iter().filter_map(|rule| rule.content.as_ref()) {
                content.push_str(&generated.text);
                if generated.quotes {
                    content.push_str(&quote_chars);
                }
                for attribute in &generated.attrs {
                    content.push_str(element.value().attr(attribute).unwrap_or_default());
                }
            }
            match (pseudo, tag) {
                // The user-agent stylesheet quotes `<q>` with open-quote/close-quote.
                (PseudoElement::Before | PseudoElement::After, "q") => {
                    content.push_str(&quote_chars)
                }
                // List item markers: bullets, or numbers for ordered lists.
                (PseudoElement::Marker, "li") => content.push_str(&marker_chars),
                _ => {}
            }
            if !content.trim().is_empty() {
                let transform = matching
                    .iter()
                    .fold(transform, |transform, rule| transform | rule.text_transform);
                add(&cascade(&styles, None, &matching), &content, transform);
            }
        }

        // ::first-line and ::first-letter style the start of the element's text, which
        // may be inside descendants: conservatively count all of it in their fonts. The
        // first letter inherits from the first line, which inherits from the element.
        let matching_pseudo = |kind: PseudoElement| -> Vec<&FontStyleRule> {
            pseudo_rules
                .iter()
                .copied()
                .filter(|rule| rule.pseudo == Some(kind) && rule.selector_matches(&element))
                .collect()
        };
        let first_line = matching_pseudo(PseudoElement::FirstLine);
        let first_letter = matching_pseudo(PseudoElement::FirstLetter);
        if !first_line.is_empty() || !first_letter.is_empty() {
            let text: String = element.text().collect();
            if !text.trim().is_empty() {
                let mut letter_parents = styles.clone();
                if !first_line.is_empty() {
                    let line_styles = cascade(&styles, None, &first_line);
                    let line_transform = first_line
                        .iter()
                        .fold(transform, |transform, rule| transform | rule.text_transform);
                    add(&line_styles, &text, line_transform);
                    for style in line_styles {
                        if !letter_parents.contains(&style) {
                            letter_parents.push(style);
                        }
                    }
                }
                if !first_letter.is_empty() {
                    let letter_transform = first_letter
                        .iter()
                        .chain(&first_line)
                        .fold(transform, |transform, rule| transform | rule.text_transform);
                    add(
                        &cascade(&letter_parents, None, &first_letter),
                        &text,
                        letter_transform,
                    );
                }
            }
        }

        resolved.insert(element.id(), (styles, transform));
    }

    (per_family, per_variant)
}

/// Characters of the default list markers (`disc`, `circle`, `square`, `decimal`).
const DEFAULT_MARKERS: &str = "\u{2022}\u{25E6}\u{25AA}0123456789. ";

/// Characters generated by `open-quote` and `close-quote` with the default `quotes`.
const DEFAULT_QUOTES: &str = "\u{201C}\u{201D}\u{2018}\u{2019}";

/// At most this many possible fonts are tracked per element; conditional rules
/// rarely produce more than a handful. Candidates are deduplicated first, so the
/// limit only applies to genuinely different fonts.
const MAX_POSSIBLE_STYLES: usize = 256;

/// Resolved font styling for an element.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedFontStyle {
    font_family: String,
    font_weight: u16,
    font_style: String,
}

impl Default for ResolvedFontStyle {
    fn default() -> Self {
        Self {
            font_family: "sans-serif".to_string(),
            font_weight: 400,
            font_style: "normal".to_string(),
        }
    }
}

/// The pseudo-elements whose generated `content` is analysed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PseudoElement {
    Before,
    After,
    Marker,
    /// `::first-line`: restyles the start of the element's (and its descendants') text.
    FirstLine,
    /// `::first-letter`: restyles its first letter; it inherits from `::first-line`.
    FirstLetter,
}

/// Uppercase or lowercase forms a `text-transform` can add.
const TRANSFORM_UPPER: u8 = 1;
const TRANSFORM_LOWER: u8 = 2;

/// A parsed `font-weight` value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FontWeightValue {
    Absolute(u16),
    Bolder,
    Lighter,
    Keyword(CssWideKeyword),
}

/// Generated text of a `content` value.
#[derive(Debug, Clone, PartialEq, Eq)]
struct GeneratedContent {
    text: String,
    /// Uses `open-quote`/`close-quote`.
    quotes: bool,
    /// Attributes whose values `attr()` inserts.
    attrs: Vec<String>,
}

/// A CSS rule (one complex selector) that sets font properties or generated content.
#[derive(Debug)]
struct FontStyleRule {
    /// The selector as written, without a trailing `::before`/`::after` (for debugging).
    #[allow(dead_code)]
    selector: String,
    font_family: Option<String>,
    font_weight: Option<FontWeightValue>,
    font_style: Option<String>,
    family_important: bool,
    weight_important: bool,
    style_important: bool,
    /// Generated text, for `::before`/`::after`/`::marker` rules.
    content: Option<GeneratedContent>,
    /// `TRANSFORM_UPPER`/`TRANSFORM_LOWER` forms its `text-transform` adds.
    text_transform: u8,
    pseudo: Option<PseudoElement>,
    /// Inside `@media`/`@supports`/`@container`/`@scope`, or matching only in an
    /// interaction state: the rule may or may not apply.
    conditional: bool,
    /// Its `@layer`'s position: the sibling index at each nesting level (empty when
    /// unlayered).
    layer: Vec<u32>,
    /// Inside `@scope`: scope proximity can make it win over later rules, so it is
    /// always a possible winner.
    scoped: bool,
    /// An element's `style` attribute.
    inline: bool,
    specificity: (u32, u32, u32),
    order: usize,
    /// The selector parsed for matching, in each interaction state it distinguishes
    /// (top-level state pseudo-classes removed); it matches if any variant matches.
    parsed: Vec<Selector>,
}

impl FontStyleRule {
    fn selector_matches(&self, element: &scraper::ElementRef) -> bool {
        self.parsed.iter().any(|selector| selector.matches(element))
    }

    /// Cascade order of one of this rule's declarations: importance, inline style,
    /// cascade layer, specificity, source order.
    fn priority(&self, important: bool) -> Priority {
        (
            important,
            self.inline,
            self.layer_order(important),
            self.specificity,
            self.order,
        )
    }

    /// Layer order, compared lexicographically: later sibling layers win, and a layer's
    /// own declarations (the trailing `u32::MAX`) beat its sublayers, as unlayered
    /// declarations beat every layer. Important declarations reverse the order.
    fn layer_order(&self, important: bool) -> Vec<u32> {
        self.layer
            .iter()
            .copied()
            .chain([u32::MAX])
            .map(|index| if important { u32::MAX - index } else { index })
            .collect()
    }
}

type Priority = (bool, bool, Vec<u32>, (u32, u32, u32), usize);

/// The user-agent origin's font for an element of `tag`, inheriting `parent`.
fn user_agent_style(parent: &ResolvedFontStyle, tag: Option<&str>) -> ResolvedFontStyle {
    // font-family, font-weight and font-style are inherited properties.
    let mut style = parent.clone();
    match tag {
        Some("h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "th") => style.font_weight = 700,
        Some("b" | "strong") => style.font_weight = bolder(parent.font_weight),
        Some("i" | "em" | "cite" | "dfn" | "var" | "address") => {
            style.font_style = "italic".to_string()
        }
        Some("button" | "input" | "select" | "textarea") => {
            // Form controls don't inherit the page font unless an author rule says so.
            style.font_family = "system-ui".to_string();
            style.font_weight = 400;
            style.font_style = "normal".to_string();
        }
        _ => {}
    }
    style
}

/// The declarations of one property that can win: the highest-priority unconditional
/// one (if any), and every conditional one that would beat it.
fn possible_winners<'a, T: Copy + PartialEq>(
    rules: &[&'a FontStyleRule],
    value: impl Fn(&'a FontStyleRule) -> Option<(T, bool)>,
    reverts_layer: impl Fn(T) -> bool,
) -> Vec<Option<T>> {
    // Every declaration once: (priority, value, conditional, scoped).
    let declarations: Vec<(Priority, T, bool, bool)> = rules
        .iter()
        .filter_map(|rule| {
            let (v, important) = value(rule)?;
            Some((rule.priority(important), v, rule.conditional, rule.scoped))
        })
        .collect();
    let mut memo = HashMap::new();
    possible_winners_below(&declarations, &reverts_layer, None, &mut memo)
}

/// (importance, inline, layer order): a `revert-layer` rollback admits declarations
/// strictly below its own cutoff in cascade order.
type Cutoff = (bool, bool, Vec<u32>);

/// The possible winners among declarations below `cutoff` (all of them for `None`).
/// Results are memoized by cutoff, so chains of `revert-layer` across many layers stay
/// linear in the number of cutoffs instead of exponential.
fn possible_winners_below<T: Copy + PartialEq>(
    declarations: &[(Priority, T, bool, bool)],
    reverts_layer: &dyn Fn(T) -> bool,
    cutoff: Option<&Cutoff>,
    memo: &mut HashMap<Option<Cutoff>, Vec<Option<T>>>,
) -> Vec<Option<T>> {
    let key = cutoff.cloned();
    if let Some(winners) = memo.get(&key) {
        return winners.clone();
    }
    let below = |p: &Priority| cutoff.is_none_or(|(i, n, l)| (p.0, p.1, &p.2) < (*i, *n, l));
    let admitted: Vec<&(Priority, T, bool, bool)> =
        declarations.iter().filter(|d| below(&d.0)).collect();
    let base = admitted
        .iter()
        .filter(|d| !d.2)
        .max_by(|a, b| a.0.cmp(&b.0))
        .copied();
    let mut winners: Vec<Option<T>> = Vec::new();
    let push = |winners: &mut Vec<Option<T>>, candidate: Option<T>| {
        if !winners.contains(&candidate) {
            winners.push(candidate);
        }
    };
    // revert-layer rolls back to the possible winners (conditional ones included) of
    // everything below its layer in cascade order: lower layers of the same importance,
    // then (for !important) every normal declaration; an inline style rolls back to the
    // stylesheets. The user-agent value (None) when there are none.
    let mut roll_back = |p: &Priority, winners: &mut Vec<Option<T>>| {
        let lower: Cutoff = (p.0, p.1, p.2.clone());
        for candidate in possible_winners_below(declarations, reverts_layer, Some(&lower), memo) {
            push(winners, candidate);
        }
    };
    match base {
        Some((p, v, _, _)) if reverts_layer(*v) => roll_back(p, &mut winners),
        Some((_, v, _, _)) => push(&mut winners, Some(*v)),
        None => push(&mut winners, None),
    }
    let base_priority = base.map(|(p, _, _, _)| p);
    for (p, v, _, scoped) in admitted.iter().copied().filter(|d| d.2) {
        // (scoped rules are always possible winners: proximity can beat source order)
        if !(*scoped || base_priority.is_none_or(|best| p > best)) {
            continue;
        }
        if reverts_layer(*v) {
            roll_back(p, &mut winners);
        } else {
            push(&mut winners, Some(*v));
        }
    }
    memo.insert(key, winners.clone());
    winners
}

/// Resolve the possible fonts of one element (or pseudo-element) from its parent's
/// possible fonts, its user-agent defaults and the author rules that match it.
fn cascade(
    parents: &[ResolvedFontStyle],
    tag: Option<&str>,
    rules: &[&FontStyleRule],
) -> Vec<ResolvedFontStyle> {
    let reverts = |value: &str| css_wide_keyword(value) == Some(CssWideKeyword::RevertLayer);
    let families = possible_winners(
        rules,
        |rule| {
            rule.font_family
                .as_deref()
                .map(|family| (family, rule.family_important))
        },
        reverts,
    );
    let weights = possible_winners(
        rules,
        |rule| {
            rule.font_weight
                .map(|weight| (weight, rule.weight_important))
        },
        |weight| weight == FontWeightValue::Keyword(CssWideKeyword::RevertLayer),
    );
    let font_styles = possible_winners(
        rules,
        |rule| {
            rule.font_style
                .as_deref()
                .map(|style| (style, rule.style_important))
        },
        reverts,
    );

    let mut out: Vec<ResolvedFontStyle> = Vec::new();
    for parent in parents {
        let ua = user_agent_style(parent, tag);
        for family in &families {
            let font_family = match family.map(|f| (f, css_wide_keyword(f))) {
                None => ua.font_family.clone(),
                Some((_, Some(CssWideKeyword::Inherit))) => parent.font_family.clone(),
                Some((_, Some(CssWideKeyword::Revert | CssWideKeyword::RevertLayer))) => {
                    ua.font_family.clone()
                }
                Some((_, Some(CssWideKeyword::Initial))) => "serif".to_string(),
                Some((family, None)) => family.to_string(),
            };
            for weight in &weights {
                let font_weight = match weight {
                    None
                    | Some(FontWeightValue::Keyword(
                        CssWideKeyword::Revert | CssWideKeyword::RevertLayer,
                    )) => ua.font_weight,
                    Some(FontWeightValue::Absolute(weight)) => *weight,
                    Some(FontWeightValue::Bolder) => bolder(parent.font_weight),
                    Some(FontWeightValue::Lighter) => lighter(parent.font_weight),
                    Some(FontWeightValue::Keyword(CssWideKeyword::Inherit)) => parent.font_weight,
                    Some(FontWeightValue::Keyword(CssWideKeyword::Initial)) => 400,
                };
                for style in &font_styles {
                    let font_style = match style.map(|s| (s, css_wide_keyword(s))) {
                        None
                        | Some((_, Some(CssWideKeyword::Revert | CssWideKeyword::RevertLayer))) => {
                            ua.font_style.clone()
                        }
                        Some((_, Some(CssWideKeyword::Inherit))) => parent.font_style.clone(),
                        Some((_, Some(CssWideKeyword::Initial))) => "normal".to_string(),
                        Some((style, None)) => style.to_string(),
                    };
                    let resolved = ResolvedFontStyle {
                        font_family: font_family.clone(),
                        font_weight,
                        font_style,
                    };
                    if !out.contains(&resolved) {
                        if out.len() >= MAX_POSSIBLE_STYLES {
                            return out;
                        }
                        out.push(resolved);
                    }
                }
            }
        }
    }
    out
}

/// `bolder` relative to the inherited weight (CSS Fonts 4, relative weights).
fn bolder(parent: u16) -> u16 {
    match parent {
        0..350 => 400,
        350..550 => 700,
        550..900 => 900,
        _ => parent,
    }
}

/// `lighter` relative to the inherited weight (CSS Fonts 4, relative weights).
fn lighter(parent: u16) -> u16 {
    match parent {
        0..100 => parent,
        100..550 => 100,
        550..750 => 400,
        _ => 700,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CssWideKeyword {
    /// `inherit`, and `unset`, which inherits for these inherited properties.
    Inherit,
    /// `revert`: the user-agent origin's value.
    Revert,
    /// `revert-layer`: the value from lower cascade layers (resolved in
    /// `possible_winners`; the user-agent value when no lower layer sets one).
    RevertLayer,
    Initial,
}

fn css_wide_keyword(value: &str) -> Option<CssWideKeyword> {
    match value.trim().to_ascii_lowercase().as_str() {
        "inherit" | "unset" => Some(CssWideKeyword::Inherit),
        "revert" => Some(CssWideKeyword::Revert),
        "revert-layer" => Some(CssWideKeyword::RevertLayer),
        "initial" => Some(CssWideKeyword::Initial),
        _ => None,
    }
}

/// Parse a CSS font-weight value.
fn parse_font_weight(value: &str) -> Option<FontWeightValue> {
    let value = value.trim().to_ascii_lowercase();
    if let Some(keyword) = css_wide_keyword(&value) {
        return Some(FontWeightValue::Keyword(keyword));
    }
    match value.as_str() {
        "normal" => Some(FontWeightValue::Absolute(400)),
        "bold" => Some(FontWeightValue::Absolute(700)),
        "lighter" => Some(FontWeightValue::Lighter),
        "bolder" => Some(FontWeightValue::Bolder),
        other => other
            .parse::<f32>()
            .ok()
            .filter(|weight| (1.0..=1000.0).contains(weight))
            .map(|weight| FontWeightValue::Absolute(weight.round() as u16)),
    }
}

/// Parse a CSS font-style value: `normal`, `italic`, `oblique` (with any angle) or a
/// CSS-wide keyword.
fn parse_font_style(value: &str) -> Option<String> {
    let style = value.trim().to_ascii_lowercase();
    if css_wide_keyword(&style).is_some() {
        return Some(style);
    }
    match style.split_whitespace().next()? {
        "normal" => Some("normal".to_string()),
        "italic" => Some("italic".to_string()),
        "oblique" => Some("oblique".to_string()),
        _ => None,
    }
}

/// Whitespace-separated component values at the top level, keeping functions such as
/// `calc(1rem + 2px)` and quoted strings whole, with `/` as a token of its own.
fn component_values(value: &str) -> Vec<&str> {
    let mut tokens = Vec::new();
    let mut rest = value.trim();
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix('/') {
            tokens.push("/");
            rest = after.trim_start();
            continue;
        }
        let end =
            find_top_level(rest, &[' ', '\t', '\n', '\r', '\u{c}', '/']).unwrap_or(rest.len());
        tokens.push(&rest[..end]);
        rest = rest[end..].trim_start();
    }
    tokens
}

/// The font properties set by a `font` shorthand: (family, weight, style).
fn parse_font_shorthand(
    value: &str,
    css_vars: &CssVariables,
) -> Option<(String, FontWeightValue, String)> {
    let value = resolve_css_var(value.trim(), css_vars);
    if let Some(keyword) = css_wide_keyword(&value) {
        return Some((
            value.trim().to_string(),
            FontWeightValue::Keyword(keyword),
            value.trim().to_string(),
        ));
    }
    // [style || variant || weight || stretch]? size [/ line-height]? family
    let mut weight = FontWeightValue::Absolute(400);
    let mut style = "normal".to_string();
    let tokens = component_values(&value);
    for (index, token) in tokens.iter().enumerate() {
        let lower = token.to_ascii_lowercase();
        if is_font_size(&lower) {
            let mut family_start = index + 1;
            if tokens.get(family_start) == Some(&"/") {
                family_start += 2; // skip the line height
            }
            let family = tokens.get(family_start..)?.join(" ");
            let family = parse_font_family_value_with_vars(&family, css_vars);
            return (!family.is_empty()).then_some((family, weight, style));
        }
        if let Some(parsed) = parse_font_style(&lower) {
            style = parsed;
        } else if let Some(parsed) = parse_font_weight(&lower) {
            weight = parsed;
        }
        // font-variant and font-stretch keywords are ignored
    }
    None // no size: a system font keyword or invalid
}

fn is_font_size(token: &str) -> bool {
    matches!(
        token,
        "xx-small"
            | "x-small"
            | "small"
            | "medium"
            | "large"
            | "x-large"
            | "xx-large"
            | "xxx-large"
            | "smaller"
            | "larger"
    ) || token.starts_with(|c: char| c.is_ascii_digit() || c == '.')
        && !token.chars().all(|c| c.is_ascii_digit())
        || ["calc(", "clamp(", "min(", "max(", "var("]
            .iter()
            .any(|function| token.starts_with(function))
}

/// Parse a `content` value into the characters it generates. Quoted strings (with CSS
/// escapes) produce text and the quote keywords are flagged; `attr()`, counters and
/// images are ignored.
fn parse_content(value: &str) -> Option<GeneratedContent> {
    let mut text = String::new();
    let mut quotes = false;
    let mut attrs = Vec::new();
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' | '\'' => text.push_str(&read_css_string(c, &mut chars)),
            _ if c.is_alphabetic() => {
                let mut word = String::from(c);
                while let Some(next) = chars.next_if(|c| c.is_alphanumeric() || *c == '-') {
                    word.push(next);
                }
                if word.eq_ignore_ascii_case("open-quote")
                    || word.eq_ignore_ascii_case("close-quote")
                {
                    quotes = true;
                } else if (word.eq_ignore_ascii_case("counter")
                    || word.eq_ignore_ascii_case("counters"))
                    && chars.next_if_eq(&'(').is_some()
                {
                    // counter(name, style): the counter's number in that list style
                    let mut argument = String::new();
                    while let Some(c) = chars.next() {
                        match c {
                            ')' => break,
                            '"' | '\'' => text.push_str(&read_css_string(c, &mut chars)),
                            _ => argument.push(c),
                        }
                    }
                    text.push_str(&counter_chars(&argument.to_ascii_lowercase()));
                } else if word.eq_ignore_ascii_case("attr") && chars.next_if_eq(&'(').is_some() {
                    // attr(name), attr(name type, "fallback"): the element's attribute value,
                    // and conservatively the fallback text too
                    let mut argument = String::new();
                    while let Some(c) = chars.next() {
                        match c {
                            ')' => break,
                            '"' | '\'' => text.push_str(&read_css_string(c, &mut chars)),
                            _ => argument.push(c),
                        }
                    }
                    if let Some(name) = argument.split([',', ' ']).next()
                        && !name.trim().is_empty()
                    {
                        attrs.push(name.trim().to_ascii_lowercase());
                    }
                }
            }
            _ => {}
        }
    }
    (!text.is_empty() || quotes || !attrs.is_empty()).then_some(GeneratedContent {
        text,
        quotes,
        attrs,
    })
}

/// Characters a `counter()`/`counters()` can render in the style its arguments name.
fn counter_chars(arguments: &str) -> String {
    let mut chars = String::from("0123456789-");
    if arguments.contains("roman") {
        chars.push_str("ivxlcdmIVXLCDM");
    }
    if arguments.contains("alpha") || arguments.contains("latin") {
        chars.extend('a'..='z');
        chars.extend('A'..='Z');
    }
    if arguments.contains("greek") {
        chars.extend('α'..='ω');
    }
    chars
}

/// Read a CSS string's value after its opening `quote`, decoding escapes.
fn read_css_string(quote: char, chars: &mut std::iter::Peekable<std::str::Chars>) -> String {
    let mut text = String::new();
    while let Some(c) = chars.next() {
        if c == quote || c == '\n' {
            break;
        }
        if c != '\\' {
            text.push(c);
            continue;
        }
        let mut hex = String::new();
        while hex.len() < 6 {
            match chars.next_if(char::is_ascii_hexdigit) {
                Some(digit) => hex.push(digit),
                None => break,
            }
        }
        if hex.is_empty() {
            if let Some(escaped) = chars.next()
                && escaped != '\n'
            {
                text.push(escaped);
            }
        } else {
            chars.next_if(|c| c.is_whitespace());
            if let Some(decoded) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                text.push(decoded);
            }
        }
    }
    text
}

/// Rules and other values read from a stylesheet.
struct ParsedCss {
    rules: Vec<FontStyleRule>,
    /// Characters of every `quotes` declaration.
    quote_chars: String,
    /// Each layer's child layer names, by the parent's full name ("" for the top level),
    /// in order of first appearance.
    layer_children: HashMap<String, Vec<String>>,
    /// Anonymous layers seen, for unique names.
    anonymous_layers: usize,
    /// Characters of every string `list-style-type` marker.
    marker_chars: String,
}

impl ParsedCss {
    /// The sibling index at each level of a full layer name (`a.inner` →
    /// [index of `a`, index of `inner` within `a`]), registering new layers in order.
    fn layer_position(&mut self, name: &str) -> Vec<u32> {
        let mut position = Vec::new();
        let mut parent = String::new();
        for part in name
            .split('.')
            .map(str::trim)
            .filter(|part| !part.is_empty())
        {
            let children = self.layer_children.entry(parent.clone()).or_default();
            let index = match children.iter().position(|child| child == part) {
                Some(index) => index,
                None => {
                    children.push(part.to_string());
                    children.len() - 1
                }
            };
            position.push(
                u32::try_from(index)
                    .unwrap_or(u32::MAX - 1)
                    .min(u32::MAX - 1),
            );
            if !parent.is_empty() {
                parent.push('.');
            }
            parent.push_str(part);
        }
        position
    }

    /// The context inside an at-rule block, or `None` for at-rules whose contents are
    /// not page styles (`@font-face`, `@keyframes`, `@page`, …).
    fn at_rule_context(&mut self, at_rule: &str, context: &Context) -> Option<Context> {
        let name_end = at_rule
            .find(|c: char| c.is_whitespace() || c == '(')
            .unwrap_or(at_rule.len());
        match at_rule[..name_end].to_ascii_lowercase().as_str() {
            "media" | "supports" | "container" | "document" | "-moz-document" => Some(Context {
                conditional: true,
                ..context.clone()
            }),
            "scope" => Some(Context {
                conditional: true,
                scoped: true,
                ..context.clone()
            }),
            "layer" => {
                let name = at_rule[name_end..].trim();
                let name = if name.is_empty() {
                    self.anonymous_layers += 1;
                    format!("\0anonymous{}", self.anonymous_layers)
                } else {
                    name.to_string()
                };
                let full = self.layer_name(context, &name);
                self.layer_position(&full);
                Some(Context {
                    layer: Some(full),
                    ..context.clone()
                })
            }
            _ => None,
        }
    }

    fn layer_name(&self, context: &Context, name: &str) -> String {
        match &context.layer {
            Some(parent) => format!("{parent}.{name}"),
            None => name.to_string(),
        }
    }

    /// `@layer a, b;` declares the layers' order.
    fn declare_layers(&mut self, names: &str, context: &Context) {
        for name in names
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            let full = self.layer_name(context, name);
            self.layer_position(&full);
        }
    }

    fn position_of(&mut self, context: &Context) -> Vec<u32> {
        match context.layer.clone() {
            Some(layer) => self.layer_position(&layer),
            None => Vec::new(),
        }
    }
}

/// Parse CSS and extract rules that set font-family, font-weight, font-style or the
/// `content` of `::before`/`::after`, with CSS variable resolution.
#[cfg_attr(not(test), allow(dead_code))] // (the tests read parsed rules)
fn parse_font_style_rules_with_vars(css: &str, css_vars: &CssVariables) -> Vec<FontStyleRule> {
    parse_css_rules(css, css_vars).rules
}

/// Selector lists become one rule per selector. Rules inside `@media`, `@supports`,
/// `@container` and `@scope` are conditional and rules inside `@layer` layered; other
/// at-rules (such as `@font-face` and `@keyframes`) are skipped.
fn parse_css_rules(css: &str, css_vars: &CssVariables) -> ParsedCss {
    let mut parsed = ParsedCss {
        rules: Vec::new(),
        quote_chars: String::new(),
        layer_children: HashMap::new(),
        anonymous_layers: 0,
        marker_chars: String::new(),
    };
    let css = strip_css_comments(css);
    parse_rule_list(&css, css_vars, Context::default(), &mut parsed);
    parsed
}

/// Remove `/* … */` comments outside strings.
fn strip_css_comments(css: &str) -> String {
    let mut out = String::with_capacity(css.len());
    let mut chars = css.chars().peekable();
    let mut quote: Option<char> = None;
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                out.push(c);
                if c == '\\' {
                    if let Some(escaped) = chars.next() {
                        out.push(escaped);
                    }
                } else if c == q || c == '\n' {
                    quote = None;
                }
            }
            None if c == '/' && chars.peek() == Some(&'*') => {
                chars.next();
                let mut previous = '\0';
                for c in chars.by_ref() {
                    if previous == '*' && c == '/' {
                        break;
                    }
                    previous = c;
                }
                out.push(' ');
            }
            None => {
                if c == '"' || c == '\'' {
                    quote = Some(c);
                } else if c == '\\' {
                    out.push(c);
                    if let Some(escaped) = chars.next() {
                        out.push(escaped);
                    }
                    continue;
                }
                out.push(c);
            }
        }
    }
    out
}

/// Visit `s` outside strings and escapes, with the bracket depth of each character;
/// stop when `visit` returns `true`, returning that byte index.
fn scan_top_level(s: &str, mut visit: impl FnMut(usize, char, i32) -> bool) -> Option<usize> {
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut chars = s.char_indices();
    while let Some((i, c)) = chars.next() {
        if c == '\\' {
            chars.next(); // the escaped character
            continue;
        }
        if let Some(q) = quote {
            // a string ends at its quote, or unterminated at a newline
            if c == q || c == '\n' {
                quote = None;
            }
            continue;
        }
        match c {
            '"' | '\'' => quote = Some(c),
            ')' | ']' => depth -= 1,
            _ => {}
        }
        if visit(i, c, depth) {
            return Some(i);
        }
        if matches!(c, '(' | '[') {
            depth += 1;
        }
    }
    None
}

/// Byte index of the first `target` at nesting depth zero outside strings and escapes.
fn find_top_level(s: &str, targets: &[char]) -> Option<usize> {
    scan_top_level(s, |_, c, depth| depth == 0 && targets.contains(&c))
}

/// Byte index of the `}` that closes a block whose `{` precedes `s`.
fn find_block_end(s: &str) -> Option<usize> {
    let mut braces = 1i32;
    scan_top_level(s, |_, c, _| {
        match c {
            '{' => braces += 1,
            '}' => braces -= 1,
            _ => {}
        }
        braces == 0
    })
}

fn split_top_level(s: &str, separator: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut rest = s;
    while let Some(i) = find_top_level(rest, &[separator]) {
        parts.push(&rest[..i]);
        rest = &rest[i + separator.len_utf8()..];
    }
    parts.push(rest);
    parts
}

/// The at-rule context of a rule list.
#[derive(Debug, Clone, Default)]
struct Context {
    conditional: bool,
    scoped: bool,
    /// The full name of the enclosing cascade layer.
    layer: Option<String>,
}

fn parse_rule_list(css: &str, css_vars: &CssVariables, context: Context, parsed: &mut ParsedCss) {
    let mut rest = css;
    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            return;
        }
        let Some(open) = find_top_level(rest, &['{', ';', '}']) else {
            return;
        };
        if !rest[open..].starts_with('{') {
            // a statement at-rule (`@import …;`, `@layer a, b;`), or a stray `;`/`}`
            if let Some(names) = rest[..open].trim().strip_prefix("@layer") {
                parsed.declare_layers(names, &context);
            }
            rest = &rest[open + 1..];
            continue;
        }
        let prelude = rest[..open].trim();
        let after_open = &rest[open + 1..];
        let Some(close) = find_block_end(after_open) else {
            return;
        };
        let block = &after_open[..close];
        rest = &after_open[close + 1..];

        if let Some(at_rule) = prelude.strip_prefix('@') {
            if let Some(inner) = parsed.at_rule_context(at_rule, &context) {
                parse_rule_list(block, css_vars, inner, parsed);
            }
            continue;
        }
        push_style_rules(prelude, block, css_vars, &context, parsed);
    }
}

/// A declaration value with its importance; a later declaration of the same property
/// replaces it unless it is `!important` and the later one isn't.
fn declare<T>(slot: &mut Option<(T, bool)>, value: T, important: bool) {
    if !slot
        .as_ref()
        .is_some_and(|(_, was_important)| *was_important && !important)
    {
        *slot = Some((value, important));
    }
}

/// The font-related declarations of one block.
#[derive(Default)]
struct Declarations {
    font_family: Option<(String, bool)>,
    font_weight: Option<(FontWeightValue, bool)>,
    font_style: Option<(String, bool)>,
    content: Option<GeneratedContent>,
    text_transform: u8,
}

impl Declarations {
    fn sets_font(&self) -> bool {
        self.font_family.is_some() || self.font_weight.is_some() || self.font_style.is_some()
    }

    fn is_empty(&self) -> bool {
        !self.sets_font() && self.content.is_none() && self.text_transform == 0
    }
}

/// Parse a declaration list (a rule's block without nested rules, or a `style` attribute).
fn parse_declarations(
    declarations: &str,
    css_vars: &CssVariables,
    quote_chars: &mut String,
    marker_chars: &mut String,
) -> Declarations {
    let mut out = Declarations::default();
    for declaration in split_top_level(declarations, ';') {
        let Some(colon) = find_top_level(declaration, &[':']) else {
            continue;
        };
        let name = declaration[..colon].trim().to_ascii_lowercase();
        let mut value = declaration[colon + 1..].trim();
        let mut important = false;
        if let Some(bang) = find_top_level(value, &['!'])
            && value[bang + 1..].trim().eq_ignore_ascii_case("important")
        {
            value = value[..bang].trim_end();
            important = true;
        }
        match name.as_str() {
            "font-family" => {
                // (an unresolved var() leaves no family: not a declaration)
                let family = parse_font_family_value_with_vars(value, css_vars);
                if !family.is_empty() {
                    declare(&mut out.font_family, family, important);
                }
            }
            "font-weight" => {
                if let Some(weight) = parse_font_weight(&resolve_css_var(value, css_vars)) {
                    declare(&mut out.font_weight, weight, important);
                }
            }
            "font-style" => {
                if let Some(style) = parse_font_style(&resolve_css_var(value, css_vars)) {
                    declare(&mut out.font_style, style, important);
                }
            }
            "font" => {
                if let Some((family, weight, style)) = parse_font_shorthand(value, css_vars) {
                    declare(&mut out.font_family, family, important);
                    declare(&mut out.font_weight, weight, important);
                    declare(&mut out.font_style, style, important);
                }
            }
            "all" => {
                // `all` resets every property, the font ones included
                let value = resolve_css_var(value, css_vars);
                if let Some(keyword) = css_wide_keyword(&value) {
                    let value = value.trim().to_ascii_lowercase();
                    declare(&mut out.font_family, value.clone(), important);
                    declare(
                        &mut out.font_weight,
                        FontWeightValue::Keyword(keyword),
                        important,
                    );
                    declare(&mut out.font_style, value, important);
                }
            }
            "content" => {
                // Conservatively union every content value in the block.
                if let Some(content) = parse_content(&resolve_css_var(value, css_vars)) {
                    match &mut out.content {
                        Some(existing) => {
                            existing.text.push_str(&content.text);
                            existing.quotes |= content.quotes;
                            existing.attrs.extend(content.attrs);
                        }
                        None => out.content = Some(content),
                    }
                }
            }
            "quotes" => {
                if let Some(quotes) = parse_content(&resolve_css_var(value, css_vars)) {
                    quote_chars.push_str(&quotes.text);
                }
            }
            "font-variant" | "font-variant-caps" => {
                // small caps are drawn from uppercase forms when the font has none
                let value = resolve_css_var(value, css_vars).to_ascii_lowercase();
                if ["small-caps", "petite-caps", "unicase", "titling-caps"]
                    .iter()
                    .any(|caps| value.contains(caps))
                {
                    out.text_transform |= TRANSFORM_UPPER;
                }
            }
            "list-style" | "list-style-type" => {
                // a string marker, such as list-style-type: "✓ "
                if let Some(marker) = parse_content(&resolve_css_var(value, css_vars)) {
                    marker_chars.push_str(&marker.text);
                }
            }
            "text-transform" => {
                // Conservatively add the transformed forms; `none` doesn't remove any.
                let value = resolve_css_var(value, css_vars).to_ascii_lowercase();
                if value.contains("uppercase") || value.contains("capitalize") {
                    out.text_transform |= TRANSFORM_UPPER;
                }
                if value.contains("lowercase") {
                    out.text_transform |= TRANSFORM_LOWER;
                }
            }
            _ => {}
        }
    }
    out
}

/// A rule block's contents in order: runs of declarations and nested rules.
enum BlockItem<'a> {
    Declarations(&'a str),
    Nested(&'a str, &'a str),
}

/// Split a rule's block into declaration runs and nested rules (CSS nesting), in order.
fn split_nested(block: &str) -> Vec<BlockItem<'_>> {
    let mut items = Vec::new();
    let mut rest = block;
    let mut offset = 0;
    while let Some(i) = find_top_level(&rest[offset..], &['{', ';']).map(|i| i + offset) {
        if rest[i..].starts_with(';') {
            offset = i + 1;
            continue;
        }
        // The nested rule's prelude starts after the run's last `;`.
        let prelude_start = rest[..i].rfind(';').map_or(0, |j| j + 1);
        if !rest[..prelude_start].trim().is_empty() {
            items.push(BlockItem::Declarations(&rest[..prelude_start]));
        }
        let after = &rest[i + 1..];
        let Some(close) = find_block_end(after) else {
            return items;
        };
        items.push(BlockItem::Nested(
            rest[prelude_start..i].trim(),
            &after[..close],
        ));
        rest = &after[close + 1..];
        offset = 0;
    }
    if !rest.trim().is_empty() {
        items.push(BlockItem::Declarations(rest));
    }
    items
}

/// A nested rule's selector list against its parent's (`&` is the parent, anywhere in
/// the selector, including inside functional pseudo-classes).
fn nest_selector(parent: &str, nested: &str) -> String {
    let parent = format!(":is({parent})");
    split_top_level(nested, ',')
        .into_iter()
        .map(|selector| {
            let selector = selector.trim();
            let mut ampersands = Vec::new();
            scan_top_level(selector, |i, c, _| {
                if c == '&' {
                    ampersands.push(i);
                }
                false
            });
            if ampersands.is_empty() {
                return format!("{parent} {selector}");
            }
            let mut out = String::new();
            let mut last = 0;
            for i in ampersands {
                out.push_str(&selector[last..i]);
                out.push_str(&parent);
                last = i + 1;
            }
            out.push_str(&selector[last..]);
            out
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn push_style_rules(
    prelude: &str,
    block: &str,
    css_vars: &CssVariables,
    context: &Context,
    parsed: &mut ParsedCss,
) {
    // Each declaration run becomes its own rules in source order, so declarations
    // after a nested rule still come after it.
    for item in split_nested(block) {
        match item {
            BlockItem::Declarations(run) => {
                let declarations = parse_declarations(
                    run,
                    css_vars,
                    &mut parsed.quote_chars,
                    &mut parsed.marker_chars,
                );
                if declarations.is_empty() {
                    continue;
                }
                for selector in split_top_level(prelude, ',') {
                    let selector = selector.trim();
                    if !selector.is_empty() && !selector.starts_with('@') {
                        push_rule(selector, &declarations, context, parsed);
                    }
                }
            }
            BlockItem::Nested(nested_prelude, nested_block) => {
                if let Some(at_rule) = nested_prelude.strip_prefix('@') {
                    if let Some(inner) = parsed.at_rule_context(at_rule, context) {
                        push_style_rules(prelude, nested_block, css_vars, &inner, parsed);
                    }
                } else {
                    let selector = nest_selector(prelude, nested_prelude);
                    push_style_rules(&selector, nested_block, css_vars, context, parsed);
                }
            }
        }
    }
}

/// Add one complex selector's rule.
fn push_rule(
    selector: &str,
    declarations: &Declarations,
    context: &Context,
    parsed: &mut ParsedCss,
) {
    let (base, target) = split_pseudo_element(selector);
    let Some(base) = base else {
        return; // another pseudo-element, such as ::placeholder or ::selection
    };
    let pseudo = match target {
        Target::Pseudo(pseudo) => Some(pseudo),
        Target::PartOfText(pseudo) => Some(pseudo),
        Target::Element => None,
    };
    let generates = matches!(
        pseudo,
        Some(PseudoElement::Before | PseudoElement::After | PseudoElement::Marker)
    );
    if !generates && !declarations.sets_font() && declarations.text_transform == 0 {
        return; // `content` only applies to pseudo-elements
    }
    let (variants, mut state_dependent) = strip_state_pseudo_classes(&base);
    let mut selectors: Vec<Selector> = variants
        .iter()
        .filter_map(|variant| Selector::parse(variant).ok())
        .collect();
    if selectors.is_empty() {
        // A pseudo-class the matcher doesn't support (`:lang()`, `:nth-child(2 of .x)`):
        // match without pseudo-classes, as a rule that may or may not apply.
        selectors = variants
            .iter()
            .filter_map(|variant| Selector::parse(&strip_pseudo_classes(variant)).ok())
            .collect();
        state_dependent = true;
    }
    let order = parsed.rules.len();
    let layer = parsed.position_of(context);
    parsed.rules.push(FontStyleRule {
        specificity: specificity(&base),
        parsed: selectors,
        selector: base,
        font_family: declarations.font_family.as_ref().map(|(v, _)| v.clone()),
        font_weight: declarations.font_weight.map(|(v, _)| v),
        font_style: declarations.font_style.as_ref().map(|(v, _)| v.clone()),
        family_important: declarations.font_family.as_ref().is_some_and(|(_, i)| *i),
        weight_important: declarations.font_weight.is_some_and(|(_, i)| i),
        style_important: declarations.font_style.as_ref().is_some_and(|(_, i)| *i),
        content: if generates {
            declarations.content.clone()
        } else {
            None
        },
        text_transform: declarations.text_transform,
        pseudo,
        conditional: context.conditional || state_dependent,
        layer,
        scoped: context.scoped,
        inline: false,
        order,
    });
}

/// A rule for an element's `style` attribute: it beats every selector rule of the same
/// importance.
fn inline_style_rule(style: &str, css_vars: &CssVariables) -> Option<FontStyleRule> {
    let declarations = parse_declarations(style, css_vars, &mut String::new(), &mut String::new());
    (declarations.sets_font() || declarations.text_transform != 0).then(|| FontStyleRule {
        selector: String::new(),
        font_family: declarations.font_family.as_ref().map(|(v, _)| v.clone()),
        font_weight: declarations.font_weight.map(|(v, _)| v),
        font_style: declarations.font_style.as_ref().map(|(v, _)| v.clone()),
        family_important: declarations.font_family.as_ref().is_some_and(|(_, i)| *i),
        weight_important: declarations.font_weight.is_some_and(|(_, i)| i),
        style_important: declarations.font_style.as_ref().is_some_and(|(_, i)| *i),
        content: None,
        text_transform: declarations.text_transform,
        pseudo: None,
        conditional: false,
        layer: Vec::new(),
        scoped: false,
        inline: true,
        specificity: (0, 0, 0),
        order: usize::MAX,
        parsed: Vec::new(),
    })
}

/// What a selector styles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    Element,
    Pseudo(PseudoElement),
    /// `::first-letter`/`::first-line`: part of the element's own text.
    PartOfText(PseudoElement),
}

/// Split a trailing pseudo-element off a selector: `::before`, `::after` and
/// `::marker` (or the legacy single-colon forms) generate text, `::first-letter` and
/// `::first-line` restyle part of the element's text. The base is `None` for other
/// pseudo-elements.
fn split_pseudo_element(selector: &str) -> (Option<String>, Target) {
    for (suffix, target) in [
        ("::before", Target::Pseudo(PseudoElement::Before)),
        ("::after", Target::Pseudo(PseudoElement::After)),
        ("::marker", Target::Pseudo(PseudoElement::Marker)),
        (
            "::first-letter",
            Target::PartOfText(PseudoElement::FirstLetter),
        ),
        ("::first-line", Target::PartOfText(PseudoElement::FirstLine)),
        (":before", Target::Pseudo(PseudoElement::Before)),
        (":after", Target::Pseudo(PseudoElement::After)),
        (
            ":first-letter",
            Target::PartOfText(PseudoElement::FirstLetter),
        ),
        (":first-line", Target::PartOfText(PseudoElement::FirstLine)),
    ] {
        let Some(start) = selector.len().checked_sub(suffix.len()) else {
            continue;
        };
        if !selector
            .get(start..)
            .is_some_and(|end| end.eq_ignore_ascii_case(suffix))
        {
            continue;
        }
        let base = &selector[..start];
        if base.ends_with('\\') || base.ends_with(':') {
            continue; // an escaped colon in a name, such as `.a\:before`
        }
        let base = if base.trim().is_empty() {
            "*".to_string()
        } else if base.ends_with(|c: char| c.is_whitespace() || matches!(c, '>' | '+' | '~')) {
            // `ul ::before` and `ul > ::before` apply to any matching descendant
            format!("{base}*")
        } else {
            base.to_string()
        };
        return (Some(base), target);
    }
    let mut other_pseudo_element = false;
    scan_top_level(selector, |i, c, _| {
        other_pseudo_element |= c == ':' && selector[i..].starts_with("::");
        false
    });
    if other_pseudo_element {
        return (None, Target::Element);
    }
    (Some(selector.to_string()), Target::Element)
}

/// Pseudo-classes for interaction states the static document never has.
const STATE_PSEUDO_CLASSES: [&str; 13] = [
    ":hover",
    ":focus-visible",
    ":focus-within",
    ":focus",
    ":active",
    ":visited",
    ":target",
    ":checked",
    ":indeterminate",
    ":popover-open",
    ":placeholder-shown",
    ":open",
    // `[open]` on <details>/<dialog>: toggled without scripts
    "[open]",
];

/// Rewrite interaction-state pseudo-classes so the static document can be matched.
/// Top-level ones are removed, matching as if the state applied. Ones inside functional
/// pseudo-classes (`:not(:hover)`, `:is(:focus)`) give two variants: the resting state
/// (`:not(*)`, never matching) and the active state (`*`). Returns the selectors to
/// match (the rule matches if any does) and whether it depends on a state (then the
/// rule may or may not apply).
fn strip_state_pseudo_classes(selector: &str) -> (Vec<String>, bool) {
    let mut found: Vec<(usize, usize, bool)> = Vec::new(); // (start, end, nested)
    scan_top_level(selector, |i, c, depth| {
        let pseudo_class =
            c == ':' && !selector[i..].starts_with("::") && !selector[..i].ends_with(':');
        if !pseudo_class && c != '[' {
            return false;
        }
        let rest = &selector[i..];
        for state in STATE_PSEUDO_CLASSES {
            let Some(after) = rest
                .get(..state.len())
                .filter(|prefix| prefix.eq_ignore_ascii_case(state))
                .map(|_| &rest[state.len()..])
            else {
                continue;
            };
            let ends_name = state.ends_with(']')
                || after
                    .chars()
                    .next()
                    .is_none_or(|c| !(c.is_alphanumeric() || c == '-' || c == '_' || c == '('));
            if ends_name {
                found.push((i, i + state.len(), depth > 0));
                break;
            }
        }
        false
    });
    if found.is_empty() {
        return (vec![selector.to_string()], false);
    }
    let any_nested = found.iter().any(|(_, _, nested)| *nested);
    // `resting`: nested states never match; otherwise every state applies (removed).
    let rewrite = |resting: bool| {
        let mut out = String::with_capacity(selector.len());
        let mut last = 0;
        for &(start, end, nested) in &found {
            out.push_str(&selector[last..start]);
            if nested && resting {
                out.push_str(":not(*)");
            } else if out.is_empty()
                || out.ends_with(|c: char| {
                    c.is_whitespace() || matches!(c, '>' | '+' | '~' | '(' | ',')
                })
            {
                out.push('*');
            }
            last = end;
        }
        out.push_str(&selector[last..]);
        out
    };
    let mut variants = vec![rewrite(true)];
    if any_nested {
        variants.push(rewrite(false));
    }
    (variants, true)
}

/// Remove every top-level pseudo-class (and its argument) from a selector.
fn strip_pseudo_classes(selector: &str) -> String {
    let mut starts = Vec::new();
    scan_top_level(selector, |i, c, depth| {
        if depth == 0
            && c == ':'
            && !selector[i..].starts_with("::")
            && !selector[..i].ends_with(':')
        {
            starts.push(i);
        }
        false
    });
    let mut out = String::with_capacity(selector.len());
    let mut last = 0;
    for start in starts {
        if start < last {
            continue;
        }
        out.push_str(&selector[last..start]);
        let rest = &selector[start + 1..];
        let name_end = rest
            .find(|c: char| !(c.is_alphanumeric() || c == '-' || c == '_'))
            .unwrap_or(rest.len());
        let mut end = start + 1 + name_end;
        if selector[end..].starts_with('(') {
            // the argument runs to its closing parenthesis
            end = match scan_top_level(&selector[end + 1..], |_, c, depth| c == ')' && depth == -1)
            {
                Some(close) => end + 1 + close + 1,
                None => selector.len(),
            };
        }
        if out.is_empty()
            || out.ends_with(|c: char| c.is_whitespace() || matches!(c, '>' | '+' | '~'))
        {
            out.push('*');
        }
        last = end;
    }
    out.push_str(&selector[last..]);
    out
}

/// Approximate CSS selector specificity as (ids, classes/attributes/pseudo-classes, types).
fn specificity(selector: &str) -> (u32, u32, u32) {
    let mut ids = 0;
    let mut classes = 0;
    let mut types = 0;
    let chars: Vec<char> = selector.chars().collect();
    let mut i = 0;
    let identifier = |c: char| c.is_alphanumeric() || c == '-' || c == '_' || !c.is_ascii();
    // Skip an identifier from `i`, including escapes such as `\:` and `\2c `.
    let skip_identifier = |mut i: usize| {
        while i < chars.len() {
            if chars[i] == '\\' {
                i = (i + 2).min(chars.len());
            } else if identifier(chars[i]) {
                i += 1;
            } else {
                break;
            }
        }
        i
    };
    // The index after the bracket matching the one at `open`, skipping strings.
    let skip_group = |open: usize, close: char| {
        let rest: String = chars[open + 1..].iter().collect();
        // (the closing bracket is the first at depth -1 relative to its contents)
        match scan_top_level(&rest, |_, c, depth| c == close && depth == -1) {
            Some(end) => open + 1 + rest[..end].chars().count() + 1,
            None => chars.len(),
        }
    };
    while i < chars.len() {
        match chars[i] {
            '#' => {
                ids += 1;
                i = skip_identifier(i + 1);
            }
            '.' => {
                classes += 1;
                i = skip_identifier(i + 1);
            }
            '[' => {
                classes += 1;
                i = skip_group(i, ']');
            }
            ':' => {
                let element = chars.get(i + 1) == Some(&':');
                let start = if element { i + 2 } else { i + 1 };
                i = skip_identifier(start);
                let name: String = chars[start..i]
                    .iter()
                    .collect::<String>()
                    .to_ascii_lowercase();
                let mut argument = None;
                if chars.get(i) == Some(&'(') {
                    let end = skip_group(i, ')');
                    argument = Some(
                        chars[i + 1..end.saturating_sub(1).max(i + 1)]
                            .iter()
                            .collect::<String>(),
                    );
                    i = end;
                }
                if element {
                    types += 1;
                    continue;
                }
                match (name.as_str(), argument) {
                    ("where", _) => {}
                    ("is" | "not" | "has" | "matches" | "-webkit-any", Some(argument)) => {
                        let max = split_top_level(&argument, ',')
                            .into_iter()
                            .map(specificity)
                            .max()
                            .unwrap_or_default();
                        ids += max.0;
                        classes += max.1;
                        types += max.2;
                    }
                    ("before" | "after" | "first-line" | "first-letter", None) => types += 1,
                    _ => classes += 1,
                }
            }
            '\\' => {
                types += 1;
                i = skip_identifier(i);
            }
            c if identifier(c) => {
                types += 1;
                i = skip_identifier(i);
            }
            _ => i += 1,
        }
    }
    (ids, classes, types)
}

/// Parse @font-face rules from CSS
fn parse_font_face_rules(css: &str) -> Vec<FontFace> {
    let mut faces = Vec::new();

    // Find all @font-face blocks
    let mut remaining = css;
    while let Some(start) = remaining.find("@font-face") {
        remaining = &remaining[start + "@font-face".len()..];

        // Find the opening brace
        let Some(brace_start) = remaining.find('{') else {
            break;
        };
        remaining = &remaining[brace_start + 1..];

        // Find matching closing brace (handle nested braces)
        let mut depth = 1;
        let mut block_end = 0;
        for (i, c) in remaining.char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        block_end = i;
                        break;
                    }
                }
                _ => {}
            }
        }

        if block_end == 0 {
            break;
        }

        let block = &remaining[..block_end];
        remaining = &remaining[block_end + 1..];

        // Parse the @font-face block
        if let Some(face) = parse_font_face_block(block) {
            faces.push(face);
        }
    }

    faces
}

/// Parse a single @font-face block content
fn parse_font_face_block(block: &str) -> Option<FontFace> {
    let mut family = None;
    let mut src = None;
    let mut weight = None;
    let mut style = None;

    for declaration in block.split(';') {
        let declaration = declaration.trim();

        if let Some(value) = declaration.strip_prefix("font-family:") {
            family = Some(parse_font_family_value(value));
        } else if let Some(value) = declaration.strip_prefix("src:") {
            src = parse_font_src(value);
        } else if let Some(value) = declaration.strip_prefix("font-weight:") {
            weight = Some(value.trim().to_string());
        } else if let Some(value) = declaration.strip_prefix("font-style:") {
            style = Some(value.trim().to_string());
        }
    }

    Some(FontFace {
        family: family?,
        src: src?,
        weight,
        style,
    })
}

/// Parse the src property of @font-face
/// Handles: url("/path/to/font.woff2"), url('/path'), url(path)
fn parse_font_src(value: &str) -> Option<String> {
    let value = value.trim();

    // Find url(...) - take the first one if there are multiple (fallbacks)
    let url_start = value.find("url(")?;
    let after_url = &value[url_start + 4..];

    // Find the closing paren
    let url_end = after_url.find(')')?;
    let url_content = &after_url[..url_end];

    // Remove quotes if present
    let url = url_content
        .trim()
        .trim_matches('"')
        .trim_matches('\'')
        .to_string();

    Some(url)
}

/// Parse a font-family value, returning the first (primary) font
/// If css_vars is provided, resolves var() references
fn parse_font_family_value(value: &str) -> String {
    parse_font_family_value_with_vars(value, &HashMap::new())
}

/// Parse a font-family value with CSS variable resolution
fn parse_font_family_value_with_vars(value: &str, css_vars: &CssVariables) -> String {
    let value = value.trim();

    // Resolve var() references first
    let resolved = resolve_css_var(value, css_vars);

    // font-family can be: "Font Name", 'Font Name', Font-Name, or a list
    // We take the first one
    let first = resolved.split(',').next().unwrap_or(&resolved).trim();

    // Remove quotes if present
    let first = first.trim_matches('"').trim_matches('\'');

    first.to_string()
}

/// Resolve CSS var() references in a value
/// Handles: var(--property-name) and var(--property-name, fallback)
fn resolve_css_var(value: &str, css_vars: &CssVariables) -> String {
    let mut result = value.to_string();

    // Keep resolving var() references until none remain (handles nested vars)
    let mut iterations = 0;
    const MAX_ITERATIONS: usize = 10; // Prevent infinite loops from circular references

    while let Some(var_start) = result.find("var(") {
        if iterations >= MAX_ITERATIONS {
            break;
        }
        iterations += 1;

        // Find matching closing paren (handle nested parens)
        let after_var = &result[var_start + 4..];
        let mut depth = 1;
        let mut var_end = None;
        for (i, c) in after_var.char_indices() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        var_end = Some(i);
                        break;
                    }
                }
                _ => {}
            }
        }

        let Some(end_offset) = var_end else {
            break; // Malformed var()
        };

        let var_content = &after_var[..end_offset];
        let full_var_end = var_start + 4 + end_offset + 1; // Include closing paren

        // Parse var content: --property-name or --property-name, fallback
        let (var_name, fallback) = if let Some(comma_pos) = var_content.find(',') {
            let name = var_content[..comma_pos].trim();
            let fallback = var_content[comma_pos + 1..].trim();
            (name, Some(fallback))
        } else {
            (var_content.trim(), None)
        };

        // Look up the variable value
        let replacement = css_vars
            .get(var_name)
            .map(|s| s.as_str())
            .or(fallback)
            .unwrap_or("");

        // Replace the var() with its resolved value
        result = format!(
            "{}{}{}",
            &result[..var_start],
            replacement,
            &result[full_var_end..]
        );
    }

    result
}

/// Parse CSS custom property declarations from CSS
/// Returns a map of --property-name -> value
fn parse_css_custom_properties(css: &str) -> CssVariables {
    let mut vars = HashMap::new();
    let css = strip_css_comments(css);
    // Every declaration list, at any depth (inside @media, @layer and @supports too):
    // the text before a `}` back to the previous `{`, `}` or `;`-run start.
    let mut list_start = 0;
    let mut declare = |declarations: &str| {
        for declaration in split_top_level(declarations, ';') {
            let declaration = declaration.trim();
            if declaration.starts_with("--")
                && let Some(colon) = declaration.find(':')
            {
                let name = declaration[..colon].trim();
                let value = declaration[colon + 1..].trim();
                // (the last definition wins; selector-scoped overrides are not tracked)
                vars.insert(name.to_string(), value.to_string());
            }
        }
    };
    scan_top_level(&css, |i, c, _| {
        match c {
            '{' => {
                // Declarations before a nested rule: up to the last `;` before its prelude.
                let before = &css[list_start..i];
                if let Some(end) = before.rfind(';') {
                    declare(&before[..end]);
                }
                list_start = i + 1;
            }
            '}' => {
                declare(&css[list_start..i]);
                list_start = i + 1;
            }
            _ => {}
        }
        false
    });
    vars
}

/// Extract CSS from HTML document (from `<style>` tags and inline styles)
pub fn extract_css_from_html(html: &str) -> String {
    let document = Html::parse_document(html);
    let style_selector = Selector::parse("style").unwrap();

    let mut css = String::new();

    for style in document.select(&style_selector) {
        css.push_str(&style.inner_html());
        css.push('\n');
    }

    css
}

#[cfg(test)]
mod tests {
    use super::*;

    fn variant_chars(html: &str, css: &str, family: &str, weight: u16, style: &str) -> String {
        let (_, variants) = collect_chars_per_font(html, css);
        let mut chars: Vec<char> = variants
            .get(&FontVariantKey {
                family: family.to_string(),
                weight,
                style: style.to_string(),
            })
            .map(|chars| chars.iter().copied().collect())
            .unwrap_or_default();
        chars.sort();
        chars.into_iter().collect()
    }

    #[test]
    fn font_family_inherit_takes_the_parent_family() {
        let css = "body { font-family: Body; } h3 { font-family: Heading; } \
                   .faq h3 { font-family: inherit; font-weight: 600; }";
        let html = "<body><div class=faq><h3>Q?</h3></div><h3>H</h3></body>";
        assert_eq!(variant_chars(html, css, "Body", 600, "normal"), "?Q");
        assert_eq!(variant_chars(html, css, "Heading", 700, "normal"), "H");
        assert_eq!(variant_chars(html, css, "inherit", 600, "normal"), "");
    }

    #[test]
    fn unset_and_initial_keywords() {
        let css = "body { font-family: Body; font-weight: 600; font-style: italic; } \
                   p { font-family: Other; font-weight: 300; } \
                   .u { font-family: unset; font-weight: inherit; } \
                   .i { font-weight: initial; font-style: initial; }";
        let html = "<body><p class=u>U</p><span class=i>I</span></body>";
        assert_eq!(variant_chars(html, css, "Body", 600, "italic"), "U");
        assert_eq!(variant_chars(html, css, "Body", 400, "normal"), "I");
    }

    #[test]
    fn specificity_beats_source_order() {
        let css = ".card .title { font-weight: 600; } h3 { font-weight: 800; } \
                   body { font-family: F; }";
        let html = "<body><div class=card><h3 class=title>A</h3></div><h3>B</h3></body>";
        assert_eq!(variant_chars(html, css, "F", 600, "normal"), "A");
        assert_eq!(variant_chars(html, css, "F", 800, "normal"), "B");
    }

    #[test]
    fn important_beats_specificity() {
        let css = "body { font-family: F; } #x.y { font-weight: 300; } p { font-weight: 500 !important; }";
        let html = "<body><p id=x class=y>A</p></body>";
        assert_eq!(variant_chars(html, css, "F", 500, "normal"), "A");
    }

    #[test]
    fn media_rules_count_for_both_variants() {
        let css = "body { font-family: F; } h2 { font-weight: 700; } \
                   @media (max-width: 600px) { .note { color: red; } h2 { font-weight: 600; } }";
        let html = "<body><h2>Wide</h2></body>";
        assert_eq!(variant_chars(html, css, "F", 700, "normal"), "Wdei");
        assert_eq!(variant_chars(html, css, "F", 600, "normal"), "Wdei");
    }

    #[test]
    fn hover_rules_count_for_both_variants() {
        let css = "body { font-family: F; } a:hover { font-weight: 700; } \
                   .f :focus-visible { font-style: italic; }";
        let html = "<body><a>Go</a><p class=f><span>Fo</span></p></body>";
        assert_eq!(variant_chars(html, css, "F", 400, "normal"), "FGo");
        assert_eq!(variant_chars(html, css, "F", 700, "normal"), "Go");
        assert_eq!(variant_chars(html, css, "F", 400, "italic"), "Fo");
    }

    #[test]
    fn pseudo_element_content_is_collected() {
        let css = r#"body { font-family: F; }
            li:not(:last-child)::after { content: ">"; }
            li a:before { content: "\2039  " attr(title); font-weight: 700; }
            .q::before { content: open-quote; }
            .none::after { content: none; }"#;
        let html =
            "<body><ul><li><a title=t>x</a></li><li class=none>y</li></ul><p class=q>z</p></body>";
        // "‹  " plus attr(title)
        assert_eq!(variant_chars(html, css, "F", 700, "normal"), " t‹");
        // plus the default list markers of the two `li`s
        assert_eq!(
            variant_chars(html, css, "F", 400, "normal"),
            " .0123456789>xyz‘’“”•▪◦"
        );
    }

    #[test]
    fn user_agent_defaults_and_relative_weights() {
        let css = "body { font-family: F; } .light { font-weight: 300; } .semi { font-weight: 600; } \
                   em { font-style: normal; }";
        let html = "<body><h4>H</h4><p class=light><b>L</b></p><p class=semi><strong>S</strong></p>\
                    <em>E</em><button>B</button></body>";
        assert_eq!(variant_chars(html, css, "F", 700, "normal"), "H");
        assert_eq!(variant_chars(html, css, "F", 400, "normal"), "EL");
        assert_eq!(variant_chars(html, css, "F", 900, "normal"), "S");
        assert_eq!(variant_chars(html, css, "system-ui", 400, "normal"), "B");
    }

    #[test]
    fn form_controls_inherit_when_told_to() {
        let css = "body { font-family: F; font-weight: 500; } button { font: inherit; }";
        let html = "<body><button>B</button></body>";
        assert_eq!(variant_chars(html, css, "F", 500, "normal"), "B");
    }

    #[test]
    fn font_shorthand_sets_family_weight_and_style() {
        let css = "body { font-family: F; } h1 { font: italic 700 2rem/1.2 \"Display\", serif; } \
                   p { font: 1rem G; }";
        let html = "<body><h1>D</h1><p><b>P</b></p></body>";
        assert_eq!(variant_chars(html, css, "Display", 700, "italic"), "D");
        assert_eq!(variant_chars(html, css, "G", 700, "normal"), "P");
    }

    #[test]
    fn selector_lists_comments_and_unsupported_at_rules() {
        let css = "/* a { font-family: X; } */ body { font-family: F; } \
                   @font-face { font-family: Skip; src: url(a.woff2); } \
                   @keyframes k { from { font-weight: 100; } } \
                   h1, .sub { font-weight: 800; } \
                   @supports (display: grid) { @media print { p { font-family: Print; } } }";
        let html = "<body><h1>A</h1><span class=sub>B</span><p>C</p></body>";
        assert_eq!(variant_chars(html, css, "F", 800, "normal"), "AB");
        assert_eq!(variant_chars(html, css, "Print", 400, "normal"), "C");
        assert_eq!(variant_chars(html, css, "X", 400, "normal"), "");
        assert_eq!(variant_chars(html, css, "F", 100, "normal"), "");
    }

    #[test]
    fn independent_media_rules_each_give_a_variant() {
        let css = "body{font-family:F}@media(max-width:600px){p{font-weight:600}}\
                   @media(min-width:601px){p{font-weight:700}}";
        let html = "<body><p>Q?</p></body>";
        for weight in [400, 600, 700] {
            assert_eq!(
                variant_chars(html, css, "F", weight, "normal"),
                "?Q",
                "weight {weight}"
            );
        }
    }

    #[test]
    fn conditional_parent_fonts_are_inherited() {
        let css = "body{font-family:F}@media(max-width:600px){.card{font-weight:600}}";
        let html = "<body><div class=card><span>S</span></div></body>";
        assert_eq!(variant_chars(html, css, "F", 600, "normal"), "S");
        assert_eq!(variant_chars(html, css, "F", 400, "normal"), "S");
    }

    #[test]
    fn negated_state_pseudo_classes_still_match() {
        let css = "body{font-family:F}p:not(:hover){font-weight:600}[data-x=\":hover\"]{font-style:italic}";
        let html = "<body><p>Q?</p><span data-x=\":hover\">I</span><span data-x=y>N</span></body>";
        assert_eq!(variant_chars(html, css, "F", 600, "normal"), "?Q");
        assert_eq!(variant_chars(html, css, "F", 400, "italic"), "I");
        // a hovered paragraph is 400
        assert_eq!(variant_chars(html, css, "F", 400, "normal"), "?NQ");
    }

    #[test]
    fn important_declaration_survives_a_later_one_in_its_block() {
        let css = "body{font-family:F}p{font-weight:600!important;font-weight:400}";
        assert_eq!(
            variant_chars("<body><p>Q?</p></body>", css, "F", 600, "normal"),
            "?Q"
        );
    }

    #[test]
    fn unlayered_rules_beat_layered_ones() {
        let css = "body{font-family:F}p{font-weight:600}@layer base{p{font-weight:400}}\
                   @layer base{.i{font-weight:800!important}}.i{font-weight:300!important}";
        let html = "<body><p>Q?</p><p class=i>I</p></body>";
        assert_eq!(variant_chars(html, css, "F", 600, "normal"), "?Q");
        assert_eq!(variant_chars(html, css, "F", 800, "normal"), "I");
    }

    #[test]
    fn scoped_rules_keep_the_unscoped_font() {
        let css = "body{font-family:F}@scope(.a){p{font-weight:600}}";
        let html = "<body><p>Q?</p></body>";
        assert_eq!(variant_chars(html, css, "F", 400, "normal"), "?Q");
    }

    #[test]
    fn font_shorthand_with_spaced_line_height_and_functions() {
        let css = "p{font:600 16px/ 1.5 Karla}h1{font:italic 700 calc(1rem + 2px) / 1.2 \"Display\", serif}";
        let html = "<body><p>P</p><h1>H</h1></body>";
        assert_eq!(variant_chars(html, css, "Karla", 600, "normal"), "P");
        assert_eq!(variant_chars(html, css, "Display", 700, "italic"), "H");
    }

    #[test]
    fn comment_openers_inside_strings_are_kept() {
        let css = "body{font-family:F}p::before{content:\"/*\"}p{font-weight:600}";
        let html = "<body><p>Q?</p></body>";
        assert_eq!(variant_chars(html, css, "F", 600, "normal"), "*/?Q");
    }

    #[test]
    fn escaped_selector_punctuation() {
        let css = "body{font-family:F}.x\\,y{font-weight:600}.a\\:b{font-weight:700}";
        let html = "<body><p class=\"x,y\">Q?</p><p class=\"a:b\">A</p></body>";
        assert_eq!(variant_chars(html, css, "F", 600, "normal"), "?Q");
        assert_eq!(variant_chars(html, css, "F", 700, "normal"), "A");
        assert_eq!(specificity(".a\\:b"), (0, 1, 0));
    }

    #[test]
    fn revert_restores_the_user_agent_value() {
        let css = "body{font-family:F}h3{font-weight:300}.r{font-weight:revert}";
        let html = "<body><h3 class=r>Q?</h3></body>";
        assert_eq!(variant_chars(html, css, "F", 700, "normal"), "?Q");
    }

    #[test]
    fn generated_quotes_use_the_pseudo_element_font_and_quotes() {
        let css = "body{font-family:Karla}q::before{font-family:Unbounded}\
                   p{quotes:\"«\" \"»\"}p::before{content:open-quote}";
        let html = "<body><q>Q?</q><p>P</p></body>";
        let unbounded = variant_chars(html, css, "Unbounded", 400, "normal");
        assert!(unbounded.contains('“'), "{unbounded}");
        assert!(variant_chars(html, css, "Karla", 400, "normal").contains('«'));
    }

    // Malformed or unusual CSS must never panic: the analysis runs on every page.
    #[test]
    fn unusual_css_does_not_panic() {
        let samples = [
            "p[data-x=\":aaaaé\"]{font-weight:600}",
            "é:hover{font-weight:600}",
            "p:hové{font-weight:600}",
            "a:hover\u{301}{font-weight:600}",
            ":hover{font-weight:600}",
            "p::befor{content:\"é\"}",
            "p:é:before{content:\"é}",
            "é::after{content:\"\\",
            "@media{",
            "}}}{{{p{font:",
            "p{font:é é é}",
            "p{font-weight:é;font-style:é;font-family:\"é}",
            ":is(é,){font-weight:1}",
            ":not({font-weight:1}",
            "[é{font-weight:1}",
            "p\\{font-weight:1}",
            "p{content:\"\\110000\"}p::before{content:\"\\ffffff \\\"}",
            "/* é",
            "\"é",
            "@layer é{@scope(é){@media é{p:hover:é::after{content:open-quote;quotes:\"é\"}}}}",
        ];
        for css in samples {
            let html = "<body><p data-x=\":aaaaé\">é</p><q>q</q><a>a</a></body>";
            let _ = collect_chars_per_font(html, css);
            let _ = specificity(css);
            let _ = strip_state_pseudo_classes(css);
            let _ = split_pseudo_element(css);
            let _ = component_values(css);
        }
    }

    #[test]
    fn escaped_brackets_and_quotes_outside_strings_keep_later_rules() {
        let css = r".c-\[\'x\'\]{color:red}.d-\(x{color:blue}.b{font-family:B}";
        let html = "<body><p class=b>Q?</p></body>";
        assert_eq!(variant_chars(html, css, "B", 400, "normal"), "?Q");
    }

    #[test]
    fn non_ascii_after_a_colon_does_not_panic() {
        let css = ".x:lang(日本語x){font-family:A}.y\\:日本{font-family:A}body{font-family:B}";
        let html = "<body><p class=x lang=ja>J</p></body>";
        let _ = collect_chars_per_font(html, css);
    }

    #[test]
    fn all_resets_the_font_properties() {
        let css = "body{font-family:A;font-weight:600}button{all:unset}";
        let html = "<body><button>B</button></body>";
        assert_eq!(variant_chars(html, css, "A", 600, "normal"), "B");
    }

    #[test]
    fn attr_content_uses_the_attribute_value() {
        let css = "body{font-family:F}td::before{content:attr(data-label) \": \";font-weight:700}";
        let html = "<body><table><tr><td data-label=Price>$1</td></tr></table></body>";
        assert_eq!(variant_chars(html, css, "F", 700, "normal"), " :Pceir");
    }

    #[test]
    fn text_transform_adds_the_transformed_case() {
        let css = "body{font-family:F}.u{text-transform:uppercase}.l::after{content:\"Ab\";text-transform:lowercase}";
        let html = "<body><div class=u><p>hi</p></div><p class=l>X</p></body>";
        assert_eq!(variant_chars(html, css, "F", 400, "normal"), "AHIXabhi");
    }

    #[test]
    fn nested_rules_are_read() {
        let css = "body{font-family:A}.a{font-weight:600;& .b{font-weight:700}.c{font-style:italic}\
                   @media (max-width:600px){font-weight:800}}";
        let html = "<body><div class=a>A<span class=b>B</span><span class=c>C</span></div></body>";
        assert_eq!(variant_chars(html, css, "A", 700, "normal"), "B");
        assert_eq!(variant_chars(html, css, "A", 600, "italic"), "C");
        // (.b always sets 700 itself)
        assert_eq!(variant_chars(html, css, "A", 800, "normal"), "A");
    }

    #[test]
    fn inline_styles_beat_selectors() {
        let css = "body{font-family:F}#x{font-weight:300}";
        let html = "<body><p id=x style=\"font-weight: 700\">I</p></body>";
        assert_eq!(variant_chars(html, css, "F", 700, "normal"), "I");
    }

    #[test]
    fn first_letter_markers_and_form_text() {
        let css = "body{font-family:F}p::first-letter{font-weight:700}li::marker{font-weight:600}\
                   ol li::marker{content:\"#\"}input{font:inherit}";
        let html = "<body><p>Pa</p><ol><li>x</li></ol><input placeholder=Where value=Mel></body>";
        assert_eq!(variant_chars(html, css, "F", 700, "normal"), "Pa");
        let marker = variant_chars(html, css, "F", 600, "normal");
        assert!(
            marker.contains('#') && marker.contains('•') && marker.contains('1'),
            "{marker}"
        );
        let normal = variant_chars(html, css, "F", 400, "normal");
        assert!(normal.contains('W') && normal.contains('M'), "{normal}");
    }

    #[test]
    fn print_and_screen_media_rules_both_count() {
        let css = "h1{font-family:A}@media (min-width:768px){h1{font-weight:800}}@media print{h1{font-weight:700}}";
        let html = "<body><h1>H</h1></body>";
        assert_eq!(variant_chars(html, css, "A", 800, "normal"), "H");
        assert_eq!(variant_chars(html, css, "A", 700, "normal"), "H");
    }

    #[test]
    fn duplicate_conditional_rules_stay_fast() {
        let css = format!(
            "body{{font-family:F}}{}",
            "@media(min-width:1px){p{font-family:F;font-weight:400;font-style:normal}}".repeat(100)
        );
        let html = format!("<body>{}</body>", "<p>Q?</p>".repeat(200));
        let started = std::time::Instant::now();
        assert_eq!(variant_chars(&html, &css, "F", 400, "normal"), "?Q");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn layer_order_and_inline_importance() {
        let css = "body{font-family:F}@layer a,b;@layer a{#x{font-weight:600}}@layer b{p{font-weight:400}}\
                   @layer a{.i{font-weight:700!important}}";
        let html =
            "<body><p id=x>Q?</p><p class=i style=\"font-weight:300!important\">I</p></body>";
        assert_eq!(variant_chars(html, css, "F", 400, "normal"), "?Q");
        assert_eq!(variant_chars(html, css, "F", 300, "normal"), "I");
    }

    #[test]
    fn scoped_rules_can_win_over_later_rules() {
        let css = "body{font-family:F}@scope(.card){p{font-weight:700}}p{font-weight:400}";
        let html = "<body><div class=card><p>Q?</p></div></body>";
        assert_eq!(variant_chars(html, css, "F", 700, "normal"), "?Q");
    }

    #[test]
    fn revert_layer_falls_back_to_the_lower_layer() {
        let css = "body{font-family:F}@layer a,b;@layer a{p{font-weight:600}}@layer b{p{font-weight:revert-layer}}";
        assert_eq!(
            variant_chars("<body><p>Q?</p></body>", css, "F", 600, "normal"),
            "?Q"
        );
    }

    #[test]
    fn positive_functional_state_selectors_can_match() {
        let css = "body{font-family:F}p:is(:hover){font-weight:700}";
        let html = "<body><p>Q?</p></body>";
        assert_eq!(variant_chars(html, css, "F", 700, "normal"), "?Q");
        assert_eq!(variant_chars(html, css, "F", 400, "normal"), "?Q");
    }

    #[test]
    fn every_content_value_in_a_block_counts() {
        let css = "body{font-family:F}p::before{content:\"?\"!important;content:\"x\"}";
        assert_eq!(
            variant_chars("<body><p>X</p></body>", css, "F", 400, "normal"),
            "?Xx"
        );
    }

    #[test]
    fn nesting_inside_functional_selectors_and_declaration_order() {
        let css = "body{font-family:F}.a{:is(&){font-weight:700}}.b{font-weight:400;&{font-weight:700}font-weight:600}";
        let html = "<body><div class=a>A</div><p class=b>B</p></body>";
        assert_eq!(variant_chars(html, css, "F", 700, "normal"), "A");
        assert_eq!(variant_chars(html, css, "F", 600, "normal"), "B");
    }

    #[test]
    fn a_trailing_escape_does_not_panic() {
        let css = "p:\\ {font-family:F}p.\\{font-weight:700}";
        let _ = collect_chars_per_font("<body><p>Q</p></body>", css);
        let _ = specificity("p:\\");
        let _ = specificity("\\");
    }

    #[test]
    fn attr_fallback_text_counts() {
        let css = "body{font-family:F}p::before{content:attr(data-x,\"?\")}";
        assert_eq!(
            variant_chars("<body><p>X</p></body>", css, "F", 400, "normal"),
            "?X"
        );
    }

    #[test]
    fn first_letter_covers_text_in_styled_descendants() {
        let css = "body{font-family:F}p::first-letter{font-weight:700}span{font-weight:400}";
        assert_eq!(
            variant_chars(
                "<body><p><span>Q?</span></p></body>",
                css,
                "F",
                700,
                "normal"
            ),
            "?Q"
        );
    }

    #[test]
    fn first_letter_wins_over_a_more_specific_element_rule() {
        let css = ".prose p{font-family:F}p::first-letter{font-family:Drop}";
        let html = "<body><div class=prose><p>Abc</p></div></body>";
        assert_eq!(variant_chars(html, css, "Drop", 400, "normal"), "Abc");
    }

    #[test]
    fn state_pseudo_classes_in_has_and_nested_parents_can_match() {
        let css = "body{font-family:F}.card:has(a:hover) h3{font-weight:800}\
                   :is(.x:hover) span{font-style:italic}.btn:hover{.icon{font-weight:600}}";
        let html = "<body><div class=card><a>l</a><h3>H</h3></div><p class=x><span>S</span></p>\
                    <span class=btn><span class=icon>I</span></span></body>";
        assert_eq!(variant_chars(html, css, "F", 800, "normal"), "H");
        assert_eq!(variant_chars(html, css, "F", 400, "italic"), "S");
        assert_eq!(variant_chars(html, css, "F", 600, "normal"), "I");
    }

    #[test]
    fn later_layers_beat_more_specific_earlier_ones() {
        let css = "body{font-family:F}@layer base, utilities;@layer utilities{.font-x{font-family:X}}\
                   @layer base{.card h2{font-family:B}}";
        let html = "<body><div class=card><h2 class=font-x>T</h2></div></body>";
        assert_eq!(variant_chars(html, css, "X", 700, "normal"), "T");
    }

    #[test]
    fn custom_properties_inside_at_rules_and_unresolved_vars() {
        let css = "@layer theme{:root{--font-sans:Inter}}body{font-family:var(--font-sans)}\
                   p{font-family:var(--missing)}";
        let html = "<body><p>P</p></body>";
        assert_eq!(variant_chars(html, css, "Inter", 400, "normal"), "P");
    }

    #[test]
    fn counters_render_digits() {
        let css = "body{font-family:F}li::after{content:counter(s) \". \"}ol::before{content:counter(n, upper-roman)}";
        let html = "<body><ol><li>x</li></ol></body>";
        let chars = variant_chars(html, css, "F", 400, "normal");
        assert!(
            chars.contains('7') && chars.contains('.') && chars.contains('X'),
            "{chars}"
        );
    }

    #[test]
    fn css_only_toggles_count_both_ways() {
        let css = "body{font-family:F}input:checked+label{font-weight:700}details[open] summary{font-style:italic}";
        let html = "<body><input type=checkbox><label>L</label><details><summary>S</summary></details></body>";
        assert_eq!(variant_chars(html, css, "F", 700, "normal"), "L");
        assert_eq!(variant_chars(html, css, "F", 400, "italic"), "S");
    }

    #[test]
    fn unsupported_pseudo_classes_still_match() {
        let css =
            "body{font-family:F}p:lang(en){font-weight:700}li:nth-child(2 of .k){font-weight:600}";
        let html = "<body><p lang=en>P</p><ul><li class=k>A</li><li class=k>B</li></ul></body>";
        assert_eq!(variant_chars(html, css, "F", 700, "normal"), "P");
        assert!(variant_chars(html, css, "F", 600, "normal").contains('B'));
    }

    #[test]
    fn string_markers_small_caps_and_unterminated_strings() {
        let css = "body{font-family:F}ul{list-style-type:\"\\2713  \"}.sc{font-variant:small-caps}\
                   p::after{content:\"x\n}.after{font-weight:700}";
        let html = "<body><ul><li>i</li></ul><p class=sc>ab</p><span class=after>A</span></body>";
        let normal = variant_chars(html, css, "F", 400, "normal");
        assert!(
            normal.contains('✓') && normal.contains('A') && normal.contains('B'),
            "{normal}"
        );
        assert_eq!(variant_chars(html, css, "F", 700, "normal"), "A");
    }

    #[test]
    fn first_line_and_first_letter_each_keep_their_font() {
        let css = "body{font-family:Karla}p::first-line{font-family:\"PT Sans\"}\
                   p::first-letter{font-family:Unbounded}";
        let html = "<body><p>Abc</p></body>";
        assert_eq!(variant_chars(html, css, "PT Sans", 400, "normal"), "Abc");
        assert_eq!(variant_chars(html, css, "Unbounded", 400, "normal"), "Abc");
        assert_eq!(variant_chars(html, css, "Karla", 400, "normal"), "Abc");
    }

    #[test]
    fn first_letter_inherits_from_first_line() {
        let css =
            "body{font-family:F}p::first-line{font-weight:700}p::first-letter{font-style:italic}";
        assert_eq!(
            variant_chars("<body><p>Ab</p></body>", css, "F", 700, "italic"),
            "Ab"
        );
    }

    #[test]
    fn nested_layers_rank_within_their_parent() {
        let css = "body{font-family:F}@layer a,b;@layer a{@layer inner{p{font-weight:700}}}\
                   @layer b{p{font-weight:400}}";
        let html = "<body><p>Q?</p></body>";
        assert_eq!(variant_chars(html, css, "F", 400, "normal"), "?Q");
        assert_eq!(variant_chars(html, css, "F", 700, "normal"), "");
    }

    #[test]
    fn a_layers_own_declarations_beat_its_sublayers() {
        let css = "body{font-family:F}@layer a{p{font-weight:600}@layer inner{p{font-weight:800}}}";
        let html = "<body><p>Q</p></body>";
        assert_eq!(variant_chars(html, css, "F", 600, "normal"), "Q");
        assert_eq!(variant_chars(html, css, "F", 800, "normal"), "");
    }

    #[test]
    fn revert_layer_rolls_back_the_whole_layer() {
        let css = "body{font-family:F}@layer a,b;@layer a{p{font-weight:600}}\
                   @layer b{p{font-weight:700;font-weight:revert-layer}}";
        let html = "<body><p>Q?</p></body>";
        assert_eq!(variant_chars(html, css, "F", 600, "normal"), "?Q");
        assert_eq!(variant_chars(html, css, "F", 700, "normal"), "");
    }

    #[test]
    fn revert_layer_without_a_lower_layer_uses_the_user_agent_value() {
        let css = "body{font-family:F}@layer b{h3{font-weight:300}.r{font-weight:revert-layer}}";
        assert_eq!(
            variant_chars("<body><h3 class=r>H</h3></body>", css, "F", 700, "normal"),
            "H"
        );
    }

    #[test]
    fn custom_properties_before_a_nested_rule() {
        let css = ".a{--f:G;.z{color:red}}body{font-family:var(--f)}";
        assert_eq!(
            variant_chars("<body><p>P</p></body>", css, "G", 400, "normal"),
            "P"
        );
    }

    #[test]
    fn revert_layer_keeps_conditional_fonts_from_lower_layers() {
        let css = "body{font-family:F}@layer a,b;@layer a{p{font-weight:400}@media(min-width:1px){p{font-weight:700}}}\
                   @layer b{p{font-weight:revert-layer}}";
        let html = "<body><p>Q</p></body>";
        assert_eq!(variant_chars(html, css, "F", 400, "normal"), "Q");
        assert_eq!(variant_chars(html, css, "F", 700, "normal"), "Q");
    }

    #[test]
    fn inline_important_revert_layer_rolls_back_to_the_stylesheets() {
        let css = "body{font-family:F}p{font-family:Author!important}";
        let html = "<body><p style=\"font-family:revert-layer!important\">Q</p></body>";
        assert_eq!(variant_chars(html, css, "Author", 400, "normal"), "Q");
    }

    #[test]
    fn important_revert_layer_falls_back_to_normal_declarations() {
        // a layered !important revert-layer with no lower !important: the normal winner
        let css = "body{font-family:F}@layer a,b;@layer a{p{font-weight:600}}\
                   @layer b{p{font-weight:revert-layer!important}}";
        assert_eq!(
            variant_chars("<body><p>Q</p></body>", css, "F", 600, "normal"),
            "Q"
        );
        // an inline !important revert-layer with no stylesheet !important: the normal winner
        let css = "body{font-family:F}p{font-family:Sheet}";
        let html = "<body><p style=\"font-family:revert-layer!important\">Q</p></body>";
        assert_eq!(variant_chars(html, css, "Sheet", 400, "normal"), "Q");
    }

    #[test]
    fn chained_conditional_revert_layers_stay_fast() {
        // 20 layers, each with a conditional revert-layer and a normal weight: the
        // rollbacks share their lower-layer results instead of recomputing them.
        let mut css = String::from("body{font-family:F}@layer ");
        css.push_str(
            &(0..20)
                .map(|i| format!("l{i}"))
                .collect::<Vec<_>>()
                .join(","),
        );
        css.push(';');
        for i in 0..20 {
            css.push_str(&format!(
                "@layer l{i}{{p{{font-weight:{}}}@media(min-width:{i}px){{p{{font-weight:revert-layer}}}}}}",
                100 + i * 10
            ));
        }
        let html = format!("<body>{}</body>", "<p>Q</p>".repeat(50));
        let started = std::time::Instant::now();
        let (_, variants) = collect_chars_per_font(&html, &css);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "{:?}",
            started.elapsed()
        );
        // every layer's weight is a possible font (each revert-layer can roll back)
        for i in 0..20 {
            let key = FontVariantKey {
                family: "F".to_string(),
                weight: 100 + i * 10,
                style: "normal".to_string(),
            };
            assert!(variants.contains_key(&key), "weight {}", 100 + i * 10);
        }
    }

    #[test]
    fn specificity_counts() {
        assert_eq!(specificity("h3"), (0, 0, 1));
        assert_eq!(specificity(".a .b h3"), (0, 2, 1));
        assert_eq!(specificity("#x li:not(:last-child)"), (1, 1, 1));
        assert_eq!(specificity(":where(.a) p"), (0, 0, 1));
        assert_eq!(specificity(":is(#a, .b) [data-x=\"1\"]"), (1, 1, 0));
        assert_eq!(specificity("*"), (0, 0, 0));
    }

    #[test]
    fn test_parse_font_family_rules() {
        let css = r#"
            body { font-family: "Inter", sans-serif; }
            h1 { font-family: 'Playfair Display'; }
            .code { font-family: monospace; }
        "#;

        let rules = parse_font_style_rules_with_vars(css, &HashMap::new());
        assert_eq!(rules.len(), 3);
        assert_eq!(rules[0].selector, "body");
        assert_eq!(rules[0].font_family.as_deref(), Some("Inter"));
        assert_eq!(rules[1].font_family.as_deref(), Some("Playfair Display"));
        assert_eq!(rules[2].font_family.as_deref(), Some("monospace"));
    }

    #[test]
    fn test_collect_chars_basic() {
        let html = r#"
            <html>
            <head>
                <style>
                    body { font-family: "TestFont"; }
                </style>
            </head>
            <body>
                <p>Hello</p>
            </body>
            </html>
        "#;

        let css = extract_css_from_html(html);
        let (chars, _) = collect_chars_per_font(html, &css);

        assert!(chars.contains_key("TestFont"));
        let test_font_chars = &chars["TestFont"];
        assert!(test_font_chars.contains(&'H'));
        assert!(test_font_chars.contains(&'e'));
        assert!(test_font_chars.contains(&'l'));
        assert!(test_font_chars.contains(&'o'));
    }

    #[test]
    fn test_different_fonts_for_elements() {
        let html = r#"
            <html>
            <head>
                <style>
                    body { font-family: "BodyFont"; }
                    h1 { font-family: "HeadingFont"; }
                </style>
            </head>
            <body>
                <h1>Title</h1>
                <p>Body text</p>
            </body>
            </html>
        "#;

        let css = extract_css_from_html(html);
        let (chars, _) = collect_chars_per_font(html, &css);

        // h1 should use HeadingFont
        assert!(chars.contains_key("HeadingFont"));
        assert!(chars["HeadingFont"].contains(&'T'));

        // p should inherit from body -> BodyFont
        assert!(chars.contains_key("BodyFont"));
        assert!(chars["BodyFont"].contains(&'B'));
    }

    #[test]
    fn test_parse_font_face_rules() {
        let css = r#"
            @font-face {
                font-family: "Inter";
                src: url("/fonts/Inter-Regular.woff2") format("woff2");
                font-weight: 400;
                font-style: normal;
            }

            @font-face {
                font-family: "Inter";
                src: url('/fonts/Inter-Bold.woff2');
                font-weight: 700;
            }

            @font-face {
                font-family: 'Playfair Display';
                src: url(fonts/Playfair.ttf);
            }

            body { font-family: "Inter", sans-serif; }
        "#;

        let faces = parse_font_face_rules(css);
        assert_eq!(faces.len(), 3);

        assert_eq!(faces[0].family, "Inter");
        assert_eq!(faces[0].src, "/fonts/Inter-Regular.woff2");
        assert_eq!(faces[0].weight, Some("400".to_string()));
        assert_eq!(faces[0].style, Some("normal".to_string()));

        assert_eq!(faces[1].family, "Inter");
        assert_eq!(faces[1].src, "/fonts/Inter-Bold.woff2");
        assert_eq!(faces[1].weight, Some("700".to_string()));
        assert_eq!(faces[1].style, None);

        assert_eq!(faces[2].family, "Playfair Display");
        assert_eq!(faces[2].src, "fonts/Playfair.ttf");
    }

    #[test]
    fn test_analyze_fonts_full() {
        let html = r#"
            <html>
            <head>
                <style>
                    @font-face {
                        font-family: "MyFont";
                        src: url("/fonts/MyFont.woff2");
                    }
                    body { font-family: "MyFont"; }
                </style>
            </head>
            <body>
                <p>Hello World</p>
            </body>
            </html>
        "#;

        let css = extract_css_from_html(html);
        let analysis = analyze_fonts(html, &css);

        // Should have the font-face
        assert_eq!(analysis.font_faces.len(), 1);
        assert_eq!(analysis.font_faces[0].family, "MyFont");
        assert_eq!(analysis.font_faces[0].src, "/fonts/MyFont.woff2");

        // Should have collected chars for MyFont
        assert!(analysis.chars_per_font.contains_key("MyFont"));
        let chars = &analysis.chars_per_font["MyFont"];
        assert!(chars.contains(&'H'));
        assert!(chars.contains(&'W'));
    }

    #[test]
    fn test_per_variant_chars() {
        let html = r#"
            <html>
            <head>
                <style>
                    body { font-family: "TestFont"; font-weight: 400; }
                    h1 { font-weight: 700; }
                    em { font-style: italic; }
                </style>
            </head>
            <body>
                <h1>Bold</h1>
                <p>Normal <em>italic</em></p>
            </body>
            </html>
        "#;

        let css = extract_css_from_html(html);
        let analysis = analyze_fonts(html, &css);

        // Per-family map should have all chars under "TestFont"
        let all_chars = &analysis.chars_per_font["TestFont"];
        assert!(all_chars.contains(&'B')); // from "Bold"
        assert!(all_chars.contains(&'N')); // from "Normal "
        assert!(all_chars.contains(&'i')); // from "italic"

        // Per-variant map should separate by weight/style
        let normal_400 = &analysis.chars_per_font_variant[&FontVariantKey {
            family: "TestFont".to_string(),
            weight: 400,
            style: "normal".to_string(),
        }];
        assert!(
            normal_400.contains(&'N'),
            "400 normal should have 'N' from 'Normal '"
        );
        assert!(
            !normal_400.contains(&'B'),
            "400 normal should NOT have 'B' from bold h1"
        );

        let bold_700 = &analysis.chars_per_font_variant[&FontVariantKey {
            family: "TestFont".to_string(),
            weight: 700,
            style: "normal".to_string(),
        }];
        assert!(
            bold_700.contains(&'B'),
            "700 normal should have 'B' from 'Bold'"
        );

        let italic_400 = &analysis.chars_per_font_variant[&FontVariantKey {
            family: "TestFont".to_string(),
            weight: 400,
            style: "italic".to_string(),
        }];
        assert!(
            italic_400.contains(&'i'),
            "400 italic should have 'i' from 'italic'"
        );
    }

    #[test]
    fn test_per_variant_html_bold_italic_tags() {
        let html = r#"
            <html>
            <head>
                <style>
                    body { font-family: "MyFont"; }
                </style>
            </head>
            <body>
                <p>Normal <b>bold</b> <i>italic</i> <strong>strong</strong></p>
            </body>
            </html>
        "#;

        let css = extract_css_from_html(html);
        let analysis = analyze_fonts(html, &css);

        // <b> and <strong> should be weight 700
        let bold = &analysis.chars_per_font_variant[&FontVariantKey {
            family: "MyFont".to_string(),
            weight: 700,
            style: "normal".to_string(),
        }];
        assert!(
            bold.contains(&'b'),
            "bold variant should have 'b' from <b>bold</b>"
        );
        assert!(
            bold.contains(&'s'),
            "bold variant should have 's' from <strong>strong</strong>"
        );

        // <i> should be italic
        let italic = &analysis.chars_per_font_variant[&FontVariantKey {
            family: "MyFont".to_string(),
            weight: 400,
            style: "italic".to_string(),
        }];
        assert!(
            italic.contains(&'i'),
            "italic variant should have 'i' from <i>italic</i>"
        );

        // Normal text
        let normal = &analysis.chars_per_font_variant[&FontVariantKey {
            family: "MyFont".to_string(),
            weight: 400,
            style: "normal".to_string(),
        }];
        assert!(
            normal.contains(&'N'),
            "normal variant should have 'N' from 'Normal '"
        );
    }

    #[test]
    fn test_parse_css_custom_properties() {
        let css = r#"
            :root {
                --font-mono: 'Iosevka', monospace;
                --font-body: "Inter", sans-serif;
                --spacing: 1rem;
            }
            body { color: black; }
        "#;

        let vars = parse_css_custom_properties(css);
        assert_eq!(
            vars.get("--font-mono"),
            Some(&"'Iosevka', monospace".to_string())
        );
        assert_eq!(
            vars.get("--font-body"),
            Some(&"\"Inter\", sans-serif".to_string())
        );
        assert_eq!(vars.get("--spacing"), Some(&"1rem".to_string()));
    }

    #[test]
    fn test_resolve_css_var_simple() {
        let mut vars = HashMap::new();
        vars.insert(
            "--font-mono".to_string(),
            "'Iosevka', monospace".to_string(),
        );

        let result = resolve_css_var("var(--font-mono)", &vars);
        assert_eq!(result, "'Iosevka', monospace");
    }

    #[test]
    fn test_resolve_css_var_with_fallback() {
        let vars: CssVariables = HashMap::new();

        // When variable doesn't exist, should use fallback
        let result = resolve_css_var("var(--undefined, Arial)", &vars);
        assert_eq!(result, "Arial");
    }

    #[test]
    fn test_resolve_css_var_nested() {
        let mut vars = HashMap::new();
        vars.insert("--base-font".to_string(), "'Inter'".to_string());
        vars.insert(
            "--font-stack".to_string(),
            "var(--base-font), sans-serif".to_string(),
        );

        let result = resolve_css_var("var(--font-stack)", &vars);
        assert_eq!(result, "'Inter', sans-serif");
    }

    #[test]
    fn test_font_family_with_css_var() {
        // This is the exact reproduction case from the issue
        let html = r#"
            <html>
            <head>
                <style>
                    @font-face {
                        font-family: 'Iosevka';
                        src: url('/fonts/Iosevka-Regular.woff2') format('woff2');
                    }

                    :root {
                        --font-mono: 'Iosevka', monospace;
                    }

                    code {
                        font-family: var(--font-mono);
                    }
                </style>
            </head>
            <body>
                <code>fn main() { println!("hello"); }</code>
            </body>
            </html>
        "#;

        let css = extract_css_from_html(html);
        let analysis = analyze_fonts(html, &css);

        // Should have the font-face for Iosevka
        assert_eq!(analysis.font_faces.len(), 1);
        assert_eq!(analysis.font_faces[0].family, "Iosevka");

        // Should have collected chars for Iosevka (not None/empty!)
        assert!(
            analysis.chars_per_font.contains_key("Iosevka"),
            "chars_per_font should contain Iosevka, but got: {:?}",
            analysis.chars_per_font.keys().collect::<Vec<_>>()
        );

        let iosevka_chars = &analysis.chars_per_font["Iosevka"];
        // Check for characters from: fn main() { println!("hello"); }
        assert!(iosevka_chars.contains(&'f'));
        assert!(iosevka_chars.contains(&'n'));
        assert!(iosevka_chars.contains(&'m'));
        assert!(iosevka_chars.contains(&'('));
        assert!(iosevka_chars.contains(&'{'));
        assert!(iosevka_chars.contains(&'h'));
        assert!(iosevka_chars.contains(&'e'));
        assert!(iosevka_chars.contains(&'l'));
        assert!(iosevka_chars.contains(&'o'));
    }

    #[test]
    fn test_css_var_in_multiple_rules() {
        let css = r#"
            :root {
                --heading-font: 'Playfair Display';
                --body-font: 'Inter';
            }

            h1 { font-family: var(--heading-font); }
            h2 { font-family: var(--heading-font); }
            p { font-family: var(--body-font); }
        "#;

        let vars = parse_css_custom_properties(css);
        let rules = parse_font_style_rules_with_vars(css, &vars);

        // Should have 3 rules (h1, h2, p)
        assert_eq!(rules.len(), 3);
        assert_eq!(rules[0].font_family.as_deref(), Some("Playfair Display"));
        assert_eq!(rules[1].font_family.as_deref(), Some("Playfair Display"));
        assert_eq!(rules[2].font_family.as_deref(), Some("Inter"));
    }
}

#[cfg(test)]
mod unicode_tests {
    use super::*;

    #[test]
    fn test_css_var_with_unicode_variable_name() {
        let css = r#"
            :root {
                --日本語: 'Noto Sans JP';
            }
            body {
                font-family: var(--日本語);
            }
        "#;

        let vars = parse_css_custom_properties(css);
        assert_eq!(vars.get("--日本語"), Some(&"'Noto Sans JP'".to_string()));

        let rules = parse_font_style_rules_with_vars(css, &vars);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].font_family.as_deref(), Some("Noto Sans JP"));
    }

    #[test]
    fn test_css_var_with_unicode_in_value() {
        let css = r#"
            :root {
                --font: '日本語フォント', sans-serif;
            }
            body {
                font-family: var(--font);
            }
        "#;

        let vars = parse_css_custom_properties(css);
        let rules = parse_font_style_rules_with_vars(css, &vars);
        assert_eq!(rules[0].font_family.as_deref(), Some("日本語フォント"));
    }

    #[test]
    fn test_css_var_unicode_before_var() {
        // Unicode content before var() in the same property value
        let css = r#"
            :root {
                --font: 'Test';
            }
            body {
                font-family: var(--font);
            }
        "#;

        let vars = parse_css_custom_properties(css);
        let rules = parse_font_style_rules_with_vars(css, &vars);
        assert_eq!(rules[0].font_family.as_deref(), Some("Test"));
    }

    #[test]
    fn test_css_var_with_emoji() {
        let css = r#"
            :root {
                --emoji-font: '🎉 Party Font';
            }
            body {
                font-family: var(--emoji-font);
            }
        "#;

        let vars = parse_css_custom_properties(css);
        let rules = parse_font_style_rules_with_vars(css, &vars);
        assert_eq!(rules[0].font_family.as_deref(), Some("🎉 Party Font"));
    }

    #[test]
    fn test_css_var_fallback_with_unicode() {
        let css = r#"
            body {
                font-family: var(--undefined, '日本語フォント');
            }
        "#;

        let vars = parse_css_custom_properties(css);
        let rules = parse_font_style_rules_with_vars(css, &vars);
        assert_eq!(rules[0].font_family.as_deref(), Some("日本語フォント"));
    }

    #[test]
    fn test_css_var_nested_with_unicode() {
        let css = r#"
            :root {
                --base: '日本語';
                --full: var(--base), sans-serif;
            }
            body {
                font-family: var(--full);
            }
        "#;

        let vars = parse_css_custom_properties(css);
        let rules = parse_font_style_rules_with_vars(css, &vars);
        assert_eq!(rules[0].font_family.as_deref(), Some("日本語"));
    }

    #[test]
    fn test_unicode_selector() {
        let css = r#"
            .日本語-class {
                font-family: 'Test Font';
            }
        "#;

        let rules = parse_font_style_rules_with_vars(css, &HashMap::new());
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].selector, ".日本語-class");
        assert_eq!(rules[0].font_family.as_deref(), Some("Test Font"));
    }

    #[test]
    fn test_unicode_in_font_face() {
        let css = r#"
            @font-face {
                font-family: '日本語フォント';
                src: url('/fonts/japanese.woff2');
            }
        "#;

        let faces = parse_font_face_rules(css);
        assert_eq!(faces.len(), 1);
        assert_eq!(faces[0].family, "日本語フォント");
    }

    #[test]
    fn test_mixed_unicode_and_ascii_complex() {
        let css = r#"
            :root {
                --primary: 'Helvetica';
                --日本語: 'Noto Sans JP';
                --combined: var(--primary), var(--日本語), sans-serif;
            }
            .my-class {
                font-family: var(--combined);
            }
        "#;

        let vars = parse_css_custom_properties(css);
        assert_eq!(vars.get("--primary"), Some(&"'Helvetica'".to_string()));
        assert_eq!(vars.get("--日本語"), Some(&"'Noto Sans JP'".to_string()));

        let rules = parse_font_style_rules_with_vars(css, &vars);
        assert_eq!(rules[0].font_family.as_deref(), Some("Helvetica"));
    }

    #[test]
    fn test_collect_chars_with_unicode_content() {
        let html = r#"
            <html>
            <head>
                <style>
                    :root { --font: 'TestFont'; }
                    body { font-family: var(--font); }
                </style>
            </head>
            <body>
                <p>日本語テキスト</p>
            </body>
            </html>
        "#;

        let css = extract_css_from_html(html);
        let (chars, _) = collect_chars_per_font(html, &css);

        assert!(chars.contains_key("TestFont"));
        let font_chars = &chars["TestFont"];
        assert!(font_chars.contains(&'日'));
        assert!(font_chars.contains(&'本'));
        assert!(font_chars.contains(&'語'));
    }

    #[test]
    fn test_analyze_fonts_unicode_everywhere() {
        let html = r#"
            <html>
            <head>
                <style>
                    @font-face {
                        font-family: '日本語フォント';
                        src: url('/fonts/jp.woff2');
                    }
                    :root {
                        --jp-font: '日本語フォント', sans-serif;
                    }
                    body {
                        font-family: var(--jp-font);
                    }
                </style>
            </head>
            <body>
                <p>こんにちは世界</p>
            </body>
            </html>
        "#;

        let css = extract_css_from_html(html);
        let analysis = analyze_fonts(html, &css);

        // Font face should be parsed
        assert_eq!(analysis.font_faces.len(), 1);
        assert_eq!(analysis.font_faces[0].family, "日本語フォント");

        // Characters should be collected
        assert!(analysis.chars_per_font.contains_key("日本語フォント"));
        let chars = &analysis.chars_per_font["日本語フォント"];
        assert!(chars.contains(&'こ'));
        assert!(chars.contains(&'世'));
        assert!(chars.contains(&'界'));
    }

    #[test]
    fn test_var_immediately_after_unicode() {
        // Edge case: var() immediately after multi-byte chars
        let value = "日本語var(--test)";
        let mut vars = HashMap::new();
        vars.insert("--test".to_string(), "'Result'".to_string());

        let resolved = resolve_css_var(value, &vars);
        assert_eq!(resolved, "日本語'Result'");
    }

    #[test]
    fn test_var_between_unicode() {
        let value = "前var(--mid)後";
        let mut vars = HashMap::new();
        vars.insert("--mid".to_string(), "中".to_string());

        let resolved = resolve_css_var(value, &vars);
        assert_eq!(resolved, "前中後");
    }

    #[test]
    fn test_multiple_vars_with_unicode() {
        let value = "var(--a)日本語var(--b)";
        let mut vars = HashMap::new();
        vars.insert("--a".to_string(), "前".to_string());
        vars.insert("--b".to_string(), "後".to_string());

        let resolved = resolve_css_var(value, &vars);
        assert_eq!(resolved, "前日本語後");
    }

    #[test]
    fn test_four_byte_unicode() {
        // Test with 4-byte UTF-8 characters (emoji, etc.)
        let css = r#"
            :root {
                --emoji: '😀🎉🚀';
            }
            body {
                font-family: var(--emoji);
            }
        "#;

        let vars = parse_css_custom_properties(css);
        assert_eq!(vars.get("--emoji"), Some(&"'😀🎉🚀'".to_string()));

        let rules = parse_font_style_rules_with_vars(css, &vars);
        assert_eq!(rules[0].font_family.as_deref(), Some("😀🎉🚀"));
    }

    #[test]
    fn test_zalgo_text() {
        // Test with combining characters (Zalgo-style text)
        let css = r#"
            :root {
                --zalgo: 'H̷e̶l̵l̴o̷';
            }
            body {
                font-family: var(--zalgo);
            }
        "#;

        let vars = parse_css_custom_properties(css);
        let rules = parse_font_style_rules_with_vars(css, &vars);
        assert_eq!(rules[0].font_family.as_deref(), Some("H̷e̶l̵l̴o̷"));
    }
}
