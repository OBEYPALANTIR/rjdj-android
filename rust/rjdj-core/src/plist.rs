//! The flattened, last-wins SAX metadata handler from SceneArchive.java.

use crate::error::{CoreError, Result};
use quick_xml::escape::resolve_xml_entity;
use quick_xml::events::Event;
use quick_xml::name::ResolveResult;
use quick_xml::{NsReader, XmlVersion};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

pub const MAX_INFO_BYTES: usize = 1024 * 1024;

/// Reads XML plist scalar pairs, including pairs nested inside dictionaries.
pub fn read_info(scene_dir: &Path) -> Result<Vec<(String, String)>> {
    let info = scene_dir.join("Info.plist");
    let file = File::open(&info).map_err(|error| {
        let reason = match error.kind() {
            io::ErrorKind::NotFound => "No such file or directory",
            io::ErrorKind::PermissionDenied => "Permission denied",
            io::ErrorKind::IsADirectory => "Is a directory",
            _ => return CoreError::from(error),
        };
        CoreError::new(format!("{} ({reason})", info.display()))
    })?;
    let mut bytes = Vec::new();
    file.take((MAX_INFO_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_INFO_BYTES {
        return Err(CoreError::new("Scene metadata is too large"));
    }
    // Java's UTF-8 String constructor replaces malformed byte sequences.
    let xml = String::from_utf8_lossy(&bytes);
    if xml.contains('\0') || xml.contains("<!ENTITY") {
        return Err(unsupported());
    }
    parse(&xml)
}

fn invalid() -> CoreError {
    CoreError::new("Invalid scene metadata")
}
fn unsupported() -> CoreError {
    CoreError::new("Unsupported scene metadata entities or encoding")
}
fn java_trim(text: &str) -> &str {
    text.trim_matches(|c| c <= '\u{20}')
}

fn parse(xml: &str) -> Result<Vec<(String, String)>> {
    let mut reader = NsReader::from_str(xml);
    reader.config_mut().expand_empty_elements = true;
    reader.config_mut().check_comments = true;
    let mut values = BTreeMap::new();
    let mut key = None;
    let mut text = String::new();
    let mut depth = 0usize;
    let mut root_seen = false;
    let mut doctype_seen = false;
    let mut declaration_seen = false;
    let mut version = XmlVersion::Implicit1_0;
    loop {
        let (namespace, event) = reader.read_resolved_event().map_err(|_| invalid())?;
        if matches!(namespace, ResolveResult::Unknown(_)) {
            return Err(invalid());
        }
        match event {
            Event::Start(element) => {
                if depth == 0 {
                    if root_seen {
                        return Err(invalid());
                    }
                    root_seen = true;
                }
                for attribute in element.attributes() {
                    let attribute = attribute.map_err(|_| invalid())?;
                    attribute.normalized_value(version).map_err(|_| invalid())?;
                }
                depth += 1;
                text.clear();
                if !matches!(element.local_name().as_ref(), "key" | "string" | "integer") {
                    key = None;
                }
            }
            Event::End(element) => {
                if depth == 0 {
                    return Err(invalid());
                }
                depth -= 1;
                match element.local_name().as_ref() {
                    "key" => key = Some(java_trim(&text).to_owned()),
                    "string" | "integer" => {
                        if let Some(key) = key.take() {
                            values.insert(key, java_trim(&text).to_owned());
                        }
                    }
                    _ => (),
                }
                text.clear();
            }
            Event::Text(content) => {
                let content = content.xml_content(version);
                if depth == 0 && !content.chars().all(xml_space) {
                    return Err(invalid());
                }
                if !content.chars().all(xml_character) {
                    return Err(invalid());
                }
                text.push_str(&content);
            }
            Event::CData(content) => {
                if depth == 0 {
                    return Err(invalid());
                }
                let content = content.xml_content(version);
                if !content.chars().all(xml_character) {
                    return Err(invalid());
                }
                text.push_str(&content);
            }
            Event::GeneralRef(reference) => {
                if depth == 0 {
                    return Err(invalid());
                }
                if let Some(character) = reference.resolve_char_ref().map_err(|_| invalid())? {
                    if !xml_character(character) {
                        return Err(invalid());
                    }
                    text.push(character);
                } else {
                    text.push_str(resolve_xml_entity(&reference).ok_or_else(invalid)?);
                }
            }
            Event::DocType(_) => {
                if root_seen || doctype_seen {
                    return Err(invalid());
                }
                doctype_seen = true;
                // External DTDs are inert, as with Java's empty EntityResolver.
            }
            Event::Decl(declaration) => {
                if root_seen || doctype_seen || declaration_seen {
                    return Err(invalid());
                }
                declaration_seen = true;
                version = declaration.xml_version().map_err(|_| invalid())?;
            }
            Event::Eof => {
                if depth != 0 || !root_seen {
                    return Err(invalid());
                }
                break;
            }
            Event::Comment(_) | Event::PI(_) => (),
            Event::Empty(_) => return Err(invalid()),
        }
    }
    if values.get("name").is_none_or(String::is_empty) {
        return Err(CoreError::new("Scene metadata is missing its name"));
    }
    values
        .entry("author".to_owned())
        .or_insert_with(|| "Unknown artist".to_owned());
    values.entry("description".to_owned()).or_default();
    Ok(values.into_iter().collect())
}

fn xml_space(character: char) -> bool {
    matches!(character, ' ' | '\t' | '\r' | '\n')
}
fn xml_character(character: char) -> bool {
    matches!(character, '\t' | '\r' | '\n' | '\u{20}'..='\u{d7ff}' | '\u{e000}'..='\u{fffd}' | '\u{10000}'..='\u{10ffff}')
}
