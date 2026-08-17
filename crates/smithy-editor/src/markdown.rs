//! A small markdown subset for agent answers.
//!
//! The panel used to dump the model into a plain label, so `**this**` and
//! fenced code rendered as the markers themselves. This parser is the piece
//! that turns those into blocks the view can paint: headings, paragraphs,
//! lists, fenced code, and a handful of inlines (`**bold**`, `*italic*`,
//! `` `code` ``).
//!
//! It is not CommonMark. Tables are kept as a mono block so pipe-rows stay
//! aligned instead of being flattened into a paragraph. Unclosed fences run
//! to the end of the string, which is what you want while a fence is still
//! streaming in.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    Heading {
        level: u8,
        inlines: Vec<Inline>,
    },
    Paragraph {
        inlines: Vec<Inline>,
    },
    List {
        ordered: bool,
        items: Vec<Vec<Inline>>,
    },
    Code {
        lang: String,
        code: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inline {
    Text(String),
    Bold(String),
    Italic(String),
    Code(String),
}

pub fn parse(src: &str) -> Vec<Block> {
    let lines: Vec<&str> = src.lines().collect();
    let mut i = 0;
    let mut blocks = Vec::new();
    while i < lines.len() {
        let trimmed = lines[i].trim_start();
        if trimmed.is_empty() {
            i += 1;
            continue;
        }
        if let Some(lang) = fence_lang(trimmed) {
            i += 1;
            let mut code = String::new();
            while i < lines.len() {
                if lines[i].trim_start().starts_with("```") {
                    i += 1;
                    break;
                }
                if !code.is_empty() {
                    code.push('\n');
                }
                code.push_str(lines[i]);
                i += 1;
            }
            blocks.push(Block::Code { lang, code });
            continue;
        }
        if let Some((level, rest)) = heading_line(trimmed) {
            blocks.push(Block::Heading {
                level,
                inlines: parse_inlines(rest),
            });
            i += 1;
            continue;
        }
        if strip_ul(trimmed).is_some() || strip_ol(trimmed).is_some() {
            let ordered = strip_ol(trimmed).is_some();
            let mut items = Vec::new();
            while i < lines.len() {
                let t = lines[i].trim_start();
                let rest = if ordered { strip_ol(t) } else { strip_ul(t) };
                match rest {
                    Some(rest) => {
                        items.push(parse_inlines(rest));
                        i += 1;
                    }
                    None => break,
                }
            }
            if !items.is_empty() {
                blocks.push(Block::List { ordered, items });
            }
            continue;
        }
        if trimmed.starts_with('|') {
            let mut code = String::new();
            while i < lines.len() {
                let t = lines[i].trim_start();
                if !t.starts_with('|') {
                    break;
                }
                if !code.is_empty() {
                    code.push('\n');
                }
                code.push_str(t);
                i += 1;
            }
            blocks.push(Block::Code {
                lang: String::new(),
                code,
            });
            continue;
        }
        let mut para = String::new();
        while i < lines.len() {
            let t = lines[i].trim_start();
            if t.is_empty()
                || fence_lang(t).is_some()
                || heading_line(t).is_some()
                || strip_ul(t).is_some()
                || strip_ol(t).is_some()
                || t.starts_with('|')
            {
                break;
            }
            if !para.is_empty() {
                para.push(' ');
            }
            para.push_str(t);
            i += 1;
        }
        if !para.is_empty() {
            blocks.push(Block::Paragraph {
                inlines: parse_inlines(&para),
            });
        }
    }
    blocks
}

fn fence_lang(line: &str) -> Option<String> {
    let rest = line.strip_prefix("```")?;
    Some(rest.trim().to_string())
}

fn heading_line(line: &str) -> Option<(u8, &str)> {
    let hashes = line.bytes().take_while(|b| *b == b'#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let rest = line.get(hashes..)?.strip_prefix(' ')?;
    Some((hashes as u8, rest))
}

fn strip_ul(line: &str) -> Option<&str> {
    line.strip_prefix("- ")
        .or_else(|| line.strip_prefix("* "))
        .or_else(|| line.strip_prefix("+ "))
}

fn strip_ol(line: &str) -> Option<&str> {
    let digits = line.bytes().take_while(|b| b.is_ascii_digit()).count();
    if digits == 0 {
        return None;
    }
    line.get(digits..)?.strip_prefix(". ")
}

fn parse_inlines(s: &str) -> Vec<Inline> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < s.len() {
        let rest = &s[i..];
        if let Some(body) = rest.strip_prefix('`') {
            if let Some(rel) = body.find('`') {
                out.push(Inline::Code(body[..rel].to_string()));
                i += rel + 2;
                continue;
            }
        }
        if let Some(body) = rest.strip_prefix("**") {
            if let Some(rel) = body.find("**") {
                let inner = &body[..rel];
                if !inner.is_empty() {
                    out.push(Inline::Bold(inner.to_string()));
                    i += rel + 4;
                    continue;
                }
            }
        } else if let Some(body) = rest.strip_prefix('*') {
            if let Some(rel) = body.find('*') {
                let inner = &body[..rel];
                if !inner.is_empty() {
                    out.push(Inline::Italic(inner.to_string()));
                    i += rel + 2;
                    continue;
                }
            }
        }
        let take = rest
            .char_indices()
            .find_map(|(k, c)| {
                if k == 0 {
                    None
                } else if c == '`' || c == '*' {
                    Some(k)
                } else {
                    None
                }
            })
            .unwrap_or(rest.len());
        let take = if take == 0 {
            rest.chars().next().map(|c| c.len_utf8()).unwrap_or(0)
        } else {
            take
        };
        if take == 0 {
            break;
        }
        push_text(&mut out, &rest[..take]);
        i += take;
    }
    out
}

fn push_text(out: &mut Vec<Inline>, s: &str) {
    if s.is_empty() {
        return;
    }
    if let Some(Inline::Text(existing)) = out.last_mut() {
        existing.push_str(s);
    } else {
        out.push(Inline::Text(s.to_string()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bold_and_inline_code() {
        assert_eq!(
            parse("**Practical upshot:** use `read`."),
            vec![Block::Paragraph {
                inlines: vec![
                    Inline::Bold("Practical upshot:".into()),
                    Inline::Text(" use ".into()),
                    Inline::Code("read".into()),
                    Inline::Text(".".into()),
                ]
            }]
        );
    }

    #[test]
    fn fenced_code_keeps_the_body_and_lang() {
        let src = "intro\n```bash\necho hi\n```\nout";
        assert_eq!(
            parse(src),
            vec![
                Block::Paragraph {
                    inlines: vec![Inline::Text("intro".into())]
                },
                Block::Code {
                    lang: "bash".into(),
                    code: "echo hi".into(),
                },
                Block::Paragraph {
                    inlines: vec![Inline::Text("out".into())]
                },
            ]
        );
    }

    #[test]
    fn an_unclosed_fence_runs_to_eof() {
        assert_eq!(
            parse("```rust\nfn main() {}"),
            vec![Block::Code {
                lang: "rust".into(),
                code: "fn main() {}".into(),
            }]
        );
    }

    #[test]
    fn headings_and_lists() {
        let src = "# Title\n\n- one\n- two\n\n1. first\n2. second";
        let blocks = parse(src);
        assert_eq!(
            blocks[0],
            Block::Heading {
                level: 1,
                inlines: vec![Inline::Text("Title".into())]
            }
        );
        assert_eq!(
            blocks[1],
            Block::List {
                ordered: false,
                items: vec![
                    vec![Inline::Text("one".into())],
                    vec![Inline::Text("two".into())],
                ]
            }
        );
        assert_eq!(
            blocks[2],
            Block::List {
                ordered: true,
                items: vec![
                    vec![Inline::Text("first".into())],
                    vec![Inline::Text("second".into())],
                ]
            }
        );
    }

    #[test]
    fn a_star_list_is_not_italic() {
        assert_eq!(
            parse("* item"),
            vec![Block::List {
                ordered: false,
                items: vec![vec![Inline::Text("item".into())]]
            }]
        );
    }

    #[test]
    fn italic_inside_a_paragraph() {
        assert_eq!(
            parse("a *b* c"),
            vec![Block::Paragraph {
                inlines: vec![
                    Inline::Text("a ".into()),
                    Inline::Italic("b".into()),
                    Inline::Text(" c".into()),
                ]
            }]
        );
    }

    #[test]
    fn unmatched_markers_stay_literal() {
        assert_eq!(
            parse("2 * 3 = 6"),
            vec![Block::Paragraph {
                inlines: vec![Inline::Text("2 * 3 = 6".into())]
            }]
        );
        assert_eq!(
            parse("**oops"),
            vec![Block::Paragraph {
                inlines: vec![Inline::Text("**oops".into())]
            }]
        );
    }

    #[test]
    fn pipe_rows_stay_together_as_a_block() {
        let src = "| a | b |\n| --- | --- |\n| 1 | 2 |";
        assert_eq!(
            parse(src),
            vec![Block::Code {
                lang: String::new(),
                code: src.to_string(),
            }]
        );
    }

    #[test]
    fn backticks_win_over_stars() {
        assert_eq!(
            parse("use `**not bold**` please"),
            vec![Block::Paragraph {
                inlines: vec![
                    Inline::Text("use ".into()),
                    Inline::Code("**not bold**".into()),
                    Inline::Text(" please".into()),
                ]
            }]
        );
    }
}
