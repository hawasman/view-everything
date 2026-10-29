//! Just enough XML to read WebDAV and Azure listings: elements by local name (any namespace
//! prefix), each with the text it held.

use quick_xml::{Reader, events::Event};

/// What `walk` reports, in document order.
pub(crate) enum Ev<'a> {
    Start(&'a str),
    /// The element's name and its trimmed text, entities resolved (empty if it had none).
    End(&'a str, &'a str),
}

pub(crate) fn walk(body: &str, mut f: impl FnMut(Ev)) -> Result<(), String> {
    let mut reader = Reader::from_str(body);
    let mut text = String::new();
    loop {
        match reader.read_event().map_err(|e| e.to_string())? {
            Event::Start(e) => {
                text.clear();
                f(Ev::Start(name(e.local_name().as_ref())?));
            }
            Event::Empty(e) => {
                let local = e.local_name();
                let name = name(local.as_ref())?;
                f(Ev::Start(name));
                f(Ev::End(name, ""));
            }
            Event::End(e) => {
                f(Ev::End(name(e.local_name().as_ref())?, text.trim()));
                text.clear();
            }
            Event::Text(t) => text.push_str(&t.decode().map_err(|e| e.to_string())?),
            Event::CData(c) => text.push_str(&c.decode().map_err(|e| e.to_string())?),
            Event::GeneralRef(r) => {
                if let Some(c) = r.resolve_char_ref().map_err(|e| e.to_string())? {
                    text.push(c);
                } else {
                    match r.decode().map_err(|e| e.to_string())?.as_ref() {
                        "amp" => text.push('&'),
                        "lt" => text.push('<'),
                        "gt" => text.push('>'),
                        "quot" => text.push('"'),
                        "apos" => text.push('\''),
                        other => return Err(format!("unknown entity &{other};")),
                    }
                }
            }
            Event::Eof => return Ok(()),
            _ => {}
        }
    }
}

fn name(bytes: &[u8]) -> Result<&str, String> {
    std::str::from_utf8(bytes).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_lose_their_prefix_and_text_its_entities() {
        let mut seen = Vec::new();
        walk(
            r#"<?xml version="1.0"?><D:a xmlns:D="DAV:"><D:b> x &amp; y &#65; </D:b><c/><![CDATA[<z>]]></D:a>"#,
            |e| match e {
                Ev::Start(n) => seen.push(format!("<{n}")),
                Ev::End(n, t) => seen.push(format!("{n}={t}")),
            },
        )
        .unwrap();
        assert_eq!(seen, ["<a", "<b", "b=x & y A", "<c", "c=", "a=<z>"]);
        assert!(walk("<a><b></a>", |_| {}).is_err());
    }
}
