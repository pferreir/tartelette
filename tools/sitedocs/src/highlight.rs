//! Syntax highlighting for the docs' code blocks: TOML (board files) and
//! shell (command lines). Small hand-written scanners, enough for what the
//! docs contain; anything else is left plain.
//!
//! Tokens become `<span class="...">`: `c` comment, `s` string, `n` number
//! or boolean, `k` key, `h` table header, `x` command, `f` flag, `v`
//! variable, `p` punctuation.

pub fn highlight(lang: &str, code: &str) -> Option<String> {
    match lang {
        "toml" => Some(toml(code)),
        "sh" | "bash" | "shell" | "console" => Some(sh(code)),
        _ => None,
    }
}

struct Out(String);

impl Out {
    fn text(&mut self, s: &str) {
        for c in s.chars() {
            match c {
                '&' => self.0.push_str("&amp;"),
                '<' => self.0.push_str("&lt;"),
                '>' => self.0.push_str("&gt;"),
                c => self.0.push(c),
            }
        }
    }

    fn span(&mut self, class: &str, s: &str) {
        if s.is_empty() {
            return;
        }
        self.0.push_str("<span class=\"");
        self.0.push_str(class);
        self.0.push_str("\">");
        self.text(s);
        self.0.push_str("</span>");
    }
}

/// The byte length of the quoted string at the start of `s`, closing quote
/// included (or to the end of the line if it isn't closed).
fn quoted(s: &str) -> usize {
    let q = s.as_bytes()[0];
    let mut i = 1;
    let b = s.as_bytes();
    while i < b.len() {
        if b[i] == b'\\' && q == b'"' {
            i += 2;
            continue;
        }
        if b[i] == q {
            return i + 1;
        }
        i += 1;
    }
    b.len()
}

fn toml(code: &str) -> String {
    let mut out = Out(String::new());
    // Inside a multi-line string: its closing delimiter.
    let mut multi: Option<&str> = None;
    // Open brackets and braces: a `[` inside an array isn't a table header.
    let mut depth = 0usize;
    for line in code.split_inclusive('\n') {
        let mut rest = line;
        if let Some(delim) = multi {
            match rest.find(delim) {
                Some(end) => {
                    out.span("s", &rest[..end + 3]);
                    rest = &rest[end + 3..];
                    multi = None;
                }
                None => {
                    out.span("s", rest);
                    continue;
                }
            }
        }
        let trimmed = rest.trim_start();
        if depth == 0 && rest.len() == line.len() && trimmed.starts_with('[') {
            // A table header: `[parts.knob]`, `[[flash]]`.
            out.text(&rest[..rest.len() - trimmed.len()]);
            let end = trimmed.find(']').map_or(trimmed.len(), |i| {
                if trimmed[i + 1..].starts_with(']') {
                    i + 2
                } else {
                    i + 1
                }
            });
            out.span("h", &trimmed[..end]);
            rest = &trimmed[end..];
        }
        // Keys appear at the start of a line and after `{` or `,` inside an
        // inline table, before an `=`.
        let mut key_ok = true;
        while !rest.is_empty() {
            let c = rest.chars().next().unwrap_or(' ');
            let n = match c {
                '#' => {
                    let end = rest.find('\n').unwrap_or(rest.len());
                    out.span("c", &rest[..end]);
                    end
                }
                '"' | '\'' => {
                    let delim = if c == '"' { "\"\"\"" } else { "'''" };
                    let len = if rest.starts_with(delim) {
                        match rest[3..].find(delim) {
                            Some(e) => e + 6,
                            None => {
                                multi = Some(delim);
                                rest.len()
                            }
                        }
                    } else {
                        quoted(rest)
                    };
                    let is_key = key_ok && rest[len..].trim_start().starts_with('=');
                    out.span(if is_key { "k" } else { "s" }, &rest[..len]);
                    key_ok = false;
                    len
                }
                '{' | '}' | '[' | ']' | ',' | '=' => {
                    out.span("p", &rest[..1]);
                    match c {
                        '{' | '[' => depth += 1,
                        '}' | ']' => depth = depth.saturating_sub(1),
                        _ => {}
                    }
                    key_ok = matches!(c, '{' | ',');
                    1
                }
                c if c.is_whitespace() => {
                    let len = rest
                        .find(|c: char| !c.is_whitespace())
                        .unwrap_or(rest.len());
                    out.text(&rest[..len]);
                    if rest[..len].contains('\n') {
                        key_ok = true;
                    }
                    len
                }
                _ => {
                    let len = rest
                        .find(|c: char| c.is_whitespace() || "#,={}[]\"'".contains(c))
                        .unwrap_or(rest.len());
                    let word = &rest[..len.max(1)];
                    if key_ok && rest[word.len()..].trim_start().starts_with('=') {
                        out.span("k", word);
                    } else if word == "true"
                        || word == "false"
                        || word.starts_with(|c: char| c.is_ascii_digit() || c == '-' || c == '+')
                    {
                        out.span("n", word);
                    } else {
                        out.text(word);
                    }
                    key_ok = false;
                    word.len()
                }
            };
            rest = &rest[n..];
        }
    }
    out.0
}

fn sh(code: &str) -> String {
    let mut out = Out(String::new());
    // Whether the next word is a command name.
    let mut cmd = true;
    let mut continued = false;
    for line in code.split_inclusive('\n') {
        if !continued {
            cmd = true;
        }
        continued = line.trim_end_matches('\n').ends_with('\\');
        let mut rest = line;
        let mut at_word_start = true;
        while !rest.is_empty() {
            let c = rest.chars().next().unwrap_or(' ');
            let n = match c {
                '#' if at_word_start => {
                    let end = rest.find('\n').unwrap_or(rest.len());
                    out.span("c", &rest[..end]);
                    end
                }
                '\'' | '"' => {
                    let len = quoted(rest);
                    out.span("s", &rest[..len]);
                    cmd = false;
                    len
                }
                '$' => {
                    let len = if rest[1..].starts_with('{') {
                        rest.find('}').map_or(rest.len(), |i| i + 1)
                    } else {
                        1 + rest[1..]
                            .find(|c: char| !(c.is_alphanumeric() || c == '_'))
                            .unwrap_or(rest.len() - 1)
                    };
                    out.span("v", &rest[..len]);
                    len
                }
                '|' | ';' | '&' | '>' | '<' | '(' | ')' => {
                    let len = rest
                        .find(|ch: char| !"|;&<>()".contains(ch))
                        .unwrap_or(rest.len());
                    out.span("p", &rest[..len]);
                    if !rest[..len].starts_with('>') && !rest[..len].starts_with('<') {
                        cmd = true;
                    }
                    len
                }
                c if c.is_whitespace() => {
                    let len = rest
                        .find(|c: char| !c.is_whitespace())
                        .unwrap_or(rest.len());
                    out.text(&rest[..len]);
                    at_word_start = true;
                    rest = &rest[len..];
                    continue;
                }
                _ => {
                    let len = rest
                        .find(|c: char| c.is_whitespace() || "'\"$|;&<>()".contains(c))
                        .unwrap_or(rest.len());
                    let word = &rest[..len.max(1)];
                    let assign = word.split_once('=').is_some_and(|(name, _)| {
                        !name.is_empty()
                            && name
                                .chars()
                                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
                    });
                    if word.starts_with('-') {
                        // `--input`, or `--input=x`: the flag, then its value.
                        match word.split_once('=') {
                            Some((flag, _)) => {
                                out.span("f", flag);
                                out.text(&word[flag.len()..]);
                            }
                            None => out.span("f", word),
                        }
                    } else if cmd && assign {
                        let (name, _) = word.split_once('=').unwrap_or((word, ""));
                        out.span("v", name);
                        out.text(&word[name.len()..]);
                    } else if cmd && word != "\\" {
                        out.span("x", word);
                        cmd = false;
                    } else {
                        out.text(word);
                    }
                    word.len()
                }
            };
            at_word_start = false;
            rest = &rest[n..];
        }
    }
    out.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toml_tokens() {
        let h = toml(
            "[parts.knob]\nkind = \"encoder\" # a knob\nclk = 22\nknob = { at = [4, 0], keys = [\"Left\"] }\n",
        );
        assert!(h.contains("<span class=\"h\">[parts.knob]</span>"));
        assert!(h.contains("<span class=\"k\">kind</span>"));
        assert!(h.contains("<span class=\"s\">\"encoder\"</span>"));
        assert!(h.contains("<span class=\"c\"># a knob</span>"));
        assert!(h.contains("<span class=\"n\">22</span>"));
        assert!(h.contains("<span class=\"k\">at</span>"));
        assert!(h.contains("<span class=\"k\">keys</span>"));
        assert!(!h.contains("<span class=\"k\">Left"));
        let h = toml("pins = [\n  [1, 2],\n]\n[next]\n");
        assert!(!h.contains("<span class=\"h\">[1"));
        assert!(h.contains("<span class=\"h\">[next]</span>"));
    }

    #[test]
    fn sh_tokens() {
        let h = sh(
            "FOO=1 tartelette fw.elf --board pico | grep x  # note\n    --press a@5 \\\n    --wav out.wav\n",
        );
        assert!(h.contains("<span class=\"v\">FOO</span>=1"));
        assert!(h.contains("<span class=\"x\">tartelette</span>"));
        assert!(h.contains("<span class=\"f\">--board</span>"));
        assert!(h.contains("<span class=\"x\">grep</span>"));
        assert!(h.contains("<span class=\"c\"># note</span>"));
        assert!(!h.contains("<span class=\"x\">fw.elf"));
        assert!(!h.contains("<span class=\"x\">out.wav"));
    }
}
