// SPDX-License-Identifier: GPL-3.0-or-later
//! Minimal parser for Valve's text KeyValues format (.vdf / .acf).

use std::collections::BTreeMap;

#[derive(Debug, Clone)]
pub enum Value {
    Str(String),
    Map(Map),
}

pub type Map = BTreeMap<String, Value>;

impl Value {
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Map(map) => map.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v),
            Value::Str(_) => None,
        }
    }

    pub fn str(&self, key: &str) -> Option<&str> {
        match self.get(key) {
            Some(Value::Str(s)) => Some(s),
            _ => None,
        }
    }

    pub fn map(&self) -> Option<&Map> {
        match self {
            Value::Map(map) => Some(map),
            Value::Str(_) => None,
        }
    }
}

fn tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(&c) = chars.peek() {
        match c {
            '"' => {
                chars.next();
                let mut s = String::new();
                while let Some(c) = chars.next() {
                    match c {
                        '\\' => {
                            if let Some(n) = chars.next() {
                                s.push(match n {
                                    'n' => '\n',
                                    't' => '\t',
                                    other => other,
                                });
                            }
                        }
                        '"' => break,
                        other => s.push(other),
                    }
                }
                out.push(s);
            }
            '{' | '}' => {
                chars.next();
                out.push(c.to_string());
            }
            '/' => {
                // Line comment.
                while let Some(c) = chars.next() {
                    if c == '\n' {
                        break;
                    }
                }
            }
            c if c.is_whitespace() => {
                chars.next();
            }
            _ => {
                let mut s = String::new();
                while let Some(&c) = chars.peek() {
                    if c.is_whitespace() || c == '{' || c == '}' || c == '"' {
                        break;
                    }
                    s.push(c);
                    chars.next();
                }
                out.push(s);
            }
        }
    }
    out
}

fn parse_map(toks: &[String], pos: &mut usize) -> Map {
    let mut map = Map::new();
    while *pos < toks.len() {
        let key = &toks[*pos];
        if key == "}" {
            *pos += 1;
            break;
        }
        *pos += 1;
        if *pos >= toks.len() {
            break;
        }
        if toks[*pos] == "{" {
            *pos += 1;
            let child = parse_map(toks, pos);
            map.insert(key.clone(), Value::Map(child));
        } else {
            map.insert(key.clone(), Value::Str(toks[*pos].clone()));
            *pos += 1;
        }
    }
    map
}

/// Parse a whole document into its top-level map.
pub fn parse(text: &str) -> Value {
    let toks = tokens(text);
    let mut pos = 0;
    Value::Map(parse_map(&toks, &mut pos))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_manifest() {
        let doc = parse("\"AppState\"\n{\n\t\"appid\"\t\t\"220\"\n\t\"name\"\t\t\"Half-Life 2\"\n\t\"UserConfig\"\n\t{\n\t\t\"language\"\t\t\"english\"\n\t}\n}\n");
        let app = doc.get("AppState").unwrap();
        assert_eq!(app.str("appid"), Some("220"));
        assert_eq!(app.str("name"), Some("Half-Life 2"));
        assert_eq!(app.get("UserConfig").unwrap().str("language"), Some("english"));
    }
}
