//! Capture policy of Raffi's own clipboard history: which selections are
//! dropped, which become temporary entries, and which are kept normally.
//!
//! Exclusions always win, and a selection a password manager marked as a
//! secret is only recorded when `temporary_password_manager` opts in. CopyQ
//! keeps recording everything either way.

use crate::ClipboardAddonConfig;

/// MIME type password managers set on secrets (KeePassXC, among others).
pub(crate) const PASSWORD_HINT: &str = "x-kde-passwordManagerHint";

/// What Raffi's history does with a selection, applied after its formats are
/// normalized into the searchable text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Disposition {
    /// Not recorded, and hidden from the browser.
    Skip,
    /// Recorded, then dropped once `Policy::temporary_timeout` elapses.
    Temporary,
    /// Recorded until it ages out of the history.
    Normal,
}

/// Compiled `addons.clipboard` capture settings.
#[derive(Debug, Clone)]
pub(crate) struct Policy {
    /// `exclude_patterns`: never recorded.
    pub(crate) exclude: Vec<regex::Regex>,
    /// `temporary_patterns`: recorded as temporary entries.
    pub(crate) temporary: Vec<regex::Regex>,
    /// `temporary_timeout` in seconds.
    pub(crate) temporary_timeout: i64,
    /// `temporary_password_manager`: record password-manager copies instead
    /// of skipping them.
    pub(crate) temporary_password_manager: bool,
}

impl Policy {
    pub(crate) fn from_config(config: &ClipboardAddonConfig) -> Result<Self, String> {
        Ok(Self {
            exclude: config.exclude_regexes()?,
            temporary: config.temporary_regexes()?,
            temporary_timeout: config.temporary_timeout_secs()?,
            temporary_password_manager: config.temporary_password_manager(),
        })
    }

    pub(crate) fn disposition(&self, text: &str, password_marked: bool) -> Disposition {
        disposition(
            text,
            password_marked,
            &self.exclude,
            &self.temporary,
            self.temporary_password_manager,
        )
    }
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            exclude: Vec::new(),
            temporary: Vec::new(),
            temporary_timeout: crate::DEFAULT_TEMPORARY_TIMEOUT as i64,
            temporary_password_manager: false,
        }
    }
}

/// The policy outcome for a selection whose normalized searchable `text` is
/// known, for example from `clipboard_store::text_of`.
pub(crate) fn disposition(
    text: &str,
    password_marked: bool,
    exclude: &[regex::Regex],
    temporary: &[regex::Regex],
    temporary_password_manager: bool,
) -> Disposition {
    if matches_any(text, exclude) {
        Disposition::Skip
    } else if password_marked {
        if temporary_password_manager {
            Disposition::Temporary
        } else {
            Disposition::Skip
        }
    } else if matches_any(text, temporary) {
        Disposition::Temporary
    } else {
        Disposition::Normal
    }
}

/// Whether `text` matches any of `patterns`. Entries without text never
/// match, so a pattern cannot catch image-only or unreadable selections.
pub(crate) fn matches_any(text: &str, patterns: &[regex::Regex]) -> bool {
    !text.is_empty() && patterns.iter().any(|pattern| pattern.is_match(text))
}

#[cfg(test)]
mod tests {
    use super::super::clipboard_store::text_of;
    use super::*;

    fn patterns(values: &[&str]) -> Vec<regex::Regex> {
        values
            .iter()
            .map(|value| regex::Regex::new(value).unwrap())
            .collect()
    }

    #[test]
    fn test_default_policy_keeps_everything() {
        let policy = Policy::from_config(&ClipboardAddonConfig::default()).unwrap();
        assert!(policy.exclude.is_empty());
        assert!(policy.temporary.is_empty());
        assert_eq!(policy.temporary_timeout, 60);
        assert!(!policy.temporary_password_manager);
        assert_eq!(policy.disposition("anything", false), Disposition::Normal);
    }

    #[test]
    fn test_policy_from_config_compiles_patterns_and_timeout() {
        let config: ClipboardAddonConfig = serde_yaml::from_str(
            "temporary_patterns: ['(?i)^verification code:', '^OTP:']\n\
             temporary_timeout: 30\n\
             temporary_password_manager: true\n",
        )
        .unwrap();
        let policy = Policy::from_config(&config).unwrap();
        assert_eq!(policy.temporary.len(), 2);
        assert_eq!(policy.temporary_timeout, 30);
        assert!(policy.temporary_password_manager);
        assert_eq!(
            policy.disposition("Verification code: 1234", false),
            Disposition::Temporary
        );
        assert_eq!(
            policy.disposition("unrelated text", false),
            Disposition::Normal
        );
    }

    #[test]
    fn test_invalid_temporary_pattern_names_its_config_key() {
        let config = ClipboardAddonConfig {
            temporary_patterns: vec!["^ok".into(), "(unclosed".into()],
            ..Default::default()
        };
        let error = Policy::from_config(&config).unwrap_err();
        assert!(
            error.starts_with("addons.clipboard.temporary_patterns[1]: "),
            "{error}"
        );
    }

    #[test]
    fn test_exclusions_win_over_temporary_rules() {
        let exclude = patterns(&["^secret:"]);
        let temporary = patterns(&["(?i)secret"]);
        assert_eq!(
            disposition("secret: hunter2", false, &exclude, &temporary, true),
            Disposition::Skip
        );
        // Even a password-manager copy is skipped when excluded.
        assert_eq!(
            disposition("secret: hunter2", true, &exclude, &temporary, true),
            Disposition::Skip
        );
        assert_eq!(
            disposition("SECRET-ish but allowed", false, &exclude, &temporary, false),
            Disposition::Temporary
        );
    }

    #[test]
    fn test_password_manager_opt_in() {
        let none: [regex::Regex; 0] = [];
        assert_eq!(
            disposition("hunter2", true, &none, &none, false),
            Disposition::Skip
        );
        assert_eq!(
            disposition("hunter2", true, &none, &none, true),
            Disposition::Temporary
        );
        // Unmarked copies are unaffected by the flag.
        assert_eq!(
            disposition("hunter2", false, &none, &none, true),
            Disposition::Normal
        );
    }

    #[test]
    fn test_temporary_patterns_match_normalized_html_text() {
        let html = vec![(
            "text/html".to_string(),
            b"<p>Your verification code: <b>424242</b></p>".to_vec(),
        )];
        let text = text_of(&html);
        let temporary = patterns(&["(?i)verification code:"]);
        let exclude = patterns(&["424242"]);
        assert_eq!(
            disposition(&text, false, &[], &temporary, false),
            Disposition::Temporary
        );
        assert_eq!(
            disposition(&text, false, &exclude, &temporary, false),
            Disposition::Skip
        );
        // Image-only captures have no text to match.
        let image = vec![("image/png".to_string(), vec![0x89, 0x50])];
        assert_eq!(
            disposition(&text_of(&image), false, &[], &patterns(&[""]), false),
            Disposition::Normal
        );
    }
}
