//! Slug and branch-name helpers for the create dialog.

/// Maximum length of the slugified portion of a branch name.
pub const MAX_SLUG_LEN: usize = 50;

/// Convert a title into a git-branch-safe slug.
///
/// Lowercases, replaces whitespace/hyphens/underscores with a single hyphen,
/// strips non-ASCII-alphanumeric characters, collapses runs of hyphens, and
/// trims leading/trailing hyphens.
pub fn slugify(title: &str) -> String {
    let lower = title.to_lowercase();
    let mut result = String::with_capacity(lower.len());
    let mut prev_hyphen = false;

    for c in lower.chars() {
        if c.is_ascii_alphanumeric() {
            prev_hyphen = false;
            result.push(c);
        } else if (c.is_whitespace() || c == '-' || c == '_') && !prev_hyphen && !result.is_empty()
        {
            result.push('-');
            prev_hyphen = true;
        }
        // All other characters are silently dropped.
    }

    // Trim trailing hyphen.
    if result.ends_with('-') {
        result.pop();
    }

    result
}

/// Truncate a slug to at most `max_len` bytes, cutting at the last hyphen
/// boundary to avoid mid-word breaks. Falls back to a hard cut when the
/// slug contains no hyphens within the limit.
pub fn truncate_slug(slug: &str, max_len: usize) -> String {
    if slug.len() <= max_len {
        return slug.to_string();
    }
    // Find last hyphen at or before max_len.
    slug[..max_len].rfind('-').map_or_else(
        || slug[..max_len].to_string(),
        |pos| slug[..pos].to_string(),
    )
}

/// Generate a 4-character hex suffix for branch name uniqueness.
pub fn random_suffix() -> String {
    let bytes = uuid::Uuid::new_v4();
    let b = bytes.as_bytes();
    format!("{:02x}{:02x}", b[0], b[1])
}

#[cfg(test)]
mod tests {
    use super::{random_suffix, slugify, truncate_slug};

    // -- slugify tests --

    #[test]
    fn slugify_basic_title() {
        assert_eq!(slugify("Fix Login Bug"), "fix-login-bug");
    }

    #[test]
    fn slugify_special_chars_stripped() {
        assert_eq!(slugify("Fix Login Bug!!"), "fix-login-bug");
    }

    #[test]
    fn slugify_collapses_whitespace() {
        assert_eq!(slugify("a   b"), "a-b");
    }

    #[test]
    fn slugify_underscores_and_hyphens() {
        assert_eq!(slugify("my_cool--feature"), "my-cool-feature");
    }

    #[test]
    fn slugify_empty_input() {
        assert_eq!(slugify(""), "");
    }

    #[test]
    fn slugify_all_special_chars() {
        assert_eq!(slugify("!!!"), "");
    }

    #[test]
    fn slugify_leading_trailing_whitespace() {
        assert_eq!(slugify("  hello world  "), "hello-world");
    }

    // -- truncate_slug tests --

    #[test]
    fn truncate_slug_no_truncation_needed() {
        assert_eq!(truncate_slug("fix-login-bug", 50), "fix-login-bug");
    }

    #[test]
    fn truncate_slug_at_word_boundary() {
        let slug = "implement-comprehensive-authentication-system-with-oauth2-and-saml-support";
        let result = truncate_slug(slug, 50);
        assert!(result.len() <= 50, "got len {}: {result}", result.len());
        // Should cut at a hyphen boundary
        assert!(
            !result.ends_with('-'),
            "should not end with hyphen: {result}"
        );
        assert_eq!(result, "implement-comprehensive-authentication-system");
    }

    #[test]
    fn truncate_slug_single_long_word() {
        let slug = "abcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyz0123456789";
        let result = truncate_slug(slug, 50);
        assert_eq!(result.len(), 50);
        assert_eq!(result, &slug[..50]);
    }

    #[test]
    fn truncate_slug_exact_boundary() {
        let slug = "a".repeat(50);
        assert_eq!(truncate_slug(&slug, 50), slug);
    }

    // -- random_suffix tests --

    #[test]
    fn random_suffix_is_4_hex_chars() {
        let s = random_suffix();
        assert_eq!(s.len(), 4);
        assert!(s.chars().all(|c| c.is_ascii_hexdigit()), "not hex: {s}");
    }
}
