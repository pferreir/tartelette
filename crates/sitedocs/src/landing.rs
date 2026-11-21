//! The landing page: `site/index.md` laid out by `landing.html`.
//!
//! The Markdown's structure decides the layout (the comment at the top of
//! `site/index.md` explains it to whoever edits the text):
//!
//! - front matter: `title`, `description` and `header` (the running header);
//! - `#`: the page's title; an italic paragraph right after it is the
//!   subtitle, and the other paragraphs before the first `##` the intro;
//! - a `##` with `###` entries: a list of demos. In an entry, a paragraph
//!   holding an image is the screenshot, a list of `Label: value` items the
//!   details, and a final paragraph of links the actions (the first is the
//!   Run button, and the screenshot links to it too); the rest is prose;
//! - any other `##`: text on the left, code blocks on the right, quotes as
//!   small print;
//! - after a horizontal rule: the footer.

use std::fmt::Write as _;

use pulldown_cmark::{Event, HeadingLevel, MetadataBlockKind, Parser, Tag, html};

use crate::{Doc, closing, code_block, escape, options, plain_text};

const TEMPLATE: &str = include_str!("landing.html");

#[derive(Default)]
struct Section {
    heading: String,
    blocks: Vec<(usize, usize)>,
    demos: Vec<Demo>,
}

struct Demo {
    name: String,
    blocks: Vec<(usize, usize)>,
}

pub fn render(doc: &Doc, source: &str) -> Result<String, String> {
    let events: Vec<Event> = Parser::new_ext(source, options()).collect();

    // The top-level blocks, as (start, end) event indices.
    let mut blocks = Vec::new();
    let mut i = 0;
    while i < events.len() {
        let end = match &events[i] {
            Event::Start(_) => closing(&events, i),
            _ => i,
        };
        blocks.push((i, end));
        i = end + 1;
    }

    let mut meta: Vec<(String, String)> = Vec::new();
    let mut title = String::new();
    let mut subtitle = String::new();
    let mut intro = String::new();
    let mut sections: Vec<Section> = Vec::new();
    let mut footer: Vec<String> = Vec::new();
    let mut in_footer = false;

    for &(start, end) in &blocks {
        let block = &events[start..=end];
        match &block[0] {
            Event::Start(Tag::MetadataBlock(MetadataBlockKind::YamlStyle)) => {
                let text = plain_text(block);
                for line in text.lines() {
                    if let Some((k, v)) = line.split_once(':') {
                        meta.push((k.trim().to_string(), v.trim().to_string()));
                    }
                }
            }
            // Comments for whoever edits the Markdown.
            Event::Html(_) | Event::Start(Tag::HtmlBlock) => {}
            Event::Rule => in_footer = true,
            _ if in_footer => footer.push(strip_p(&html_of(doc, block))),
            Event::Start(Tag::Heading { level, .. }) => {
                let text = html_of(doc, &block[1..block.len() - 1]);
                match level {
                    HeadingLevel::H1 => title = text,
                    HeadingLevel::H2 => sections.push(Section {
                        heading: text,
                        ..Section::default()
                    }),
                    _ => {
                        let section = sections
                            .last_mut()
                            .ok_or("a demo (###) before any section (##)")?;
                        section.demos.push(Demo {
                            name: text,
                            blocks: Vec::new(),
                        });
                    }
                }
            }
            _ => {
                if let Some(section) = sections.last_mut() {
                    match section.demos.last_mut() {
                        Some(demo) => demo.blocks.push((start, end)),
                        None => section.blocks.push((start, end)),
                    }
                } else if subtitle.is_empty() && intro.is_empty() && is_emphasis(block) {
                    subtitle = html_of(doc, &block[2..block.len() - 2]);
                } else {
                    intro.push_str(&html_of(doc, block).replace("<p>", "<p class=\"intro\">"));
                }
            }
        }
    }

    let mut head = String::new();
    let _ = writeln!(head, "        <h1>{title}</h1>");
    if !subtitle.is_empty() {
        let _ = writeln!(head, "        <p class=\"sub\">{subtitle}</p>");
    }
    head.push_str(&intro);

    let mut body = String::new();
    let mut number = 0;
    for section in &sections {
        let _ = writeln!(body, "    <h2 class=\"sec\">{}</h2>", section.heading);
        if section.demos.is_empty() {
            body.push_str(&plain_section(doc, &events, &section.blocks));
        } else {
            for &b in &section.blocks {
                body.push_str(&html_of(doc, &events[b.0..=b.1]));
            }
            body.push_str("    <main>\n");
            for demo in &section.demos {
                number += 1;
                body.push_str(&demo_html(doc, &events, demo, number));
            }
            body.push_str("    </main>\n");
        }
    }

    let get = |k: &str| meta.iter().find(|(m, _)| m == k).map(|(_, v)| v.as_str());
    let plain_title = get("title").map_or_else(|| strip_tags(&title), str::to_string);
    Ok(TEMPLATE
        .replace("{{TITLE}}", &escape(&plain_title))
        .replace("{{DESCRIPTION}}", &escape(get("description").unwrap_or("")))
        .replace("{{HEADER}}", &escape(get("header").unwrap_or(&plain_title)))
        .replace("{{INTRO}}", &head)
        .replace("{{SECTIONS}}", &body)
        .replace("{{FOOTER}}", &footer.join(" "))
        .replace("{{REPO_URL}}", &doc.site.repo))
}

/// A run of events as HTML, links rewritten and code highlighted.
fn html_of(doc: &Doc, events: &[Event]) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i < events.len() {
        if let Event::Start(Tag::CodeBlock(_)) = &events[i] {
            let end = closing(events, i);
            out.push_str(&code_block(&events[i..=end]));
            i = end + 1;
            continue;
        }
        let next = events[i..]
            .iter()
            .position(|e| matches!(e, Event::Start(Tag::CodeBlock(_))))
            .map_or(events.len(), |p| i + p);
        html::push_html(
            &mut out,
            events[i..next].iter().map(|e| doc.rewrite(e.clone())),
        );
        i = next;
    }
    out
}

fn strip_p(html: &str) -> String {
    html.trim()
        .trim_start_matches("<p>")
        .trim_end_matches("</p>")
        .to_string()
}

fn strip_tags(html: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// A paragraph that is all one emphasis: `*like this*`.
fn is_emphasis(block: &[Event]) -> bool {
    block.len() >= 4
        && matches!(block[0], Event::Start(Tag::Paragraph))
        && matches!(block[1], Event::Start(Tag::Emphasis))
        && closing(block, 1) == block.len() - 2
}

/// A paragraph holding an image: its URL and alt text.
fn image(doc: &Doc, block: &[Event]) -> Option<(String, String)> {
    if !matches!(block.first(), Some(Event::Start(Tag::Paragraph))) {
        return None;
    }
    let at = block
        .iter()
        .position(|e| matches!(e, Event::Start(Tag::Image { .. })))?;
    let Event::Start(Tag::Image { dest_url, .. }) = doc.rewrite(block[at].clone()) else {
        return None;
    };
    let end = closing(block, at);
    Some((dest_url.to_string(), plain_text(&block[at + 1..end])))
}

/// A list whose every item is `Label: value`: (label, value HTML) pairs.
fn details(doc: &Doc, block: &[Event]) -> Option<Vec<(String, String)>> {
    if !matches!(block.first(), Some(Event::Start(Tag::List(_)))) {
        return None;
    }
    let mut rows = Vec::new();
    let mut i = 1;
    while i < block.len() - 1 {
        let end = closing(block, i);
        let html = strip_p(&html_of(doc, &block[i + 1..end]));
        let (label, value) = html.split_once(':')?;
        if label.contains('<') || label.len() > 20 {
            return None;
        }
        // Inline code in the details is keys to press.
        let value = value
            .trim()
            .replace("<code>", "<kbd>")
            .replace("</code>", "</kbd>");
        rows.push((label.trim().to_string(), value));
        i = end + 1;
    }
    Some(rows)
}

/// A paragraph of nothing but links: (URL, text HTML) pairs.
fn actions(doc: &Doc, block: &[Event]) -> Option<Vec<(String, String)>> {
    if !matches!(block.first(), Some(Event::Start(Tag::Paragraph))) {
        return None;
    }
    let mut links = Vec::new();
    let mut i = 1;
    while i < block.len() - 1 {
        match &block[i] {
            Event::Start(Tag::Link { .. }) => {
                let end = closing(block, i);
                let Event::Start(Tag::Link { dest_url, .. }) = doc.rewrite(block[i].clone()) else {
                    unreachable!()
                };
                links.push((dest_url.to_string(), html_of(doc, &block[i + 1..end])));
                i = end + 1;
            }
            Event::SoftBreak | Event::HardBreak => i += 1,
            Event::Text(t) if t.trim().trim_matches('·').trim().is_empty() => i += 1,
            _ => return None,
        }
    }
    (!links.is_empty()).then_some(links)
}

fn demo_html(doc: &Doc, events: &[Event], demo: &Demo, number: usize) -> String {
    let mut shot = None;
    let mut rows = Vec::new();
    let mut links = Vec::new();
    let mut prose = String::new();
    for (n, &(start, end)) in demo.blocks.iter().enumerate() {
        let block = &events[start..=end];
        if shot.is_none()
            && let Some(img) = image(doc, block)
        {
            shot = Some(img);
        } else if let Some(d) = details(doc, block) {
            rows.extend(d);
        } else if n == demo.blocks.len() - 1
            && let Some(a) = actions(doc, block)
        {
            links = a;
        } else {
            prose.push_str(&html_of(doc, block));
        }
    }

    let mut out = String::new();
    let _ = writeln!(out, "      <section class=\"demo\">");
    let _ = writeln!(out, "        <div class=\"no\">{number:02}</div>");
    if let Some((src, alt)) = &shot {
        let img = format!(
            "<img src=\"{}\" alt=\"{}\" loading=\"lazy\">",
            escape(src),
            escape(alt)
        );
        match links.first() {
            Some((href, _)) => {
                let _ = writeln!(
                    out,
                    "        <a class=\"shot\" href=\"{}\">{img}</a>",
                    escape(href)
                );
            }
            None => {
                let _ = writeln!(out, "        <div class=\"shot\">{img}</div>");
            }
        }
    }
    let _ = writeln!(out, "        <div class=\"text\">");
    let _ = writeln!(out, "          <h3>{}</h3>", demo.name);
    out.push_str(&prose);
    if !rows.is_empty() {
        out.push_str("          <dl>\n");
        for (label, value) in &rows {
            let _ = writeln!(out, "            <dt>{label}</dt><dd>{value}</dd>");
        }
        out.push_str("          </dl>\n");
    }
    if !links.is_empty() {
        out.push_str("          <div class=\"actions\">\n");
        for (n, (href, text)) in links.iter().enumerate() {
            let class = if n == 0 { " class=\"run\"" } else { "" };
            let _ = writeln!(
                out,
                "            <a{class} href=\"{}\">{text}</a>",
                escape(href)
            );
        }
        out.push_str("          </div>\n");
    }
    out.push_str("        </div>\n      </section>\n");
    out
}

/// A section that isn't a list of demos: text left, code right.
fn plain_section(doc: &Doc, events: &[Event], blocks: &[(usize, usize)]) -> String {
    let mut text = String::new();
    let mut code = String::new();
    for &(start, end) in blocks {
        let block = &events[start..=end];
        if matches!(block[0], Event::Start(Tag::CodeBlock(_))) {
            code.push_str(&html_of(doc, block));
        } else {
            text.push_str(&html_of(doc, block));
        }
    }
    let mut out = String::from("    <div class=\"local\">\n      <div>\n");
    out.push_str(&text);
    out.push_str("      </div>\n");
    if !code.is_empty() {
        let _ = writeln!(out, "      <div>{code}</div>");
    }
    out.push_str("    </div>\n");
    out
}
