//! Markdown in card descriptions and comments, as HTML that is safe to put in the page.
//!
//! The text comes from anyone on the team (or an agent), so nothing in it may run or load:
//! raw HTML is shown as text, links go only to `http`, `https` and `mailto` addresses and open in
//! a new tab, and images are shown as links rather than fetched, so opening a card never tells a
//! third party who looked and when. Pure Rust, so it is tested on the host.

use pulldown_cmark::{CowStr, Event, LinkType, Options, Parser, Tag, TagEnd, html};

/// `text` as Markdown (GitHub flavoured: tables, strikethrough, task lists), rendered to HTML.
/// A single line break is kept, as people type it in a comment box.
pub fn to_html(text: &str) -> String {
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut events = Vec::new();
    // Each open link or image: whether it became an anchor, so its end closes only what opened.
    let mut open: Vec<bool> = Vec::new();
    let mut in_code_block = false;
    // The parser splits text at characters that might have been emphasis (`_` in an address);
    // joined again, an address is seen whole.
    let mut parsed: Vec<Event<'_>> = Vec::new();
    for event in Parser::new_ext(text, options) {
        match (parsed.last_mut(), event) {
            (Some(Event::Text(prev)), Event::Text(next)) => {
                *prev = CowStr::from(format!("{prev}{next}"));
            }
            (_, event) => parsed.push(event),
        }
    }
    let mut i = 0;
    while i < parsed.len() {
        match parsed[i].clone() {
            Event::Html(raw) | Event::InlineHtml(raw) => events.push(Event::Text(raw)),
            Event::SoftBreak => events.push(Event::HardBreak),
            Event::Start(Tag::CodeBlock(kind)) => {
                in_code_block = true;
                events.push(Event::Start(Tag::CodeBlock(kind)));
            }
            Event::End(TagEnd::CodeBlock) => {
                in_code_block = false;
                events.push(Event::End(TagEnd::CodeBlock));
            }
            Event::Start(Tag::Link { link_type, dest_url, .. }) => {
                // `<a@example.com>` comes without its scheme, which the parser adds only when it
                // writes the HTML itself.
                let mail = (link_type == LinkType::Email).then(|| format!("mailto:{dest_url}"));
                let safe = safe_url(mail.as_deref().unwrap_or(&dest_url));
                if let Some(url) = safe {
                    events.push(Event::Html(anchor(url).into()));
                }
                open.push(safe.is_some());
            }
            Event::Start(Tag::Image { dest_url, .. }) => {
                let safe = safe_url(&dest_url);
                if let Some(url) = safe {
                    events.push(Event::Html(anchor(url).into()));
                }
                open.push(safe.is_some());
                // An image with no description still needs something to click.
                if matches!(parsed.get(i + 1), Some(Event::End(TagEnd::Image))) {
                    events.push(Event::Text(CowStr::from(format!("image: {dest_url}"))));
                }
            }
            Event::End(TagEnd::Link | TagEnd::Image) => {
                if open.pop() == Some(true) {
                    events.push(Event::Html("</a>".into()));
                }
            }
            Event::Text(t) if open.is_empty() && !in_code_block => linkify(&t, &mut events),
            other => events.push(other),
        }
        i += 1;
    }
    let mut out = String::with_capacity(text.len() * 3 / 2);
    html::push_html(&mut out, events.into_iter());
    out
}

/// The address itself when it may be followed from the board.
fn safe_url(url: &str) -> Option<&str> {
    let url = url.trim();
    let lower = url.get(..8).unwrap_or(url).to_ascii_lowercase();
    ["http://", "https://", "mailto:"].iter().any(|s| lower.starts_with(s)).then_some(url)
}

fn anchor(url: &str) -> String {
    format!(r#"<a href="{}" target="_blank" rel="noopener noreferrer nofollow">"#, escape_attr(url))
}

fn escape_attr(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Text with every bare `http(s)://` address in it made a link, as GitHub does. One pass over
/// the text, so a comment full of addresses cannot stall the board.
fn linkify<'a>(text: &CowStr<'a>, events: &mut Vec<Event<'a>>) {
    let text: &str = text;
    // ASCII lowercasing keeps every byte offset the same as in `text`.
    let lower = text.to_ascii_lowercase();
    let mut written = 0;
    let mut from = 0;
    while let Some(found) = lower[from..].find("http") {
        let start = from + found;
        let scheme = ["https://", "http://"].into_iter().find(|s| lower[start..].starts_with(s));
        let Some(scheme) = scheme else {
            from = start + "http".len();
            continue;
        };
        let rest = &text[start..];
        let end = rest
            .find(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '"' | '`'))
            .unwrap_or(rest.len());
        let url = trim_url(&rest[..end]);
        if url.len() <= scheme.len() {
            // A scheme with no address after it stays text.
            from = start + scheme.len();
            continue;
        }
        if start > written {
            events.push(Event::Text(CowStr::from(text[written..start].to_owned())));
        }
        events.push(Event::Html(anchor(url).into()));
        events.push(Event::Text(CowStr::from(url.to_owned())));
        events.push(Event::Html("</a>".into()));
        written = start + url.len();
        from = written;
    }
    if written < text.len() {
        events.push(Event::Text(CowStr::from(text[written..].to_owned())));
    }
}

/// Drops punctuation that ends the sentence rather than the address, and a closing bracket the
/// address did not open.
fn trim_url(url: &str) -> &str {
    // Counted once, so a long tail of brackets costs one pass.
    let mut opens = url.matches('(').count();
    let mut closes = url.matches(')').count();
    let mut url = url;
    loop {
        let Some(last) = url.chars().last() else { return url };
        let unbalanced = last == ')' && opens < closes;
        if matches!(last, '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '*' | '_') || unbalanced {
            if last == ')' {
                closes -= 1;
            } else if last == '(' {
                opens -= 1;
            }
            url = &url[..url.len() - last.len_utf8()];
        } else {
            return url;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::to_html;

    #[test]
    fn renders_the_markdown_people_write_in_cards() {
        let html = to_html(
            "## Goal\nUse **less** memory and _keep_ `cpu-throttling` on.\n\n- one\n- [x] done",
        );
        assert!(html.contains("<h2>Goal</h2>"), "{html}");
        assert!(html.contains("<strong>less</strong>"), "{html}");
        assert!(html.contains("<em>keep</em>"), "{html}");
        assert!(html.contains("<code>cpu-throttling</code>"), "{html}");
        assert!(html.contains("<li>one</li>"), "{html}");
        assert!(html.contains(r#"type="checkbox""#) && html.contains("checked"), "{html}");
        let table = to_html("| a | b |\n|---|---|\n| 1 | 2 |");
        assert!(table.contains("<table>") && table.contains("<td>2</td>"), "{table}");
        assert!(to_html("~~gone~~").contains("<del>gone</del>"));
    }

    #[test]
    fn keeps_a_single_line_break_as_typed() {
        assert_eq!(to_html("first line\nsecond line"), "<p>first line<br />\nsecond line</p>\n");
    }

    #[test]
    fn shows_raw_html_as_text() {
        let html =
            to_html("<script>alert(1)</script>\n\nhi <img src=x onerror=alert(1)> <!-- c -->");
        assert!(!html.contains("<script"), "{html}");
        assert!(!html.contains("<img"), "{html}");
        assert!(!html.contains("<!--"), "{html}");
        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"), "{html}");
    }

    #[test]
    fn links_only_to_web_and_mail_addresses_in_a_new_tab() {
        let html = to_html("[site](https://example.com/a?b=1&c=2) [mail](mailto:a@example.com)");
        assert!(
            html.contains(r#"<a href="https://example.com/a?b=1&amp;c=2" target="_blank" rel="noopener noreferrer nofollow">site</a>"#),
            "{html}"
        );
        assert!(html.contains(r#"href="mailto:a@example.com""#), "{html}");
        for bad in [
            "javascript:alert(1)",
            "JaVaScRiPt:alert(1)",
            "data:text/html,x",
            "vbscript:x",
            "/local",
            "file:///etc/passwd",
        ] {
            let html = to_html(&format!("[click]({bad})"));
            assert!(!html.contains("<a"), "{bad}: {html}");
            assert!(html.contains("click"), "{bad}: {html}");
        }
        // A quote cannot end the attribute early.
        let html = to_html(r#"[x](<https://example.com/" onmouseover="alert(1)>)"#);
        assert!(!html.contains(r#"" onmouseover"#), "{html}");
    }

    #[test]
    fn never_loads_an_image() {
        let html = to_html(
            "![diagram](https://example.com/d.png) ![](https://example.com/e.png) ![x](javascript:1)",
        );
        assert!(!html.contains("<img"), "{html}");
        assert!(
            html.contains(r#"href="https://example.com/d.png""#) && html.contains(">diagram</a>"),
            "{html}"
        );
        assert!(html.contains(">image: https://example.com/e.png</a>"), "{html}");
        assert!(!html.contains("javascript"), "{html}");
    }

    #[test]
    fn makes_bare_addresses_links_but_not_in_code() {
        let html = to_html("See https://example.com/t/abc1 (status: ready).");
        assert!(
            html.contains(r#"See <a href="https://example.com/t/abc1" target="_blank" rel="noopener noreferrer nofollow">https://example.com/t/abc1</a> (status: ready)."#),
            "{html}"
        );
        let wiki = to_html("https://en.wikipedia.org/wiki/Rust_(programming_language), next");
        assert!(
            wiki.contains(">https://en.wikipedia.org/wiki/Rust_(programming_language)</a>, next"),
            "{wiki}"
        );
        let code = to_html("`https://example.com` and\n\n```\nhttps://example.com/x\n```");
        assert!(!code.contains("<a"), "{code}");
        let linked = to_html("[https://example.com](https://example.com)");
        assert_eq!(linked.matches("<a ").count(), 1, "{linked}");
        assert!(!to_html("just https:// alone").contains("<a"));
    }

    #[test]
    fn escapes_every_text_it_emits() {
        // Markup characters reach the page only as entities, wherever the text sits.
        let html = to_html(concat!(
            "# <b>head</b>\n\n",
            "`<b>span</b>` and <b>inline</b>\n\n",
            "```\n<b>block</b>\n```\n\n",
            "| <b>cell</b> |\n|---|\n| <i>x</i> |\n\n",
            "- [ ] <b>task</b>\n\n",
            "> <b>quote</b>\n\n",
            "[<b>link text</b>](https://example.com/a)\n\n",
            "![<b>alt</b>](https://example.com/i.png)\n\n",
            "see https://example.com/?a=1&b=2'x\n\n",
            "&lt;b&gt; stays &amp;lt;b&amp;gt;",
        ));
        for tag in ["<b>", "</b>", "<i>"] {
            assert!(!html.contains(tag), "{tag} in {html}");
        }
        assert!(html.contains("&lt;b&gt;head&lt;/b&gt;"), "{html}");
        assert!(html.contains("<code>&lt;b&gt;span&lt;/b&gt;</code>"), "{html}");
        assert!(html.contains("&lt;b&gt;block&lt;/b&gt;"), "{html}");
        assert!(
            html.contains(">https://example.com/?a=1&amp;b=2&#39;x</a>")
                || html.contains(">https://example.com/?a=1&amp;b=2'x</a>"),
            "{html}"
        );
        assert!(html.contains(r#"href="https://example.com/?a=1&amp;b=2&#39;x""#), "{html}");
        // An entity typed as text stays text: it is not decoded into a tag.
        assert!(!html.contains("<b> stays"), "{html}");
    }

    #[test]
    fn links_mail_autolinks_and_short_addresses() {
        let mail = to_html("<a@example.com>");
        assert!(mail.contains(r#"href="mailto:a@example.com""#), "{mail}");
        assert!(to_html("go to http://a now").contains(r#"href="http://a""#));
        assert!(!to_html("httpx://a and http:/a").contains("<a"));
    }

    #[test]
    fn renders_a_megabyte_of_addresses_in_linear_time() {
        // Every address used to rescan the rest of the text: 1 MB took over a minute.
        let text = "http:// https://example.com/x ".repeat(35_000);
        let started = std::time::Instant::now();
        let html = to_html(&text);
        assert!(html.matches("<a ").count() == 35_000, "every address linked");
        assert!(started.elapsed() < std::time::Duration::from_secs(10), "{:?}", started.elapsed());
        // A long tail of closing brackets used to be recounted for each one trimmed.
        let tail = format!("http://a{}", ")".repeat(500_000));
        let started = std::time::Instant::now();
        assert!(to_html(&tail).contains(r#"href="http://a""#));
        assert!(started.elapsed() < std::time::Duration::from_secs(10), "{:?}", started.elapsed());
    }

    #[test]
    fn escapes_text() {
        assert_eq!(to_html("a < b & c > d"), "<p>a &lt; b &amp; c &gt; d</p>\n");
    }
}
