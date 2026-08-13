//! mediainfo reports BCP-47-ish tags ("en", "en-US"), mkvmerge reports ISO 639-2 ("eng").
//! Both should agree, otherwise behavior changes based on the backend, normalization is done
//! in this module.

/// Each group aliases one language
const ACCEPTED_LANGS: &[&[&str]] = &[&["en", "eng"], &["de", "ger", "deu"], &["ja", "jpn"]];

/// "en-US" -> "en", "zh-Hans" -> "zh". Region and script subtags never change which
/// language a track is in, so they must not affect the decision to drop it.
fn primary_subtag(code: &str) -> &str {
    code.split(['-', '_']).next().unwrap_or(code)
}

/// True for tracks carrying no usable language tag. These are never stripped: we
/// cannot know what they contain.
pub fn is_undefined_lang(code: Option<&str>) -> bool {
    match code {
        None => true,
        Some(c) => {
            let c = c.trim();
            c.is_empty() || c.eq_ignore_ascii_case("und")
        }
    }
}

pub fn is_accepted_lang(code: &str) -> bool {
    let primary = primary_subtag(code.trim());
    ACCEPTED_LANGS
        .iter()
        .flat_map(|group| group.iter())
        .any(|lang| lang.eq_ignore_ascii_case(primary))
}

/// A track is undesired only when it carries a tag we recognize as not accepted
pub fn is_undesired_lang(code: Option<&str>) -> bool {
    !is_undefined_lang(code) && !is_accepted_lang(code.unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_codes_are_accepted() {
        for code in ["en", "eng", "de", "ger", "deu", "ja", "jpn"] {
            assert!(is_accepted_lang(code), "{code} should be accepted");
        }
    }

    /// mediainfo reports region subtags verbatim, and treating "en-US"
    /// as undesired leads to deleted English audio tracks
    #[test]
    fn region_and_script_subtags_are_stripped_before_comparing() {
        for code in ["en-US", "en-GB", "de-AT", "ja-JP", "en_US"] {
            assert!(is_accepted_lang(code), "{code} should be accepted");
            assert!(
                !is_undesired_lang(Some(code)),
                "{code} must never be dropped"
            );
        }
        // A region subtag must not rescue a language we do not accept
        assert!(is_undesired_lang(Some("pt-BR")));
    }

    #[test]
    fn comparison_is_case_insensitive() {
        for code in ["EN", "En-us", "ENG", "Jpn"] {
            assert!(is_accepted_lang(code), "{code} should be accepted");
        }
    }

    #[test]
    fn unaccepted_languages_are_undesired() {
        for code in ["fr", "fre", "fra", "es", "spa", "ru", "zh-Hans"] {
            assert!(is_undesired_lang(Some(code)), "{code} should be undesired");
            assert!(!is_accepted_lang(code));
        }
    }

    #[test]
    fn undefined_tags_are_neither_accepted_nor_undesired() {
        for code in [None, Some(""), Some("   "), Some("und"), Some("UND")] {
            assert!(is_undefined_lang(code), "{code:?} should be undefined");
            // Never dropped: we cannot know what the track contains
            assert!(!is_undesired_lang(code), "{code:?} must never be dropped");
        }
    }

    #[test]
    fn a_tag_is_never_both_undefined_and_undesired() {
        for code in [
            None,
            Some(""),
            Some("und"),
            Some("en"),
            Some("en-US"),
            Some("fr"),
        ] {
            assert!(!(is_undefined_lang(code) && is_undesired_lang(code)));
        }
    }
}
