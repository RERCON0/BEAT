//! Small built-in EN/RU catalog; no runtime files or network are required.
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    #[default]
    En,
    Ru,
}

static RUSSIAN: AtomicBool = AtomicBool::new(cfg!(test));

include!("i18n_catalog.rs");

pub fn tr(text: &str) -> &str {
    translate(current(), text)
}

pub fn set(language: Language) {
    RUSSIAN.store(language == Language::Ru, Ordering::Relaxed);
}

pub fn current() -> Language {
    if RUSSIAN.load(Ordering::Relaxed) {
        Language::Ru
    } else {
        Language::En
    }
}

impl Language {
    pub fn toggle(self) -> Self {
        match self {
            Self::En => Self::Ru,
            Self::Ru => Self::En,
        }
    }
    pub fn switch_label(self) -> &'static str {
        match self {
            Self::En => "RU",
            Self::Ru => "EN",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ui_labels_switch_but_song_metadata_is_preserved() {
        assert_eq!(translate(Language::En, "БИБЛИОТЕКА"), "LIBRARY");
        assert_eq!(translate(Language::Ru, "LIBRARY"), "БИБЛИОТЕКА");
        assert_eq!(translate(Language::En, "Enjoy the Silence"), "Enjoy the Silence");
        assert_eq!(Language::En.toggle(), Language::Ru);
        assert_eq!(Language::Ru.switch_label(), "EN");
    }
}
