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

    let mut per_family: HashMap<String, HashSet<char>> = HashMap::new();
    let mut per_variant: HashMap<FontVariantKey, HashSet<char>> = HashMap::new();
    let mut add = |styles: &[ResolvedFontStyle], text: &str| {
        for style in styles {
            let family_chars = per_family.entry(style.font_family.clone()).or_default();
            family_chars.extend(text.chars());
            let key = FontVariantKey {
                family: style.font_family.clone(),
                weight: style.font_weight,
                style: style.font_style.clone(),
            };
            per_variant.entry(key).or_default().extend(text.chars());
        }
    };

    // Elements are visited in document order, so a parent is always resolved
    // before its children.
    let root = vec![ResolvedFontStyle::default()];
    let mut resolved: HashMap<_, Vec<ResolvedFontStyle>> = HashMap::new();
    let all_elements = Selector::parse("*").unwrap();
    for element in document.select(&all_elements) {
        let parents = element
            .ancestors()
            .find_map(scraper::ElementRef::wrap)
            .and_then(|parent| resolved.get(&parent.id()))
            .unwrap_or(&root);
        let matching: Vec<&FontStyleRule> = element_rules
            .iter()
            .copied()
            .filter(|rule| rule.selector_matches(&element))
            .collect();
        let tag = element.value().name();
        let styles = cascade(parents, Some(tag), &matching);

        // Get all direct text nodes (not from children)
        let text: String = element
            .children()
            .filter_map(|child| child.value().as_text().map(|t| t.to_string()))
            .collect();
        if !text.trim().is_empty() {
            add(&styles, &text);
        }

        for pseudo in [PseudoElement::Before, PseudoElement::After] {
            let matching: Vec<&FontStyleRule> = pseudo_rules
                .iter()
                .copied()
                .filter(|rule| rule.pseudo == Some(pseudo) && rule.selector_matches(&element))
                .collect();
            // Conservatively union every matching content value instead of
            // resolving which one wins.
            let mut content: String = matching
                .iter()
                .filter_map(|rule| rule.content.as_ref())
                .map(|content| {
                    if content.quotes {
                        format!("{}{quote_chars}", content.text)
                    } else {
                        content.text.clone()
                    }
                })
                .collect();
            if tag == "q" {
                // The user-agent stylesheet quotes `<q>` with open-quote/close-quote.
                content.push_str(&quote_chars);
            }
            if !content.trim().is_empty() {
                add(&cascade(&styles, None, &matching), &content);
            }
        }

        resolved.insert(element.id(), styles);
    }

    (per_family, per_variant)
}

/// Characters generated by `open-quote` and `close-quote` with the default `quotes`.
const DEFAULT_QUOTES: &str = "\u{201C}\u{201D}\u{2018}\u{2019}";

/// At most this many possible fonts are tracked per element; conditional rules
/// rarely produce more than a handful.
const MAX_POSSIBLE_STYLES: usize = 64;

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
    Keyword(CssWideKeyword),
}

/// Generated text of a `content` value.
#[derive(Debug, Clone, PartialEq, Eq)]
struct GeneratedContent {
    text: String,
    /// Uses `open-quote`/`close-quote`.
    quotes: bool,
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
    /// Generated text, for `::before`/`::after` rules.
    content: Option<GeneratedContent>,
    pseudo: Option<PseudoElement>,
    /// Inside `@media`/`@supports`/`@container`/`@scope`, or matching only in an
    /// interaction state: the rule may or may not apply.
    conditional: bool,
    /// Inside `@layer`: unlayered normal declarations beat it, layered `!important` ones
    /// beat unlayered `!important` ones.
    layered: bool,
    specificity: (u32, u32, u32),
    order: usize,
    /// The selector parsed for matching (top-level state pseudo-classes removed).
    parsed: Option<Selector>,
}

impl FontStyleRule {
    fn selector_matches(&self, element: &scraper::ElementRef) -> bool {
        self.parsed
            .as_ref()
            .is_some_and(|selector| selector.matches(element))
    }

    /// Cascade order of one of this rule's declarations: importance, layer, specificity,
    /// source order.
    fn priority(&self, important: bool) -> Priority {
        // Layers reverse for important declarations.
        let layer = if important {
            self.layered
        } else {
            !self.layered
        };
        (important, layer, self.specificity, self.order)
    }
}

type Priority = (bool, bool, (u32, u32, u32), usize);

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
fn possible_winners<'a, T: Copy>(
    rules: &[&'a FontStyleRule],
    value: impl Fn(&'a FontStyleRule) -> Option<(T, bool)>,
) -> Vec<Option<T>> {
    let mut base: Option<(Priority, T)> = None;
    for rule in rules.iter().filter(|rule| !rule.conditional) {
        if let Some((v, important)) = value(rule) {
            let p = rule.priority(important);
            if base.is_none_or(|(best, _)| p > best) {
                base = Some((p, v));
            }
        }
    }
    let mut winners = vec![base.map(|(_, v)| v)];
    for rule in rules.iter().filter(|rule| rule.conditional) {
        if let Some((v, important)) = value(rule)
            && base.is_none_or(|(best, _)| rule.priority(important) > best)
        {
            winners.push(Some(v));
        }
    }
    winners
}

/// Resolve the possible fonts of one element (or pseudo-element) from its parent's
/// possible fonts, its user-agent defaults and the author rules that match it.
fn cascade(
    parents: &[ResolvedFontStyle],
    tag: Option<&str>,
    rules: &[&FontStyleRule],
) -> Vec<ResolvedFontStyle> {
    let families = possible_winners(rules, |rule| {
        rule.font_family
            .as_deref()
            .map(|family| (family, rule.family_important))
    });
    let weights = possible_winners(rules, |rule| {
        rule.font_weight
            .map(|weight| (weight, rule.weight_important))
    });
    let font_styles = possible_winners(rules, |rule| {
        rule.font_style
            .as_deref()
            .map(|style| (style, rule.style_important))
    });

    let mut out: Vec<ResolvedFontStyle> = Vec::new();
    for parent in parents {
        let ua = user_agent_style(parent, tag);
        for family in &families {
            let font_family = match family.map(|f| (f, css_wide_keyword(f))) {
                None => ua.font_family.clone(),
                Some((_, Some(CssWideKeyword::Inherit))) => parent.font_family.clone(),
                Some((_, Some(CssWideKeyword::Revert))) => ua.font_family.clone(),
                Some((_, Some(CssWideKeyword::Initial))) => "serif".to_string(),
                Some((family, None)) => family.to_string(),
            };
            for weight in &weights {
                let font_weight = match weight {
                    None | Some(FontWeightValue::Keyword(CssWideKeyword::Revert)) => ua.font_weight,
                    Some(FontWeightValue::Absolute(weight)) => *weight,
                    Some(FontWeightValue::Bolder) => bolder(parent.font_weight),
                    Some(FontWeightValue::Lighter) => lighter(parent.font_weight),
                    Some(FontWeightValue::Keyword(CssWideKeyword::Inherit)) => parent.font_weight,
                    Some(FontWeightValue::Keyword(CssWideKeyword::Initial)) => 400,
                };
                for style in &font_styles {
                    let font_style = match style.map(|s| (s, css_wide_keyword(s))) {
                        None | Some((_, Some(CssWideKeyword::Revert))) => ua.font_style.clone(),
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
    /// `revert` and `revert-layer`: the user-agent origin's value (layers are not
    /// distinguished).
    Revert,
    Initial,
}

fn css_wide_keyword(value: &str) -> Option<CssWideKeyword> {
    match value.trim().to_ascii_lowercase().as_str() {
        "inherit" | "unset" => Some(CssWideKeyword::Inherit),
        "revert" | "revert-layer" => Some(CssWideKeyword::Revert),
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
                }
            }
            _ => {}
        }
    }
    (!text.is_empty() || quotes).then_some(GeneratedContent { text, quotes })
}

/// Read a CSS string's value after its opening `quote`, decoding escapes.
fn read_css_string(quote: char, chars: &mut std::iter::Peekable<std::str::Chars>) -> String {
    let mut text = String::new();
    while let Some(c) = chars.next() {
        if c == quote {
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
}

/// Parse CSS and extract rules that set font-family, font-weight, font-style or the
/// `content` of `::before`/`::after`, with CSS variable resolution.
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
                } else if c == q {
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
            if c == q {
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
#[derive(Debug, Clone, Copy, Default)]
struct Context {
    conditional: bool,
    layered: bool,
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
            let inner = match name.as_str() {
                "media" | "supports" | "container" | "scope" | "document" | "-moz-document" => {
                    Context {
                        conditional: true,
                        ..context
                    }
                }
                "layer" => Context {
                    layered: true,
                    ..context
                },
                _ => continue, // @font-face, @keyframes, @page, @property, …
            };
            parse_rule_list(block, css_vars, inner, parsed);
            continue;
        }
        push_style_rules(prelude, block, css_vars, context, parsed);
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

fn push_style_rules(
    prelude: &str,
    block: &str,
    css_vars: &CssVariables,
    context: Context,
    parsed: &mut ParsedCss,
) {
    let mut font_family: Option<(String, bool)> = None;
    let mut font_weight: Option<(FontWeightValue, bool)> = None;
    let mut font_style: Option<(String, bool)> = None;
    let mut content: Option<GeneratedContent> = None;

    // (nested rules in the block are ignored: their declarations come after them)
    let declarations = match find_top_level(block, &['{']) {
        Some(nested) => &block[..block[..nested].rfind(';').map_or(0, |i| i + 1)],
        None => block,
    };
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
            "font-family" => declare(
                &mut font_family,
                parse_font_family_value_with_vars(value, css_vars),
                important,
            ),
            "font-weight" => {
                if let Some(weight) = parse_font_weight(&resolve_css_var(value, css_vars)) {
                    declare(&mut font_weight, weight, important);
                }
            }
            "font-style" => {
                if let Some(style) = parse_font_style(&resolve_css_var(value, css_vars)) {
                    declare(&mut font_style, style, important);
                }
            }
            "font" => {
                if let Some((family, weight, style)) = parse_font_shorthand(value, css_vars) {
                    declare(&mut font_family, family, important);
                    declare(&mut font_weight, weight, important);
                    declare(&mut font_style, style, important);
                }
            }
            "content" => content = parse_content(&resolve_css_var(value, css_vars)),
            "quotes" => {
                if let Some(quotes) = parse_content(&resolve_css_var(value, css_vars)) {
                    parsed.quote_chars.push_str(&quotes.text);
                }
            }
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
        if pseudo.is_none() && !sets_font {
            continue; // `content` only applies to pseudo-elements
        }
        let (matchable, state_dependent) = strip_state_pseudo_classes(&base);
        let order = parsed.rules.len();
        parsed.rules.push(FontStyleRule {
            specificity: specificity(&base),
            parsed: Selector::parse(&matchable).ok(),
            selector: base,
            font_family: font_family.as_ref().map(|(v, _)| v.clone()),
            font_weight: font_weight.map(|(v, _)| v),
            font_style: font_style.as_ref().map(|(v, _)| v.clone()),
            family_important: font_family.as_ref().is_some_and(|(_, i)| *i),
            weight_important: font_weight.is_some_and(|(_, i)| i),
            style_important: font_style.as_ref().is_some_and(|(_, i)| *i),
            content: if pseudo.is_some() {
                content.clone()
            } else {
                None
            },
            pseudo,
            conditional: context.conditional || state_dependent,
            layered: context.layered,
            order,
        });
    }
}

/// Split a trailing `::before`/`::after` (or the legacy single-colon form) off a
/// selector. The base is `None` when the selector targets another pseudo-element.
fn split_pseudo_element(selector: &str) -> (Option<String>, Option<PseudoElement>) {
    for (suffix, pseudo) in [
        ("::before", PseudoElement::Before),
        ("::after", PseudoElement::After),
        (":before", PseudoElement::Before),
        (":after", PseudoElement::After),
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
        if base.ends_with('\\') {
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
        return (Some(base), Some(pseudo));
    }
    let mut other_pseudo_element = false;
    scan_top_level(selector, |i, c, _| {
        let rest = &selector[i..];
        other_pseudo_element |= c == ':'
            && (rest.starts_with("::")
                || [":first-line", ":first-letter"].iter().any(|legacy| {
                    rest.get(..legacy.len())
                        .is_some_and(|r| r.eq_ignore_ascii_case(legacy))
                }));
        false
    });
    if other_pseudo_element {
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

/// Rewrite interaction-state pseudo-classes so the static document can be matched.
/// Top-level ones are removed, matching as if the state applied; ones inside functional
/// pseudo-classes such as `:not(:hover)` become the never-matching `:not(*)`, the
/// document's resting state. Returns the selector to match and whether it depends on a
/// state (then the rule may or may not apply).
fn strip_state_pseudo_classes(selector: &str) -> (String, bool) {
    let mut found: Vec<(usize, usize, bool)> = Vec::new(); // (start, end, nested)
    scan_top_level(selector, |i, c, depth| {
        if c != ':' || selector[i..].starts_with("::") || selector[..i].ends_with(':') {
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
            let ends_name = after
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
        return (selector.to_string(), false);
    }
    let mut out = String::with_capacity(selector.len());
    let mut last = 0;
    for (start, end, nested) in found {
        out.push_str(&selector[last..start]);
        if nested {
            out.push_str(":not(*)");
        } else if out.is_empty()
            || out.ends_with(|c: char| c.is_whitespace() || matches!(c, '>' | '+' | '~'))
        {
            out.push('*');
        }
        last = end;
    }
    out.push_str(&selector[last..]);
    (out, true)
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
                i += 2;
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
