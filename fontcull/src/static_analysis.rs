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

    let mut per_family: HashMap<String, HashSet<char>> = HashMap::new();
    let mut per_variant: HashMap<FontVariantKey, HashSet<char>> = HashMap::new();

    // For each element with text, determine which font-family applies
    // by checking CSS rules in order of specificity (simplified: last match wins)
    let all_elements = Selector::parse("*").unwrap();

    for element in document.select(&all_elements) {
        // Get all direct text nodes (not from children)
        let text: String = element
            .children()
            .filter_map(|child| child.value().as_text().map(|t| t.to_string()))
            .collect();

        if text.trim().is_empty() {
            continue;
        }

        // Find which font styling applies to this element
        let resolved = find_font_style_for_element(&element, &font_rules);

        // Add characters to the per-family map
        let family_chars = per_family.entry(resolved.font_family.clone()).or_default();
        for c in text.chars() {
            family_chars.insert(c);
        }

        // Add characters to the per-variant map
        let key = FontVariantKey {
            family: resolved.font_family,
            weight: resolved.font_weight,
            style: resolved.font_style,
        };
        let variant_chars = per_variant.entry(key).or_default();
        for c in text.chars() {
            variant_chars.insert(c);
        }
    }

    (per_family, per_variant)
}

/// Resolved font styling for an element.
struct ResolvedFontStyle {
    font_family: String,
    font_weight: u16,
    font_style: String,
}

/// A CSS rule that sets font properties
#[derive(Debug)]
struct FontStyleRule {
    selector: String,
    font_family: Option<String>,
    font_weight: Option<u16>,
    font_style: Option<String>,
}

/// Parse a CSS font-weight value to a numeric weight.
/// Handles named weights and numeric values.
fn parse_font_weight(value: &str) -> Option<u16> {
    match value.trim().to_lowercase().as_str() {
        "normal" => Some(400),
        "bold" => Some(700),
        "lighter" => Some(100), // simplified — depends on inherited weight
        "bolder" => Some(700),  // simplified — depends on inherited weight
        other => other.parse().ok(),
    }
}

/// Parse CSS and extract rules that set font-family, font-weight, or font-style,
/// with CSS variable resolution.
fn parse_font_style_rules_with_vars(css: &str, css_vars: &CssVariables) -> Vec<FontStyleRule> {
    let mut rules = Vec::new();

    let chars = css.chars().peekable();
    let mut current_selector = String::new();
    let mut in_block = false;
    let mut block_content = String::new();

    for c in chars {
        if c == '{' {
            in_block = true;
            block_content.clear();
        } else if c == '}' {
            in_block = false;

            let font_family = extract_font_family_with_vars(&block_content, css_vars);
            let font_weight = extract_font_weight_with_vars(&block_content, css_vars);
            let font_style = extract_font_style_with_vars(&block_content, css_vars);

            // Only add a rule if it sets at least one font property
            if font_family.is_some() || font_weight.is_some() || font_style.is_some() {
                let selector = current_selector.trim().to_string();
                if !selector.is_empty() && !selector.starts_with('@') {
                    rules.push(FontStyleRule {
                        selector,
                        font_family,
                        font_weight,
                        font_style,
                    });
                }
            }

            current_selector.clear();
        } else if in_block {
            block_content.push(c);
        } else {
            current_selector.push(c);
        }
    }

    rules
}

/// Extract font-weight value from a CSS declaration block, with CSS variable resolution.
fn extract_font_weight_with_vars(block: &str, css_vars: &CssVariables) -> Option<u16> {
    for declaration in block.split(';') {
        let declaration = declaration.trim();
        if let Some(value) = declaration.strip_prefix("font-weight:") {
            let resolved = resolve_css_var(value.trim(), css_vars);
            return parse_font_weight(&resolved);
        }
    }
    None
}

/// Extract font-style value from a CSS declaration block, with CSS variable resolution.
fn extract_font_style_with_vars(block: &str, css_vars: &CssVariables) -> Option<String> {
    for declaration in block.split(';') {
        let declaration = declaration.trim();
        if let Some(value) = declaration.strip_prefix("font-style:") {
            let resolved = resolve_css_var(value.trim(), css_vars);
            let style = resolved.trim().to_lowercase();
            if style == "normal" || style == "italic" || style == "oblique" {
                return Some(style);
            }
        }
    }
    None
}

/// Find the resolved font styling for an element by checking CSS rules and inheritance.
fn find_font_style_for_element(
    element: &scraper::ElementRef,
    rules: &[FontStyleRule],
) -> ResolvedFontStyle {
    // Collect the full ancestor chain (element itself + parents), nearest first
    let mut chain: Vec<scraper::ElementRef> = Vec::new();
    chain.push(*element);
    for ancestor in element.ancestors() {
        if let Some(el) = scraper::ElementRef::wrap(ancestor) {
            chain.push(el);
        }
    }

    // Resolve inherited font properties by walking from root to element.
    // Later (more specific) rules override earlier ones.
    let mut font_family = "sans-serif".to_string();
    let mut font_weight: u16 = 400;
    let mut font_style = "normal".to_string();

    // Walk from root ancestor down to element (reverse of chain)
    for el in chain.iter().rev() {
        for rule in rules {
            if let Ok(selector) = Selector::parse(&rule.selector) {
                if selector.matches(el) {
                    if let Some(ref ff) = rule.font_family {
                        font_family = ff.clone();
                    }
                    if let Some(fw) = rule.font_weight {
                        font_weight = fw;
                    }
                    if let Some(ref fs) = rule.font_style {
                        font_style = fs.clone();
                    }
                }
            }
        }

        // Also check for HTML elements that imply weight/style
        let tag = el.value().name();
        match tag {
            "b" | "strong" => font_weight = 700,
            "i" | "em" | "cite" | "dfn" => font_style = "italic".to_string(),
            _ => {}
        }
    }

    ResolvedFontStyle { font_family, font_weight, font_style }
}

/// Extract font-family value from a CSS declaration block, with CSS variable resolution
fn extract_font_family_with_vars(block: &str, css_vars: &CssVariables) -> Option<String> {
    // Look for font-family: value; or font: ... value;
    for declaration in block.split(';') {
        let declaration = declaration.trim();

        if let Some(value) = declaration.strip_prefix("font-family:") {
            return Some(parse_font_family_value_with_vars(value, css_vars));
        }

        // Handle shorthand 'font' property (simplified - just look for font-family at end)
        if declaration.starts_with("font:") {
            // The font shorthand is complex; for now just skip it
            // TODO: properly parse font shorthand
        }
    }

    None
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
        assert!(normal_400.contains(&'N'), "400 normal should have 'N' from 'Normal '");
        assert!(!normal_400.contains(&'B'), "400 normal should NOT have 'B' from bold h1");

        let bold_700 = &analysis.chars_per_font_variant[&FontVariantKey {
            family: "TestFont".to_string(),
            weight: 700,
            style: "normal".to_string(),
        }];
        assert!(bold_700.contains(&'B'), "700 normal should have 'B' from 'Bold'");

        let italic_400 = &analysis.chars_per_font_variant[&FontVariantKey {
            family: "TestFont".to_string(),
            weight: 400,
            style: "italic".to_string(),
        }];
        assert!(italic_400.contains(&'i'), "400 italic should have 'i' from 'italic'");
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
        assert!(bold.contains(&'b'), "bold variant should have 'b' from <b>bold</b>");
        assert!(bold.contains(&'s'), "bold variant should have 's' from <strong>strong</strong>");

        // <i> should be italic
        let italic = &analysis.chars_per_font_variant[&FontVariantKey {
            family: "MyFont".to_string(),
            weight: 400,
            style: "italic".to_string(),
        }];
        assert!(italic.contains(&'i'), "italic variant should have 'i' from <i>italic</i>");

        // Normal text
        let normal = &analysis.chars_per_font_variant[&FontVariantKey {
            family: "MyFont".to_string(),
            weight: 400,
            style: "normal".to_string(),
        }];
        assert!(normal.contains(&'N'), "normal variant should have 'N' from 'Normal '");
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
