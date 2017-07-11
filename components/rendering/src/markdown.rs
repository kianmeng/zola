use std::borrow::Cow::Owned;

use pulldown_cmark as cmark;
use self::cmark::{Parser, Event, Tag, Options, OPTION_ENABLE_TABLES, OPTION_ENABLE_FOOTNOTES};
use slug::slugify;
use syntect::easy::HighlightLines;
use syntect::html::{start_coloured_html_snippet, styles_to_coloured_html, IncludeBackground};
use tera::{Context as TeraContext};

use errors::Result;
use utils::site::resolve_internal_link;
use front_matter::InsertAnchor;
use context::Context;
use highlighting::{SYNTAX_SET, THEME_SET};
use short_code::{SHORTCODE_RE, ShortCode, parse_shortcode, render_simple_shortcode};
use table_of_contents::{TempHeader, Header, make_table_of_contents};


pub fn markdown_to_html(content: &str, context: &Context) -> Result<(String, Vec<Header>)> {
    // We try to be smart about highlighting code as it can be time-consuming
    // If the global config disables it, then we do nothing. However,
    // if we see a code block in the content, we assume that this page needs
    // to be highlighted. It could potentially have false positive if the content
    // has ``` in it but that seems kind of unlikely
    let should_highlight = if context.highlight_code {
        content.contains("```")
    } else {
        false
    };
    // Set while parsing
    let mut error = None;
    let mut highlighter: Option<HighlightLines> = None;
    let mut shortcode_block = None;
    // shortcodes live outside of paragraph so we need to ensure we don't close
    // a paragraph that has already been closed
    let mut added_shortcode = false;
    // Don't transform things that look like shortcodes in code blocks
    let mut in_code_block = false;
    // If we get text in header, we need to insert the id and a anchor
    let mut in_header = false;
    // pulldown_cmark can send several text events for a title if there are markdown
    // specific characters like `!` in them. We only want to insert the anchor the first time
    let mut header_already_inserted = false;
    let mut anchors: Vec<String> = vec![];

    // the rendered html
    let mut html = String::new();

    // We might have cases where the slug is already present in our list of anchor
    // for example an article could have several titles named Example
    // We add a counter after the slug if the slug is already present, which
    // means we will have example, example-1, example-2 etc
    fn find_anchor(anchors: &[String], name: String, level: u8) -> String {
        if level == 0 && !anchors.contains(&name) {
            return name.to_string();
        }

        let new_anchor = format!("{}-{}", name, level + 1);
        if !anchors.contains(&new_anchor) {
            return new_anchor;
        }

        find_anchor(anchors, name, level + 1)
    }

    let mut headers = vec![];
    // Defaults to a 0 level so not a real header
    // It should be an Option ideally but not worth the hassle to update
    let mut temp_header = TempHeader::default();

    let mut opts = Options::empty();
    opts.insert(OPTION_ENABLE_TABLES);
    opts.insert(OPTION_ENABLE_FOOTNOTES);

    {
        let parser = Parser::new_ext(content, opts).map(|event| match event {
            Event::Text(text) => {
                // if we are in the middle of a code block
                if let Some(ref mut highlighter) = highlighter {
                    let highlighted = &highlighter.highlight(&text);
                    let html = styles_to_coloured_html(highlighted, IncludeBackground::Yes);
                    return Event::Html(Owned(html));
                }

                if in_code_block {
                    return Event::Text(text);
                }

                // Shortcode without body
                if shortcode_block.is_none() && text.starts_with("{{") && text.ends_with("}}") && SHORTCODE_RE.is_match(&text) {
                    let (name, args) = parse_shortcode(&text);
                    added_shortcode = true;
                    match render_simple_shortcode(context.tera, &name, &args) {
                        Ok(s) => return Event::Html(Owned(format!("</p>{}", s))),
                        Err(e) => {
                            error = Some(e);
                            return Event::Html(Owned("".to_string()));
                        }
                    }
                    // non-matching will be returned normally below
                }

                // Shortcode with a body
                if shortcode_block.is_none() && text.starts_with("{%") && text.ends_with("%}") {
                    if SHORTCODE_RE.is_match(&text) {
                        let (name, args) = parse_shortcode(&text);
                        shortcode_block = Some(ShortCode::new(&name, args));
                    }
                    // Don't return anything
                    return Event::Text(Owned("".to_string()));
                }

                // If we have some text while in a shortcode, it's either the body
                // or the end tag
                if shortcode_block.is_some() {
                    if let Some(ref mut shortcode) = shortcode_block {
                        if text.trim() == "{% end %}" {
                            added_shortcode = true;
                            match shortcode.render(context.tera) {
                                Ok(s) => return Event::Html(Owned(format!("</p>{}", s))),
                                Err(e) => {
                                    error = Some(e);
                                    return Event::Html(Owned("".to_string()));
                                }
                            }
                        } else {
                            shortcode.append(&text);
                            return Event::Html(Owned("".to_string()));
                        }
                    }
                }

                if in_header {
                    if header_already_inserted {
                        return Event::Text(text);
                    }
                    let id = find_anchor(&anchors, slugify(&text), 0);
                    anchors.push(id.clone());
                    let anchor_link = if context.should_insert_anchor() {
                        let mut c = TeraContext::new();
                        c.add("id", &id);
                        context.tera.render("anchor-link.html", &c).unwrap()
                    } else {
                        String::new()
                    };
                    // update the header and add it to the list
                    temp_header.id = id.clone();
                    temp_header.title = text.clone().into_owned();
                    temp_header.permalink = format!("{}#{}", context.current_page_permalink, id);
                    headers.push(temp_header.clone());
                    temp_header = TempHeader::default();

                    header_already_inserted = true;
                    let event = match context.insert_anchor {
                        InsertAnchor::Left => Event::Html(Owned(format!(r#"id="{}">{}{}"#, id, anchor_link, text))),
                        InsertAnchor::Right => Event::Html(Owned(format!(r#"id="{}">{}{}"#, id, text, anchor_link))),
                        InsertAnchor::None => Event::Html(Owned(format!(r#"id="{}">{}"#, id, text)))
                    };
                    return event;
                }

                // Business as usual
                Event::Text(text)
            },
            Event::Start(Tag::CodeBlock(ref info)) => {
                in_code_block = true;
                if !should_highlight {
                    return Event::Html(Owned("<pre><code>".to_owned()));
                }
                let theme = &THEME_SET.themes[&context.highlight_theme];
                highlighter = SYNTAX_SET.with(|ss| {
                    let syntax = info
                        .split(' ')
                        .next()
                        .and_then(|lang| ss.find_syntax_by_token(lang))
                        .unwrap_or_else(|| ss.find_syntax_plain_text());
                    Some(HighlightLines::new(syntax, theme))
                });
                let snippet = start_coloured_html_snippet(theme);
                Event::Html(Owned(snippet))
            },
            Event::End(Tag::CodeBlock(_)) => {
                in_code_block = false;
                if !should_highlight{
                    return Event::Html(Owned("</code></pre>\n".to_owned()))
                }
                // reset highlight and close the code block
                highlighter = None;
                Event::Html(Owned("</pre>".to_owned()))
            },
            // Need to handle relative links
            Event::Start(Tag::Link(ref link, ref title)) => {
                if in_header {
                    return Event::Html(Owned("".to_owned()));
                }
                if link.starts_with("./") {
                    match resolve_internal_link(link, context.permalinks) {
                        Ok(url) => {
                            return Event::Start(Tag::Link(Owned(url), title.clone()));
                        },
                        Err(_) => {
                            error = Some(format!("Relative link {} not found.", link).into());
                            return Event::Html(Owned("".to_string()));
                        }
                    };
                }

                Event::Start(Tag::Link(link.clone(), title.clone()))
            },
            Event::End(Tag::Link(_, _)) => {
                if in_header {
                    return Event::Html(Owned("".to_owned()));
                }
                event
            }
            // need to know when we are in a code block to disable shortcodes in them
            Event::Start(Tag::Code) => {
                in_code_block = true;
                event
            },
            Event::End(Tag::Code) => {
                in_code_block = false;
                event
            },
            Event::Start(Tag::Header(num)) => {
                in_header = true;
                temp_header = TempHeader::new(num);
                // ugly eh
                Event::Html(Owned(format!("<h{} ", num)))
            },
            Event::End(Tag::Header(_)) => {
                in_header = false;
                header_already_inserted = false;
                event
            },
            // If we added shortcodes, don't close a paragraph since there's none
            Event::End(Tag::Paragraph) => {
                if added_shortcode {
                    added_shortcode = false;
                    return Event::Html(Owned("".to_owned()));
                }
                event
            },
            // Ignore softbreaks inside shortcodes
            Event::SoftBreak => {
                if shortcode_block.is_some() {
                    return Event::Html(Owned("".to_owned()));
                }
                event
            },
             _ => {
                 // println!("event = {:?}", event);
                 event
             },
        });

        cmark::html::push_html(&mut html, parser);
    }

    match error {
        Some(e) => Err(e),
        None => Ok((html.replace("<p></p>", ""), make_table_of_contents(&headers))),
    }
}
