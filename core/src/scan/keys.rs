//! Paths as Charon compares them: relative to the job's folder, `/`-separated, and in
//! Unicode NFC form.
//!
//! The same name can be stored in two ways: "é" as one character (NFC, what most systems
//! write) or as "e" plus an accent (NFD, what macOS HFS+ drives store). Comparing the NFC
//! forms makes both spellings the same file, so a name on an older Mac drive is never
//! mistaken for a different file than the one on the computer.
//!
//! When either side's drive ignores upper and lower case (most Mac and Windows drives),
//! keys are also *folded* to lower case: "Report.pdf" and "report.pdf" are then one item,
//! as they are to that drive. Each side's own spelling is remembered (see `Snapshot`).

use unicode_normalization::{is_nfc, UnicodeNormalization};

pub struct Keys;

impl Keys {
    /// The key for a relative path as spelled on disk.
    pub fn normalize(path: &str) -> String {
        if is_nfc(path) {
            path.to_string()
        } else {
            path.nfc().collect()
        }
    }

    /// `a/b/c` → `a/b`; `None` for items at the top.
    pub fn parent(key: &str) -> Option<&str> {
        key.rfind('/').map(|i| &key[..i])
    }

    /// `a/b/c` → `c`.
    pub fn name(key: &str) -> &str {
        key.rsplit('/').next().unwrap_or(key)
    }

    /// How many folders deep: `a` is 0, `a/b` is 1.
    pub fn depth(key: &str) -> usize {
        key.matches('/').count()
    }

    /// `a/b/c` → `a/b`, `a` (nearest first).
    pub fn ancestors(key: &str) -> impl Iterator<Item = &str> {
        key.match_indices('/').map(|(i, _)| &key[..i]).collect::<Vec<_>>().into_iter().rev()
    }

    /// Whether `key` is `folder` itself or somewhere inside it.
    pub fn is_within(key: &str, folder: &str) -> bool {
        key == folder || (key.len() > folder.len() && key.starts_with(folder) && key.as_bytes()[folder.len()] == b'/')
    }

    /// The key of an NFC key on drives that ignore case, by Unicode's simple case folding:
    /// "Report" and "report" are one name, and so are the letters that have two lower-case
    /// forms ("ΟΔΟΣ" and "οδος", the micro sign µ and the Greek μ, the long ſ and s), as
    /// they are to the drive.
    pub fn fold(key: &str) -> String {
        let mut folded = String::with_capacity(key.len());
        for c in key.chars() {
            folded.extend(Self::fold_char(c));
        }
        Self::normalize(&folded)
    }

    /// One character's folded form: the lower case of its upper case, so both lower-case
    /// forms of a letter meet. A letter whose upper case is more than one letter (ß, the
    /// ligatures) keeps its own lower case.
    ///
    /// The Turkish dotless ı meets i too. Windows and the drives made for it (NTFS, exFAT,
    /// FAT, most network shares) compare names by their upper case, where ı is I: there
    /// "Fiyatlar" and "Fıyatlar" are one folder. Apple's drives keep ı apart, so two items
    /// named that way side by side are left alone there, as twins, which is safe; treating
    /// one name as two would make a rename on one side look like everything removed and
    /// added again, on a drive that can't hold both.
    fn fold_char(c: char) -> std::char::ToLowercase {
        let mut upper = c.to_uppercase();
        match (upper.next(), upper.next()) {
            (Some(single), None) => single.to_lowercase(),
            _ => c.to_lowercase(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_spellings_give_one_key() {
        assert_eq!(Keys::normalize("cafe\u{301}/menu.txt"), "caf\u{e9}/menu.txt");
        assert_eq!(Keys::normalize("caf\u{e9}/menu.txt"), "caf\u{e9}/menu.txt");
    }

    #[test]
    fn path_pieces() {
        assert_eq!(Keys::parent("a/b/c"), Some("a/b"));
        assert_eq!(Keys::parent("a"), None);
        assert_eq!(Keys::name("a/b/c"), "c");
        assert_eq!(Keys::depth("a/b/c"), 2);
        assert_eq!(Keys::ancestors("a/b/c").collect::<Vec<_>>(), vec!["a/b", "a"]);
        assert!(Keys::is_within("a/b", "a"));
        assert!(Keys::is_within("a", "a"));
        assert!(!Keys::is_within("ab", "a"));
    }

    #[test]
    fn folding_ignores_case() {
        assert_eq!(Keys::fold("Photos/Report.PDF"), "photos/report.pdf");
        assert_eq!(Keys::fold("ΟΔΟΣ"), Keys::fold("οδος"));
        assert_eq!(Keys::fold("ODOς"), "odoσ");
        assert_eq!(Keys::fold("caf\u{e9}"), Keys::fold("CAF\u{c9}"));
    }

    #[test]
    fn letters_with_two_lower_case_forms_fold_together() {
        // Drives that ignore case compare upper case: µ (micro sign) and μ are both Μ there.
        assert_eq!(Keys::fold("\u{b5}.txt"), Keys::fold("\u{3bc}.txt"));
        assert_eq!(Keys::fold("\u{17f}ee"), Keys::fold("See"));
        assert_eq!(Keys::fold("\u{3d1}"), Keys::fold("\u{398}"));
        assert_eq!(Keys::fold("\u{1e9e}"), "\u{df}", "capital ẞ is ß, never ss");
        assert_ne!(Keys::fold("stra\u{df}e"), Keys::fold("strasse"));
        assert_eq!(Keys::fold("\u{212a}elvin"), "kelvin");
    }

    #[test]
    fn the_dotless_i_meets_i_as_on_drives_that_compare_upper_case() {
        // NTFS, exFAT and FAT find "Fıyatlar" under "Fiyatlar" and "FIYATLAR".
        assert_eq!(Keys::fold("F\u{131}yatlar/teklif.pdf"), Keys::fold("Fiyatlar/teklif.pdf"));
        assert_eq!(Keys::fold("F\u{131}yatlar"), Keys::fold("FIYATLAR"));
        assert_eq!(Keys::fold("I\u{131}"), "ii");
        // The capital İ is I with a dot above, apart from I there too.
        assert_ne!(Keys::fold("\u{130}stanbul"), Keys::fold("Istanbul"));
    }
}
