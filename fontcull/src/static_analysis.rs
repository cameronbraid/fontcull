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
/// defaults, then author declarations ordered by `!important`, specificity and
/// source order, with inheritance and the CSS-wide keywords. Characters from
/// `::before`/`::after` `content` strings are included. Rules that apply only
/// under some condition (inside `@media`, `@supports` or `@container`, or with
/// `:hover`/`:focus`-style state pseudo-classes) are analysed both ways and
/// their characters counted in both variants, so text that changes font with
/// the viewport or interaction stays covered.
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
    let font_rules = parse_font_style_rules_with_vars(css, &css_vars);
    let has_conditional = font_rules.iter().any(|rule| rule.conditional);

    let mut per_family: HashMap<String, HashSet<char>> = HashMap::new();
    let mut per_variant: HashMap<FontVariantKey, HashSet<char>> = HashMap::new();

    let passes: &[bool] = if has_conditional {
        &[false, true]
    } else {
        &[false]
    };
    for &include_conditional in passes {
        let rules: Vec<&FontStyleRule> = font_rules
            .iter()
            .filter(|rule| include_conditional || !rule.conditional)
            .collect();
        collect_pass(&document, &rules, &mut per_family, &mut per_variant);
    }

    (per_family, per_variant)
}

fn collect_pass(
    document: &Html,
    rules: &[&FontStyleRule],
    per_family: &mut HashMap<String, HashSet<char>>,
    per_variant: &mut HashMap<FontVariantKey, HashSet<char>>,
) {
    let (element_rules, pseudo_rules): (Vec<&FontStyleRule>, Vec<&FontStyleRule>) = rules
        .iter()
        .copied()
        .partition(|rule| rule.pseudo.is_none());

    let mut add = |style: &ResolvedFontStyle, text: &str| {
        let family_chars = per_family.entry(style.font_family.clone()).or_default();
        family_chars.extend(text.chars());
        let key = FontVariantKey {
            family: style.font_family.clone(),
            weight: style.font_weight,
            style: style.font_style.clone(),
        };
        per_variant.entry(key).or_default().extend(text.chars());
    };

    // Elements are visited in document order, so a parent is always resolved
    // before its children.
    let mut resolved: HashMap<_, ResolvedFontStyle> = HashMap::new();
    let all_elements = Selector::parse("*").unwrap();
    for element in document.select(&all_elements) {
        let parent = element
            .ancestors()
            .find_map(scraper::ElementRef::wrap)
            .and_then(|parent| resolved.get(&parent.id()))
            .cloned()
            .unwrap_or_default();
        let matching = element_rules
            .iter()
            .copied()
            .filter(|rule| rule.selector_matches(&element));
        let style = cascade(&parent, Some(element.value().name()), matching);

        // Get all direct text nodes (not from children)
        let text: String = element
            .children()
            .filter_map(|child| child.value().as_text().map(|t| t.to_string()))
            .collect();
        if !text.trim().is_empty() {
            add(&style, &text);
        }

        if element.value().name() == "q" {
            // The user-agent stylesheet quotes `<q>` with open-quote/close-quote.
            add(&style, DEFAULT_QUOTES);
        }

        for pseudo in [PseudoElement::Before, PseudoElement::After] {
            let matching: Vec<&FontStyleRule> = pseudo_rules
                .iter()
                .copied()
                .filter(|rule| rule.pseudo == Some(pseudo) && rule.selector_matches(&element))
                .collect();
            // Conservatively union every matching content value instead of
            // resolving which one wins.
            let content: String = matching
                .iter()
                .filter_map(|rule| rule.content.as_deref())
                .collect();
            if !content.trim().is_empty() {
                let pseudo_style = cascade(&style, None, matching.iter().copied());
                add(&pseudo_style, &content);
            }
        }

        resolved.insert(element.id(), style);
    }
}

/// Characters generated by `open-quote` and `close-quote` with the default `quotes`.
const DEFAULT_QUOTES: &str = "\u{201C}\u{201D}\u{2018}\u{2019}";

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
}

/// A parsed `font-weight` value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FontWeightValue {
    Absolute(u16),
    Bolder,
    Lighter,
    Inherit,
    Initial,
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
    /// Characters of the `content` strings, for `::before`/`::after` rules.
    content: Option<String>,
    pseudo: Option<PseudoElement>,
    /// Inside `@media`/`@supports`/`@container`, or matching only in an interaction state.
    conditional: bool,
    specificity: (u32, u32, u32),
    order: usize,
    /// The selector parsed for matching (state pseudo-classes removed).
    parsed: Option<Selector>,
}

impl FontStyleRule {
    fn selector_matches(&self, element: &scraper::ElementRef) -> bool {
        self.parsed
            .as_ref()
            .is_some_and(|selector| selector.matches(element))
    }
}

/// Resolve one element's (or pseudo-element's) font from its parent's computed style,
/// its user-agent defaults and the author rules that match it.
fn cascade<'a>(
    parent: &ResolvedFontStyle,
    tag: Option<&str>,
    rules: impl Iterator<Item = &'a FontStyleRule>,
) -> ResolvedFontStyle {
    // font-family, font-weight and font-style are inherited properties.
    let mut style = parent.clone();
    if let Some(tag) = tag {
        match tag {
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "th" => style.font_weight = 700,
            "b" | "strong" => style.font_weight = bolder(parent.font_weight),
            "i" | "em" | "cite" | "dfn" | "var" | "address" => {
                style.font_style = "italic".to_string()
            }
            "button" | "input" | "select" | "textarea" => {
                // Form controls don't inherit the page font unless an author rule says so.
                style.font_family = "system-ui".to_string();
                style.font_weight = 400;
                style.font_style = "normal".to_string();
            }
            _ => {}
        }
    }

    type Priority = (bool, (u32, u32, u32), usize);
    let mut family: Option<(Priority, &str)> = None;
    let mut weight: Option<(Priority, FontWeightValue)> = None;
    let mut font_style: Option<(Priority, &str)> = None;
    for rule in rules {
        let priority = |important: bool| (important, rule.specificity, rule.order);
        if let Some(value) = rule.font_family.as_deref() {
            let p = priority(rule.family_important);
            if family.is_none_or(|(best, _)| p > best) {
                family = Some((p, value));
            }
        }
        if let Some(value) = rule.font_weight {
            let p = priority(rule.weight_important);
            if weight.is_none_or(|(best, _)| p > best) {
                weight = Some((p, value));
            }
        }
        if let Some(value) = rule.font_style.as_deref() {
            let p = priority(rule.style_important);
            if font_style.is_none_or(|(best, _)| p > best) {
                font_style = Some((p, value));
            }
        }
    }

    if let Some((_, value)) = family {
        match css_wide_keyword(value) {
            Some(CssWideKeyword::Inherit) => style.font_family = parent.font_family.clone(),
            Some(CssWideKeyword::Initial) => style.font_family = "serif".to_string(),
            None => style.font_family = value.to_string(),
        }
    }
    if let Some((_, value)) = weight {
        style.font_weight = match value {
            FontWeightValue::Absolute(weight) => weight,
            FontWeightValue::Bolder => bolder(parent.font_weight),
            FontWeightValue::Lighter => lighter(parent.font_weight),
            FontWeightValue::Inherit => parent.font_weight,
            FontWeightValue::Initial => 400,
        };
    }
    if let Some((_, value)) = font_style {
        match css_wide_keyword(value) {
            Some(CssWideKeyword::Inherit) => style.font_style = parent.font_style.clone(),
            Some(CssWideKeyword::Initial) => style.font_style = "normal".to_string(),
            None => style.font_style = value.to_string(),
        }
    }
    style
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
    /// `inherit`, and `unset`/`revert`/`revert-layer`, which inherit for these inherited
    /// properties.
    Inherit,
    Initial,
}

fn css_wide_keyword(value: &str) -> Option<CssWideKeyword> {
    match value.trim().to_ascii_lowercase().as_str() {
        "inherit" | "unset" | "revert" | "revert-layer" => Some(CssWideKeyword::Inherit),
        "initial" => Some(CssWideKeyword::Initial),
        _ => None,
    }
}

/// Parse a CSS font-weight value.
fn parse_font_weight(value: &str) -> Option<FontWeightValue> {
    let value = value.trim().to_ascii_lowercase();
    match css_wide_keyword(&value) {
        Some(CssWideKeyword::Inherit) => return Some(FontWeightValue::Inherit),
        Some(CssWideKeyword::Initial) => return Some(FontWeightValue::Initial),
        None => {}
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

/// The font properties set by a `font` shorthand: (family, weight, style).
fn parse_font_shorthand(
    value: &str,
    css_vars: &CssVariables,
) -> Option<(String, FontWeightValue, String)> {
    let value = resolve_css_var(value.trim(), css_vars);
    if let Some(keyword) = css_wide_keyword(&value) {
        let (weight, style) = match keyword {
            CssWideKeyword::Inherit => (FontWeightValue::Inherit, "inherit"),
            CssWideKeyword::Initial => (FontWeightValue::Initial, "initial"),
        };
        return Some((value.trim().to_string(), weight, style.to_string()));
    }
    // [style || variant || weight || stretch]? size [/ line-height]? family
    let mut weight = FontWeightValue::Absolute(400);
    let mut style = "normal".to_string();
    let mut rest = value.as_str();
    loop {
        rest = rest.trim_start();
        let token_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let token = &rest[..token_end];
        if token.is_empty() {
            return None; // no size or family: a system font keyword or invalid
        }
        let lower = token.to_ascii_lowercase();
        if is_font_size(&lower) {
            let mut after = rest[token_end..].trim_start();
            if let Some(line_height) = after.strip_prefix('/') {
                let line_height = line_height.trim_start();
                let end = line_height
                    .find(char::is_whitespace)
                    .unwrap_or(line_height.len());
                after = &line_height[end..];
            }
            let family = parse_font_family_value_with_vars(after, css_vars);
            if family.is_empty() {
                return None;
            }
            return Some((family, weight, style));
        }
        if let Some(parsed) = parse_font_style(&lower) {
            style = parsed;
        } else if let Some(parsed) = parse_font_weight(&lower) {
            weight = parsed;
        }
        // font-variant and font-stretch keywords are ignored
        rest = &rest[token_end..];
    }
}

fn is_font_size(token: &str) -> bool {
    let size = token.split('/').next().unwrap_or(token);
    matches!(
        size,
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
    ) || size.starts_with(|c: char| c.is_ascii_digit() || c == '.')
        && !size.chars().all(|c| c.is_ascii_digit())
        || size.starts_with("calc(")
        || size.starts_with("clamp(")
        || size.starts_with("min(")
        || size.starts_with("max(")
}

/// Parse a `content` value into the characters it generates. Quoted strings (with CSS
/// escapes) and quote keywords produce text; `attr()`, counters and images are ignored.
fn parse_content(value: &str) -> Option<String> {
    let mut text = String::new();
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' | '\'' => {
                let quote = c;
                while let Some(c) = chars.next() {
                    if c == quote {
                        break;
                    }
                    if c == '\\' {
                        let mut hex = String::new();
                        while hex.len() < 6 && chars.peek().is_some_and(|c| c.is_ascii_hexdigit()) {
                            hex.push(chars.next().unwrap_or_default());
                        }
                        if hex.is_empty() {
                            if let Some(escaped) = chars.next()
                                && escaped != '\n'
                            {
                                text.push(escaped);
                            }
                        } else {
                            if chars.peek().is_some_and(|c| c.is_whitespace()) {
                                chars.next();
                            }
                            if let Some(decoded) =
                                u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32)
                            {
                                text.push(decoded);
                            }
                        }
                    } else {
                        text.push(c);
                    }
                }
            }
            _ if c.is_alphabetic() => {
                let mut word = String::from(c);
                while chars
                    .peek()
                    .is_some_and(|c| c.is_alphanumeric() || *c == '-')
                {
                    word.push(chars.next().unwrap_or_default());
                }
                if matches!(word.as_str(), "open-quote" | "close-quote") {
                    text.push_str(DEFAULT_QUOTES);
                }
            }
            _ => {}
        }
    }
    (!text.is_empty()).then_some(text)
}

/// Parse CSS and extract rules that set font-family, font-weight, font-style or the
/// `content` of `::before`/`::after`, with CSS variable resolution.
///
/// Selector lists become one rule per selector. Rules inside `@media`, `@supports` and
/// `@container` are marked conditional; other at-rules (such as `@font-face` and
/// `@keyframes`) are skipped.
fn parse_font_style_rules_with_vars(css: &str, css_vars: &CssVariables) -> Vec<FontStyleRule> {
    let mut rules = Vec::new();
    let css = strip_css_comments(css);
    parse_rule_list(&css, css_vars, false, &mut rules);
    rules
}

fn strip_css_comments(css: &str) -> String {
    let mut out = String::with_capacity(css.len());
    let mut rest = css;
    while let Some(start) = rest.find("/*") {
        out.push_str(&rest[..start]);
        rest = match rest[start + 2..].find("*/") {
            Some(end) => &rest[start + 2 + end + 2..],
            None => "",
        };
    }
    out.push_str(rest);
    out
}

/// Byte index of the first `target` at nesting depth zero outside strings.
fn find_top_level(s: &str, targets: &[char]) -> Option<usize> {
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (i, c) in s.char_indices() {
        if let Some(q) = quote {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '"' | '\'' => quote = Some(c),
            '\\' => escaped = true,
            _ if depth == 0 && targets.contains(&c) => return Some(i),
            '(' | '[' => depth += 1,
            ')' | ']' => depth -= 1,
            _ => {}
        }
    }
    None
}

/// Byte index of the `}` that closes a block whose `{` precedes `s`.
fn find_block_end(s: &str) -> Option<usize> {
    let mut depth = 1i32;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (i, c) in s.char_indices() {
        if let Some(q) = quote {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '"' | '\'' => quote = Some(c),
            '\\' => escaped = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
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

fn parse_rule_list(
    css: &str,
    css_vars: &CssVariables,
    conditional: bool,
    rules: &mut Vec<FontStyleRule>,
) {
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
            // a statement at-rule (`@import …;`), or a stray `;`/`}`
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
            let name = at_rule
                .split(|c: char| c.is_whitespace() || c == '(')
                .next()
                .unwrap_or("")
                .to_ascii_lowercase();
            match name.as_str() {
                "media" | "supports" | "container" | "document" | "-moz-document" => {
                    parse_rule_list(block, css_vars, true, rules)
                }
                "layer" | "scope" => parse_rule_list(block, css_vars, conditional, rules),
                _ => {} // @font-face, @keyframes, @page, @property, …
            }
            continue;
        }
        push_style_rules(prelude, block, css_vars, conditional, rules);
    }
}

fn push_style_rules(
    prelude: &str,
    block: &str,
    css_vars: &CssVariables,
    conditional: bool,
    rules: &mut Vec<FontStyleRule>,
) {
    let mut font_family = None;
    let mut font_weight = None;
    let mut font_style = None;
    let mut family_important = false;
    let mut weight_important = false;
    let mut style_important = false;
    let mut content = None;

    // (nested rules in the block are ignored: their declarations come after them)
    let declarations = match find_top_level(block, &['{']) {
        Some(nested) => &block[..block[..nested].rfind(';').map_or(0, |i| i + 1)],
        None => block,
    };
    for declaration in split_top_level(declarations, ';') {
        let Some(colon) = declaration.find(':') else {
            continue;
        };
        let name = declaration[..colon].trim().to_ascii_lowercase();
        let mut value = declaration[colon + 1..].trim();
        let mut important = false;
        if let Some(i) = value.to_ascii_lowercase().rfind("!important")
            && value[i + "!important".len()..].trim().is_empty()
        {
            value = value[..i].trim_end();
            important = true;
        }
        match name.as_str() {
            "font-family" => {
                font_family = Some(parse_font_family_value_with_vars(value, css_vars));
                family_important = important;
            }
            "font-weight" => {
                if let Some(weight) = parse_font_weight(&resolve_css_var(value, css_vars)) {
                    font_weight = Some(weight);
                    weight_important = important;
                }
            }
            "font-style" => {
                if let Some(style) = parse_font_style(&resolve_css_var(value, css_vars)) {
                    font_style = Some(style);
                    style_important = important;
                }
            }
            "font" => {
                if let Some((family, weight, style)) = parse_font_shorthand(value, css_vars) {
                    font_family = Some(family);
                    font_weight = Some(weight);
                    font_style = Some(style);
                    family_important = important;
                    weight_important = important;
                    style_important = important;
                }
            }
            "content" => content = parse_content(&resolve_css_var(value, css_vars)),
            _ => {}
        }
    }

    let sets_font = font_family.is_some() || font_weight.is_some() || font_style.is_some();
    if !sets_font && content.is_none() {
        return;
    }

    for selector in split_top_level(prelude, ',') {
        let selector = selector.trim();
        if selector.is_empty() || selector.starts_with('@') {
            continue;
        }
        let (base, pseudo) = split_pseudo_element(selector);
        let Some(base) = base else {
            continue; // another pseudo-element, such as ::placeholder or ::marker
        };
        let base = base.as_str();
        if pseudo.is_none() && !sets_font {
            continue; // `content` only applies to pseudo-elements
        }
        let (matchable, state_dependent) = strip_state_pseudo_classes(base);
        let order = rules.len();
        rules.push(FontStyleRule {
            selector: base.to_string(),
            font_family: font_family.clone(),
            font_weight,
            font_style: font_style.clone(),
            family_important,
            weight_important,
            style_important,
            content: if pseudo.is_some() {
                content.clone()
            } else {
                None
            },
            pseudo,
            conditional: conditional || state_dependent,
            specificity: specificity(base),
            order,
            parsed: Selector::parse(&matchable).ok(),
        });
    }
}

/// Split a trailing `::before`/`::after` (or the legacy single-colon form) off a
/// selector. The base is `None` when the selector targets another pseudo-element.
fn split_pseudo_element(selector: &str) -> (Option<String>, Option<PseudoElement>) {
    let lower = selector.to_ascii_lowercase();
    for (suffix, pseudo) in [
        ("::before", PseudoElement::Before),
        ("::after", PseudoElement::After),
        (":before", PseudoElement::Before),
        (":after", PseudoElement::After),
    ] {
        if lower.ends_with(suffix) {
            let base = &selector[..selector.len() - suffix.len()];
            let base = if base.trim().is_empty() {
                "*".to_string()
            } else if base.ends_with(|c: char| c.is_whitespace() || matches!(c, '>' | '+' | '~')) {
                // `ul ::before` and `ul > ::before` apply to any matching descendant
                format!("{base}*")
            } else {
                base.to_string()
            };
            return (Some(base), Some(pseudo));
        }
    }
    if lower.contains("::") || lower.ends_with(":first-line") || lower.ends_with(":first-letter") {
        return (None, None);
    }
    (Some(selector.to_string()), None)
}

/// Pseudo-classes for interaction states the static document never has.
const STATE_PSEUDO_CLASSES: [&str; 7] = [
    ":hover",
    ":focus-visible",
    ":focus-within",
    ":focus",
    ":active",
    ":visited",
    ":target",
];

/// Remove interaction-state pseudo-classes so the rule can be matched as if the state
/// applied. Returns the selector to match and whether anything was removed.
fn strip_state_pseudo_classes(selector: &str) -> (String, bool) {
    let mut out = String::with_capacity(selector.len());
    let mut removed = false;
    let mut rest = selector;
    'outer: while !rest.is_empty() {
        if rest.starts_with(':') && !rest.starts_with("::") {
            let lower = rest.to_ascii_lowercase();
            for state in STATE_PSEUDO_CLASSES {
                let ends_identifier = lower[state.len().min(lower.len())..]
                    .chars()
                    .next()
                    .is_none_or(|c| !(c.is_alphanumeric() || c == '-' || c == '_' || c == '('));
                if lower.starts_with(state) && ends_identifier {
                    if out.is_empty() || out.ends_with([' ', '>', '+', '~', '(', ',']) {
                        out.push('*');
                    }
                    rest = &rest[state.len()..];
                    removed = true;
                    continue 'outer;
                }
            }
        }
        let c = rest.chars().next().unwrap_or_default();
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    (out, removed)
}

/// Approximate CSS selector specificity as (ids, classes/attributes/pseudo-classes, types).
fn specificity(selector: &str) -> (u32, u32, u32) {
    let mut ids = 0;
    let mut classes = 0;
    let mut types = 0;
    let chars: Vec<char> = selector.chars().collect();
    let mut i = 0;
    let identifier =
        |c: char| c.is_alphanumeric() || c == '-' || c == '_' || c == '\\' || !c.is_ascii();
    while i < chars.len() {
        let c = chars[i];
        match c {
            '#' => {
                ids += 1;
                i += 1;
                while i < chars.len() && identifier(chars[i]) {
                    i += 1;
                }
            }
            '.' => {
                classes += 1;
                i += 1;
                while i < chars.len() && identifier(chars[i]) {
                    i += 1;
                }
            }
            '[' => {
                classes += 1;
                let rest: String = chars[i..].iter().collect();
                i += find_top_level(&rest[1..], &[']'])
                    .map_or(rest.len(), |end| rest[..1 + end + 1].chars().count());
            }
            ':' => {
                let element = chars.get(i + 1) == Some(&':');
                i += if element { 2 } else { 1 };
                let start = i;
                while i < chars.len() && identifier(chars[i]) {
                    i += 1;
                }
                let name: String = chars[start..i]
                    .iter()
                    .collect::<String>()
                    .to_ascii_lowercase();
                let mut argument = None;
                if chars.get(i) == Some(&'(') {
                    let rest: String = chars[i + 1..].iter().collect();
                    let end = find_top_level(&rest, &[')']).unwrap_or(rest.len());
                    argument = Some(rest[..end].to_string());
                    i += 1 + rest[..end].chars().count() + 1;
                }
                if element {
                    types += 1;
                } else {
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
            }
            '*' => i += 1,
            _ if identifier(c) => {
                types += 1;
                while i < chars.len() && identifier(chars[i]) {
                    i += 1;
                }
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

    // Parse through CSS looking for custom property declarations
    let mut remaining = css;

    while let Some(brace_start) = remaining.find('{') {
        let after_brace = &remaining[brace_start + 1..];

        // Find matching closing brace
        let mut depth = 1;
        let mut block_end = None;
        for (i, c) in after_brace.char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        block_end = Some(i);
                        break;
                    }
                }
                _ => {}
            }
        }

        let Some(end) = block_end else {
            break;
        };

        let block = &after_brace[..end];

        // Parse declarations in this block
        for declaration in block.split(';') {
            let declaration = declaration.trim();

            // Look for custom property declarations (--name: value)
            if declaration.starts_with("--")
                && let Some(colon_pos) = declaration.find(':')
            {
                let name = declaration[..colon_pos].trim();
                let value = declaration[colon_pos + 1..].trim();
                vars.insert(name.to_string(), value.to_string());
            }
        }

        remaining = &after_brace[end + 1..];
    }

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
        assert_eq!(variant_chars(html, css, "F", 700, "normal"), " ‹");
        assert_eq!(variant_chars(html, css, "F", 400, "normal"), ">xyz‘’“”");
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
