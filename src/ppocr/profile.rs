//! Explicit identities for the fixed PP-OCRv6 model/dictionary pairs.
use crate::Result;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum OcrProfile {
    #[default]
    Tiny,
    Small,
}

/// Dictionary file entries exclude the CTC blank and the appended space class.
/// Both fixed official files contain no empty entry or ASCII space entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DictionaryClassContract {
    pub dictionary_entries: usize,
    pub ctc_classes: usize,
    pub blank_index: usize,
    pub space_index: usize,
}

impl OcrProfile {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "tiny" => Ok(Self::Tiny),
            "small" => Ok(Self::Small),
            _ => Err("OCR 模型须为 tiny 或 small".into()),
        }
    }

    pub const fn id(self) -> &'static str {
        match self {
            Self::Tiny => "tiny",
            Self::Small => "small",
        }
    }

    pub const fn as_str(self) -> &'static str {
        self.id()
    }

    pub const fn provider_id(self) -> &'static str {
        match self {
            Self::Tiny => "ppocrv6-tiny",
            Self::Small => "ppocrv6-small",
        }
    }

    #[cfg(test)]
    pub const fn index(self) -> usize {
        match self {
            Self::Tiny => 0,
            Self::Small => 1,
        }
    }

    pub const fn dict_class_contract(self) -> DictionaryClassContract {
        let dictionary_entries = match self {
            Self::Tiny => 6904,
            Self::Small => 18708,
        };
        DictionaryClassContract {
            dictionary_entries,
            ctc_classes: dictionary_entries + 2,
            blank_index: 0,
            space_index: dictionary_entries + 1,
        }
    }

    /// Byte/SHA validation belongs to assets; this checks the parsed CTC pairing.
    /// Entries are not trimmed, sorted, deduplicated or inferred from a path.
    pub(crate) fn validate_dictionary(self, dictionary: &[String]) -> Result<()> {
        let expected = self.dict_class_contract().dictionary_entries;
        if dictionary.len() != expected || dictionary.iter().any(String::is_empty) {
            return Err(format!(
                "OCR {} 词典应有 {expected} 个非空原始项，实际 {}",
                self.as_str(),
                dictionary.len()
            )
            .into());
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "profile_tests.rs"]
mod tests;
