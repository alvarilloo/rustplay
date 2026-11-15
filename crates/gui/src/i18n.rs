use std::collections::HashMap;
use std::fmt::Display;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use windows::Win32::Globalization::{GetUserDefaultUILanguage, LCIDToLocaleName};

// Flat JSON objects whose keys look like "send.settings.image", by ISO 639-1 code.
const SOURCES: [(&str, &str); 2] = [("es", include_str!("../lang/es.json")), ("en", include_str!("../lang/en.json"))];
const FALLBACK: &str = "en";
// Explicit choices in the settings, after "system": index into SOURCES.
const CHOICES: [&str; 2] = ["es", "en"];

static TABLES: OnceLock<Vec<HashMap<String, String>>> = OnceLock::new();
static CURRENT: AtomicUsize = AtomicUsize::new(0);

fn tables() -> &'static [HashMap<String, String>] {
    TABLES.get_or_init(|| {
        SOURCES
            .iter()
            .map(|(code, json)| {
                serde_json::from_str(json).unwrap_or_else(|e| {
                    tracing::error!("archivo de idioma «{code}» no válido: {e}");
                    HashMap::new()
                })
            })
            .collect()
    })
}

fn position(code: &str) -> Option<usize> {
    SOURCES.iter().position(|(c, _)| *c == code)
}

// Windows display language as an ISO 639-1 code, e.g. "es" for es-ES.
fn system_code() -> String {
    let mut name = [0u16; 85];
    let len = unsafe { LCIDToLocaleName(u32::from(GetUserDefaultUILanguage()), Some(&mut name), 0) };
    let name = String::from_utf16_lossy(&name[..(len.max(1) as usize - 1).min(name.len())]);
    name.split('-').next().unwrap_or_default().to_ascii_lowercase()
}

// Picks the language for `preference` (0 follows Windows); a system language without
// its own JSON file falls back to English.
pub fn apply(preference: usize) {
    let code = match preference.checked_sub(1).and_then(|i| CHOICES.get(i)) {
        Some(code) => (*code).to_owned(),
        None => system_code(),
    };
    let index = position(&code).or_else(|| position(FALLBACK)).unwrap_or(0);
    CURRENT.store(index, Ordering::Release);
}

// ISO code of the language in use ("es" or "en").
pub fn code() -> &'static str {
    SOURCES[CURRENT.load(Ordering::Acquire)].0
}

// Looks the key up in the current language; a missing translation shows the key itself.
pub fn t(key: &'static str) -> &'static str {
    tables()[CURRENT.load(Ordering::Acquire)].get(key).map_or(key, String::as_str)
}

// Like `t`, replacing each `{name}` placeholder with its argument.
pub fn tf(key: &'static str, args: &[(&str, &dyn Display)]) -> String {
    let mut text = t(key).to_owned();
    for (name, value) in args {
        text = text.replace(&format!("{{{name}}}"), &value.to_string());
    }
    text
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn both_languages_define_the_same_keys() {
        let es: BTreeSet<_> = tables()[position("es").unwrap()].keys().collect();
        let en: BTreeSet<_> = tables()[position("en").unwrap()].keys().collect();
        assert!(!es.is_empty());
        assert_eq!(es.difference(&en).collect::<Vec<_>>(), Vec::<&&String>::new(), "faltan en en.json");
        assert_eq!(en.difference(&es).collect::<Vec<_>>(), Vec::<&&String>::new(), "faltan en es.json");
    }

    #[test]
    fn unknown_system_language_uses_english() {
        assert_eq!(position("de").or_else(|| position(FALLBACK)), position("en"));
    }

    #[test]
    fn missing_keys_fall_back_to_the_key() {
        assert_eq!(t("no.such.key"), "no.such.key");
    }

    #[test]
    fn placeholders_are_replaced() {
        CURRENT.store(position("es").unwrap(), Ordering::Release);
        assert_eq!(tf("status.mirroring", &[("name", &"TV")]), "Duplicando en «TV»");
    }
}
