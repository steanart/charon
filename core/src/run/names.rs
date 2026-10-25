//! Names Charon makes up: conflict copies ("plan (conflict 2026-10-16 16.20).txt") and
//! numbered items in the trash ("plan.txt (2)"). Drives allow 255 bytes (Linux) or 255
//! UTF-16 units (Windows, exFAT, Mac OS Extended) per name, so a long name is shortened at
//! a character boundary to make room for what Charon adds, instead of failing every run.
//! Mac OS Extended counts a name decomposed (é as e and an accent), which makes accented
//! names longer there than they look.

use unicode_normalization::UnicodeNormalization;

/// The longest name every drive accepts, in bytes and in UTF-16 units.
const LIMIT: usize = 255;

/// `head` + `tail`, with `head` shortened as far as needed for the whole to fit.
pub(crate) fn fit(head: &str, tail: &str) -> String {
    shorten(head, tail, LIMIT, 0).unwrap_or_else(|| tail.to_string())
}

/// `head` + `tail` in at most `bytes` bytes (and within every drive's limit on names), with
/// `head` shortened as far as needed, but to no fewer than `keep` of its characters. `None`
/// when that doesn't fit: a name that must fit a path's remaining room may have less than a
/// whole name's.
pub(crate) fn shorten(head: &str, tail: &str, bytes: usize, keep: usize) -> Option<String> {
    let fits = |text: &str| {
        text.len() <= bytes.min(LIMIT) && text.encode_utf16().count() <= LIMIT && text.nfd().map(char::len_utf16).sum::<usize>() <= LIMIT
    };
    let mut head: Vec<char> = head.chars().collect();
    loop {
        let name = format!("{}{tail}", head.iter().collect::<String>());
        if fits(&name) {
            return Some(name);
        }
        if head.len() <= keep {
            return None;
        }
        head.pop();
    }
}

/// `name` split into its stem and extension: `plan.txt` → (`plan`, `.txt`); a name that
/// starts with its only dot (`.env`) has no extension.
pub(crate) fn split_extension(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 && name.len() - i <= 16 => (&name[..i], &name[i..]),
        _ => (name, ""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_names_are_shortened_to_fit() {
        let long = "é".repeat(200);
        let name = fit(&long, " (conflict 2026-10-16 16.20).txt");
        assert!(name.len() <= 255);
        assert!(name.ends_with(" (conflict 2026-10-16 16.20).txt"));
        assert_eq!(fit("plan", ".txt"), "plan.txt");
    }

    #[test]
    fn names_fit_mac_os_extended_too() {
        // ǖ is 2 bytes and 1 UTF-16 unit, but u and two accents (3 units) once decomposed:
        // 100 of them fit in bytes and in UTF-16 units, not on Mac OS Extended.
        let name = fit(&"\u{1d6}".repeat(100), " (conflict 2026-10-16 16.20).txt");
        assert!(name.nfd().map(char::len_utf16).sum::<usize>() <= 255, "{} units", name.nfd().count());
        assert!(name.ends_with(" (conflict 2026-10-16 16.20).txt"));
    }

    #[test]
    fn a_name_shortened_to_the_room_a_path_leaves_keeps_some_of_the_original() {
        assert_eq!(shorten("quarterly report", " (2).txt", 12, 1).as_deref(), Some("quar (2).txt"));
        assert_eq!(shorten("plan", " (conflict 2026-10-16 16.20).txt", 20, 1), None, "not even “p” fits");
        assert_eq!(fit("", " (2)"), " (2)");
    }

    #[test]
    fn extensions() {
        assert_eq!(split_extension("plan.txt"), ("plan", ".txt"));
        assert_eq!(split_extension(".env"), (".env", ""));
        assert_eq!(split_extension("archive.tar.gz"), ("archive.tar", ".gz"));
        assert_eq!(split_extension("noext"), ("noext", ""));
    }
}
