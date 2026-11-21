//! Renders the demo site's Markdown as HTML: the documentation under `docs/`
//! and the landing page, `site/index.md`.
//!
//!     sitedocs ROOT OUT REPO_URL [--var NAME=VALUE]... [--drop NAME]...
//!
//! ROOT is the repository. `docs/x.md` becomes `OUT/docs/x.html` (and a
//! `README.md` an `index.html`); `site/index.md` becomes `OUT/index.html`.
//! Links between them are rewritten to match, links to other files in
//! `site/` stay relative, and links to anything else in the repository
//! (source files, board files) go to REPO_URL. Headings get the same ids
//! GitHub gives them, so `page.md#some-heading` keeps working.
//!
//! `{{NAME}}` in the Markdown is replaced by `--var NAME=VALUE` (and
//! `{{REPO_URL}}` by REPO_URL); `--drop NAME` removes everything between
//! `<!-- NAME -->` and `<!-- /NAME -->`. Used by `scripts/build-site.sh`.

use std::{
    collections::HashMap,
    fmt::Write as _,
    fs,
    path::{Component, Path, PathBuf},
    process::ExitCode,
};

use pulldown_cmark::{CodeBlockKind, CowStr, Event, HeadingLevel, Options, Parser, Tag, html};

mod highlight;
mod landing;

const TEMPLATE: &str = include_str!("page.html");

/// Top-level pages, in reading order (the overview first). Anything else
/// follows, by name.
const ORDER: &[&str] = &[
    "README",
    "running",
    "boards",
    "devices",
    "rp2040",
    "usb",
    "development",
];

/// Where the docs and the landing page are, in the repository and on the site.
const DOCS: &str = "docs";
const SITE: &str = "site";

struct Page {
    /// Path under the docs directory, with `/` separators: `devices/led.md`.
    rel: String,
    /// Path on the site: `docs/devices/led.html`.
    out: String,
    title: String,
    source: String,
}

/// What links are resolved against.
struct Site {
    root: PathBuf,
    repo: String,
    pages: Vec<Page>,
}

/// One Markdown file being rendered: its directory in the repository and
/// its path on the site.
struct Doc<'a> {
    site: &'a Site,
    dir: String,
    out: String,
}

struct Args {
    root: PathBuf,
    out: PathBuf,
    repo: String,
    vars: Vec<(String, String)>,
    drop: Vec<String>,
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("sitedocs: {e}");
            eprintln!("usage: sitedocs ROOT OUT REPO_URL [--var NAME=VALUE]... [--drop NAME]...");
            return ExitCode::FAILURE;
        }
    };
    match run(&args) {
        Ok(n) => {
            eprintln!("sitedocs: {n} pages in {}", args.out.display());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("sitedocs: {e}");
            ExitCode::FAILURE
        }
    }
}

fn parse_args() -> Result<Args, String> {
    let mut positional = Vec::new();
    let mut vars = Vec::new();
    let mut drop = Vec::new();
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--var" => {
                let v = it.next().ok_or("--var needs NAME=VALUE")?;
                let (k, v) = v.split_once('=').ok_or("--var needs NAME=VALUE")?;
                vars.push((k.to_string(), v.to_string()));
            }
            "--drop" => drop.push(it.next().ok_or("--drop needs a NAME")?),
            _ => positional.push(a),
        }
    }
    let [root, out, repo] = <[String; 3]>::try_from(positional)
        .map_err(|_| "needs ROOT, OUT and REPO_URL".to_string())?;
    let repo = repo.trim_end_matches('/').to_string();
    vars.push(("REPO_URL".into(), repo.clone()));
    Ok(Args {
        root: PathBuf::from(root)
            .canonicalize()
            .map_err(|e| format!("ROOT: {e}"))?,
        out: out.into(),
        repo,
        vars,
        drop,
    })
}

/// Substitutes the variables and drops the marked regions.
fn preprocess(source: &str, args: &Args) -> String {
    let mut s = source.to_string();
    for name in &args.drop {
        let (open, close) = (format!("<!-- {name} -->"), format!("<!-- /{name} -->"));
        while let (Some(a), Some(b)) = (s.find(&open), s.find(&close)) {
            if b < a {
                break;
            }
            s.replace_range(a..b + close.len(), "");
        }
    }
    for (k, v) in &args.vars {
        s = s.replace(&format!("{{{{{k}}}}}"), v);
    }
    s
}

fn run(args: &Args) -> Result<usize, String> {
    let docs = args.root.join(DOCS);
    let mut files = Vec::new();
    collect(&docs, &docs, &mut files)?;
    let mut pages: Vec<Page> = files
        .into_iter()
        .map(|rel| {
            let source = fs::read_to_string(docs.join(&rel)).map_err(|e| format!("{rel}: {e}"))?;
            let source = preprocess(&source, args);
            let title = first_heading(&source).unwrap_or_else(|| rel.clone());
            Ok(Page {
                out: format!("{DOCS}/{}", html_name(&rel)),
                rel,
                title,
                source,
            })
        })
        .collect::<Result<_, String>>()?;
    sort_pages(&mut pages);
    let site = Site {
        root: args.root.clone(),
        repo: args.repo.clone(),
        pages,
    };

    let index = format!("{DOCS}/index.html");
    for page in &site.pages {
        let dir = match page.rel.rsplit_once('/') {
            Some((d, _)) => format!("{DOCS}/{d}"),
            None => DOCS.to_string(),
        };
        let doc = Doc {
            site: &site,
            dir,
            out: page.out.clone(),
        };
        let body = render(&doc, &page.source);
        write_page(&args.out, &site, &page.out, &page.title, &page.rel, &body)?;
    }
    if !site.pages.iter().any(|p| p.out == index) {
        // No docs/README.md: list the pages instead.
        let mut body = String::from("<h1>Documentation</h1>\n<ul class=\"toc\">\n");
        for p in &site.pages {
            let _ = writeln!(
                body,
                "<li><a href=\"{}\">{}</a></li>",
                relative(&index, &p.out),
                inline_html(&p.title)
            );
        }
        body.push_str("</ul>\n");
        write_page(&args.out, &site, &index, "Documentation", "", &body)?;
    }

    let mut n = site.pages.len();
    let landing = args.root.join(SITE).join("index.md");
    if landing.exists() {
        let source =
            fs::read_to_string(&landing).map_err(|e| format!("{}: {e}", landing.display()))?;
        let doc = Doc {
            site: &site,
            dir: SITE.to_string(),
            out: "index.html".into(),
        };
        let html = landing::render(&doc, &preprocess(&source, args))?;
        write_file(&args.out.join("index.html"), &html)?;
        n += 1;
    }
    Ok(n)
}

fn collect(base: &Path, dir: &Path, files: &mut Vec<String>) -> Result<(), String> {
    let entries = fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for entry in entries {
        let path = entry.map_err(|e| e.to_string())?.path();
        if path.is_dir() {
            collect(base, &path, files)?;
        } else if path.extension().is_some_and(|e| e == "md") {
            files.push(rel_path(base, &path));
        }
    }
    Ok(())
}

fn rel_path(base: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(base).unwrap_or(path);
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn html_name(rel: &str) -> String {
    match rel.rsplit_once('/') {
        Some((dir, "README.md")) => format!("{dir}/index.html"),
        None if rel == "README.md" => "index.html".into(),
        _ => format!("{}.html", rel.trim_end_matches(".md")),
    }
}

/// Top-level pages in `ORDER`, each directory's index right after its
/// position, and a directory's pages in the order its README links to them.
fn sort_pages(pages: &mut [Page]) {
    let readmes: HashMap<String, String> = pages
        .iter()
        .filter_map(|p| {
            p.rel
                .strip_suffix("README.md")
                .map(|d| (d.to_string(), p.source.clone()))
        })
        .collect();
    let key = |p: &Page| {
        let (dir, file) = match p.rel.rsplit_once('/') {
            Some((d, f)) => (format!("{d}/"), f.to_string()),
            None => (String::new(), p.rel.clone()),
        };
        let top = p
            .rel
            .split('/')
            .next()
            .unwrap_or("")
            .trim_end_matches(".md")
            .to_string();
        let top_rank = ORDER.iter().position(|o| *o == top).unwrap_or(ORDER.len());
        let in_dir = if file == "README.md" || dir.is_empty() {
            0
        } else {
            readmes
                .get(&dir)
                .and_then(|r| r.find(&format!("({file})")))
                .map_or(usize::MAX, |i| i + 1)
        };
        (top_rank, top, in_dir, file)
    };
    pages.sort_by_key(key);
}

fn first_heading(source: &str) -> Option<String> {
    source
        .lines()
        .find_map(|l| l.strip_prefix("# "))
        .map(|t| t.trim().to_string())
}

/// A title's Markdown (backticks only, in practice) as HTML.
fn inline_html(title: &str) -> String {
    let mut out = String::new();
    html::push_html(&mut out, Parser::new(title));
    out.trim()
        .trim_start_matches("<p>")
        .trim_end_matches("</p>")
        .to_string()
}

/// GitHub's heading ids: lower case, spaces to dashes, punctuation dropped.
fn slug(text: &str) -> String {
    text.chars()
        .filter_map(|c| match c {
            ' ' => Some('-'),
            '-' | '_' => Some(c),
            c if c.is_alphanumeric() => Some(c.to_lowercase().next().unwrap_or(c)),
            _ => None,
        })
        .collect()
}

fn options() -> Options {
    Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_YAML_STYLE_METADATA_BLOCKS
}

/// The plain text of a run of inline events.
fn plain_text(events: &[Event]) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Text(t) | Event::Code(t) => Some(t.as_ref()),
            _ => None,
        })
        .collect()
}

/// The index of the event that closes the one at `start`.
fn closing(events: &[Event], start: usize) -> usize {
    let mut depth = 0usize;
    for (i, e) in events.iter().enumerate().skip(start) {
        match e {
            Event::Start(_) => depth += 1,
            Event::End(_) => {
                depth -= 1;
                if depth == 0 {
                    return i;
                }
            }
            _ => {}
        }
    }
    events.len() - 1
}

/// A docs page's body.
fn render(doc: &Doc, source: &str) -> String {
    let events: Vec<Event> = Parser::new_ext(source, options()).collect();
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut out_events = Vec::with_capacity(events.len());
    let mut i = 0;
    while i < events.len() {
        match &events[i] {
            Event::Start(Tag::Heading { level, .. }) => {
                // Gather the heading's text for its id, then emit it by hand.
                let h = level_num(*level);
                let end = closing(&events, i);
                let base = slug(&plain_text(&events[i + 1..end]));
                let n = seen.entry(base.clone()).or_insert(0);
                let id = if *n == 0 {
                    base.clone()
                } else {
                    format!("{base}-{n}")
                };
                *n += 1;
                if h == 1 {
                    out_events.push(Event::Html(format!("<h1 id=\"{id}\">").into()));
                } else {
                    out_events.push(Event::Html(
                        format!("<h{h} id=\"{id}\"><a class=\"anchor\" href=\"#{id}\" aria-hidden=\"true\">#</a>").into(),
                    ));
                }
                out_events.extend(events[i + 1..end].iter().map(|e| doc.rewrite(e.clone())));
                out_events.push(Event::Html(format!("</h{h}>\n").into()));
                i = end + 1;
            }
            Event::Start(Tag::CodeBlock(_)) => {
                let end = closing(&events, i);
                out_events.push(Event::Html(code_block(&events[i..=end]).into()));
                i = end + 1;
            }
            e => {
                out_events.push(doc.rewrite(e.clone()));
                i += 1;
            }
        }
    }
    let mut body = String::new();
    html::push_html(&mut body, out_events.into_iter());
    // Tables scroll on their own on a narrow screen.
    body.replace("<table>", "<div class=\"table\"><table>")
        .replace("</table>", "</table></div>")
}

/// A code block's events (start to end), highlighted.
fn code_block(events: &[Event]) -> String {
    let lang = match events.first() {
        Some(Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(info)))) => {
            info.split_whitespace().next().unwrap_or("").to_string()
        }
        _ => String::new(),
    };
    let code: String = events
        .iter()
        .filter_map(|e| match e {
            Event::Text(t) => Some(t.as_ref()),
            _ => None,
        })
        .collect();
    let class = if lang.is_empty() {
        String::new()
    } else {
        format!(" class=\"language-{lang}\"")
    };
    let inner = highlight::highlight(&lang, &code).unwrap_or_else(|| escape(&code));
    format!("<pre><code{class}>{inner}</code></pre>\n")
}

fn level_num(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

impl Doc<'_> {
    fn rewrite<'a>(&self, event: Event<'a>) -> Event<'a> {
        match event {
            Event::Start(Tag::Link {
                link_type,
                dest_url,
                title,
                id,
            }) => Event::Start(Tag::Link {
                link_type,
                dest_url: self.link(&dest_url).into(),
                title,
                id,
            }),
            Event::Start(Tag::Image {
                link_type,
                dest_url,
                title,
                id,
            }) => {
                let dest_url: CowStr = match self.link(&dest_url) {
                    // Images are served raw, not from the page on GitHub.
                    u if u.starts_with(&self.site.repo) => u.replacen("/blob/", "/raw/", 1).into(),
                    u => u.into(),
                };
                Event::Start(Tag::Image {
                    link_type,
                    dest_url,
                    title,
                    id,
                })
            }
            e => e,
        }
    }

    /// Where a relative link in the Markdown should point on the site.
    fn link(&self, dest: &str) -> String {
        if dest.starts_with('#')
            || dest.contains("://")
            || dest.starts_with("mailto:")
            || dest.starts_with('/')
        {
            return dest.to_string();
        }
        let split = dest.find(['?', '#']).unwrap_or(dest.len());
        let (path, tail) = dest.split_at(split);
        let target = normalize(&format!("{}/{path}", self.dir));
        let is_dir =
            path.is_empty() || path.ends_with('/') || self.site.root.join(&target).is_dir();

        // Another page of the docs.
        if let Some(in_docs) = target
            .strip_prefix(&format!("{DOCS}/"))
            .or((target == DOCS).then_some(""))
        {
            let md = match (is_dir, in_docs.is_empty()) {
                (true, true) => "README.md".to_string(),
                (true, false) => format!("{in_docs}/README.md"),
                (false, _) => in_docs.to_string(),
            };
            let out = match self.site.pages.iter().find(|p| p.rel == md) {
                Some(p) => Some(p.out.clone()),
                // The generated index.
                None if is_dir && in_docs.is_empty() => Some(format!("{DOCS}/index.html")),
                None => None,
            };
            if let Some(out) = out {
                return format!("{}{tail}", relative(&self.out, &out));
            }
        }
        // Something served next to the landing page: images, the web app.
        if let Some(served) = target.strip_prefix(&format!("{SITE}/")) {
            let slash = if path.ends_with('/') { "/" } else { "" };
            return format!("{}{slash}{tail}", relative(&self.out, served));
        }
        if target == SITE {
            return format!("{}{tail}", relative(&self.out, ""));
        }
        let kind = if is_dir { "tree" } else { "blob" };
        format!("{}/{kind}/HEAD/{target}{tail}", self.site.repo)
    }
}

/// `a/b/../c` → `a/c`.
fn normalize(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for c in Path::new(path).components() {
        match c {
            Component::ParentDir => {
                parts.pop();
            }
            Component::Normal(s) => parts.push(s.to_str().unwrap_or("")),
            _ => {}
        }
    }
    parts.join("/")
}

/// The link from the page at site path `from` to site path `to`.
fn relative(from: &str, to: &str) -> String {
    let from_dirs: Vec<&str> =
        from.split('/').collect::<Vec<_>>()[..from.matches('/').count()].to_vec();
    let to_parts: Vec<&str> = to.split('/').collect();
    let to_dirs = &to_parts[..to_parts.len() - 1];
    let common = from_dirs
        .iter()
        .zip(to_dirs)
        .take_while(|(a, b)| a == b)
        .count();
    let up = "../".repeat(from_dirs.len() - common);
    let rest = to_parts[common..].join("/");
    match (up.is_empty(), rest.is_empty()) {
        (true, true) => "./".into(),
        _ => format!("{up}{rest}"),
    }
}

fn write_file(path: &Path, contents: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    fs::write(path, contents).map_err(|e| format!("{}: {e}", path.display()))
}

fn write_page(
    out: &Path,
    site: &Site,
    name: &str,
    title: &str,
    rel: &str,
    body: &str,
) -> Result<(), String> {
    let mut nav = String::from("<ul>\n");
    let mut open_dir: Option<String> = None;
    for p in &site.pages {
        let dir = p.rel.rsplit_once('/').map(|(d, _)| d.to_string());
        let is_index = p.rel.ends_with("README.md");
        if open_dir.is_some() && dir != open_dir {
            nav.push_str("</ul></li>\n");
            open_dir = None;
        }
        let current = if p.out == name {
            " aria-current=\"page\""
        } else {
            ""
        };
        let href = relative(name, &p.out);
        if is_index && dir.is_some() {
            let _ = write!(
                nav,
                "<li><a href=\"{href}\"{current}>{}</a>\n<ul class=\"sub\">\n",
                inline_html(&p.title)
            );
            open_dir = dir;
        } else if open_dir.is_some() {
            // A part's page: its kind, from the file name.
            let stem = p
                .rel
                .rsplit('/')
                .next()
                .unwrap_or("")
                .trim_end_matches(".md");
            let _ = writeln!(
                nav,
                "<li><a href=\"{href}\"{current}><code>{stem}</code></a></li>"
            );
        } else {
            let _ = writeln!(
                nav,
                "<li><a href=\"{href}\"{current}>{}</a></li>",
                inline_html(&p.title)
            );
        }
    }
    if open_dir.is_some() {
        nav.push_str("</ul></li>\n");
    }
    nav.push_str("</ul>\n");

    let plain_title: String = title.chars().filter(|c| *c != '`').collect();
    let repo = &site.repo;
    let source = if rel.is_empty() {
        String::new()
    } else {
        format!("Generated from <a href=\"{repo}/blob/HEAD/{DOCS}/{rel}\">{DOCS}/{rel}</a>")
    };
    let html = TEMPLATE
        .replace("{{TITLE}}", &escape(&plain_title))
        .replace("{{UP}}", &"../".repeat(name.matches('/').count()))
        .replace(
            "{{DOCS_INDEX}}",
            &relative(name, &format!("{DOCS}/index.html")),
        )
        .replace("{{REPO_URL}}", repo)
        .replace("{{NAV}}", &nav)
        .replace("{{SOURCE}}", &source)
        .replace("{{BODY}}", body);
    write_file(&out.join(name), &html)
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_match_github() {
        assert_eq!(
            slug("`encoder`: a rotary encoder"),
            "encoder-a-rotary-encoder"
        );
        assert_eq!(slug("SD cards"), "sd-cards");
        assert_eq!(slug("Adding a device"), "adding-a-device");
    }

    #[test]
    fn paths() {
        assert_eq!(html_name("devices/README.md"), "devices/index.html");
        assert_eq!(html_name("boards.md"), "boards.html");
        assert_eq!(normalize("docs/devices/../running.md"), "docs/running.md");
        assert_eq!(
            relative("docs/devices/led.html", "docs/boards.html"),
            "../boards.html"
        );
        assert_eq!(
            relative("index.html", "docs/boards.html"),
            "docs/boards.html"
        );
        assert_eq!(
            relative("docs/boards.html", "img/logo.png"),
            "../img/logo.png"
        );
        assert_eq!(relative("docs/a.html", "docs/b.html"), "b.html");
    }
}
