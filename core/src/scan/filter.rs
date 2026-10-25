//! Exclude patterns: which items Charon never looks at, copies or deletes.

use crate::error::{Error, Result};
use crate::scan::{Entry, Keys};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

/// Charon's own safe-delete folder, at the root of each side.
pub const TRASH_FOLDER: &str = ".charon-trash";
/// Charon's own small folder at the root of each side: it holds the marker that tells
/// Charon it's looking at the same folder as last time.
pub const MARKER_FOLDER: &str = ".charon";
/// What Charon 0.2 and earlier added to files being copied (`report.pdf.charon-partial`).
pub const PARTIAL_SUFFIX: &str = ".charon-partial";
/// Files being copied are written as `.charon-<16 hex digits>.part` next to their real
/// name, then renamed once complete. The name is short, so a long name never gets too long.
pub const PARTIAL_PREFIX: &str = ".charon-";
pub const PARTIAL_EXTENSION: &str = ".part";
/// The modification time of an empty file that holds a name for a moment, while an item is
/// renamed onto it (see `run::rename`): 2 January 1980, 00:00:00 UTC. Every drive can keep
/// it (FAT's dates start on 1 January 1980, in local time), and apps don't save files with
/// it. An empty file with exactly this time is such a claim, left behind by a sync that was
/// interrupted, not the person's own file.
pub const CLAIM_MODIFIED_MS: i64 = 315_619_200_000;

/// A fresh temporary name, `.charon-0123456789abcdef.part`: its first 8 hex digits are the
/// time it was made, in seconds (see `partial_started`), the other 8 are random.
pub fn new_partial_name() -> String {
    let seconds = chrono::Utc::now().timestamp() as u32;
    let random = uuid::Uuid::new_v4().as_u128() as u32;
    format!("{PARTIAL_PREFIX}{seconds:08x}{random:08x}{PARTIAL_EXTENSION}")
}

/// Whether `entry` is an empty file that only held a name for Charon (see `CLAIM_MODIFIED_MS`).
pub fn is_claim(entry: &Entry) -> bool {
    entry.is_file() && entry.size == 0 && entry.modified == CLAIM_MODIFIED_MS
}

/// Whether `name` is a file Charon was copying (now or in an interrupted run).
pub fn is_partial(name: &str) -> bool {
    if is_legacy_partial(name) {
        return true;
    }
    partial_digits(name).is_some()
}

/// When the copy written under the temporary name `name` was started, in milliseconds since
/// 1970: its first 8 hex digits count the seconds (see `Copier::partial_path`). `None` for
/// other names, and for the temporary names of earlier versions, whose digits were all
/// random (a time that isn't within the last ten years is taken for one of those).
pub fn partial_started(name: &str, now_ms: i64) -> Option<i64> {
    const TEN_YEARS_MS: i64 = 10 * 366 * 24 * 3_600_000;
    let seconds = i64::from_str_radix(&partial_digits(name)?[..8], 16).ok()?;
    let started = seconds * 1000;
    (now_ms - TEN_YEARS_MS..=now_ms + 24 * 3_600_000).contains(&started).then_some(started)
}

/// The 16 hex digits of a temporary name `.charon-0123456789abcdef.part`.
fn partial_digits(name: &str) -> Option<&str> {
    let digits = name.strip_prefix(PARTIAL_PREFIX).and_then(|rest| rest.strip_suffix(PARTIAL_EXTENSION))?;
    (digits.len() == 16 && digits.bytes().all(|b| b.is_ascii_hexdigit())).then_some(digits)
}

/// Whether `name` is named like a half-copied file of Charon 0.2 and earlier. Such a name
/// may just as well be the person's own file, so these are only ever moved from a list the
/// person confirmed, by name.
pub fn is_legacy_partial(name: &str) -> bool {
    name.ends_with(PARTIAL_SUFFIX)
}

/// Whether `name` is the name of one of Charon's own folders written in other upper and
/// lower case (`.Charon-Trash`). On a drive that ignores case, such an item is where
/// Charon's own folder goes; wherever keys are folded, it has that folder's key.
pub fn is_charon_folder_in_other_case(name: &str) -> bool {
    name != TRASH_FOLDER && name != MARKER_FOLDER && [TRASH_FOLDER, MARKER_FOLDER].contains(&Keys::fold(name).as_str())
}

/// Files the operating system scatters into folders by itself (Finder and Explorer
/// settings, thumbnail caches, custom folder icons, macOS metadata on non-Mac drives).
/// They never keep a folder from being removed: when everything else in a folder is gone,
/// they go along with it. Your own exclude patterns are never treated like this.
pub fn is_clutter(name: &str) -> bool {
    name == "Icon\r"
        || name.starts_with("._")
        || [".ds_store", "thumbs.db", "ehthumbs.db", "desktop.ini"].contains(&name.to_lowercase().as_str())
}

/// Folders a drive keeps for itself at its top level (recycle bins, indexes, repair
/// folders). At the top of a job's folder they are never looked at.
pub fn is_system_folder(name: &str) -> bool {
    let lower = name.to_lowercase();
    [
        "system volume information",
        "$recycle.bin",
        ".trashes",
        ".spotlight-v100",
        ".fseventsd",
        ".temporaryitems",
        ".documentrevisions-v100",
        ".pkinstallsandboxmanager",
        ".pkinstallsandboxmanager-systemsoftware",
        "lost+found",
    ]
    .contains(&lower.as_str())
        || lower.starts_with(".trash-")
}

/// Patterns without a `/` match an item's name anywhere in the tree (`node_modules`,
/// `*.tmp`); patterns with a `/` match its path from the folder's root (`build/cache`). A
/// pattern starting with `/` only matches from the root (`/Photos Library`).
///
/// When keys are folded (one of the drives ignores upper and lower case), patterns match
/// the folded key, built the same way as the plan's keys (see `Keys::fold`): every letter
/// then counts the same in either case, "ü" and "Ü" as much as "a" and "A". An item and
/// its other spelling on the other side have one key, so a pattern leaves out both or
/// neither, and Charon never writes into a folder the person excluded on the other side.
#[derive(Debug, Clone)]
pub struct ExcludeFilter {
    names: GlobSet,
    paths: GlobSet,
    /// Patterns match folded keys (see `Keys::fold`).
    folded: bool,
}

impl ExcludeFilter {
    /// Builds a filter from the job's patterns. Charon's own files are always excluded.
    /// `folded`: one of the drives ignores upper and lower case, so patterns do too.
    pub fn new(patterns: &[String], folded: bool) -> Result<Self> {
        let mut names = GlobSetBuilder::new();
        let mut paths = GlobSetBuilder::new();
        for pattern in patterns {
            let pattern = pattern.trim();
            if pattern.is_empty() {
                continue;
            }
            let anchored = pattern.starts_with('/');
            let body = Keys::normalize(pattern.trim_matches('/'));
            if body.is_empty() {
                continue;
            }
            let body = if folded { Keys::fold(&body) } else { body };
            let glob = GlobBuilder::new(&body).build().map_err(|_| Error::Pattern(pattern.into()))?;
            if anchored || body.contains('/') {
                paths.add(glob);
            } else {
                names.add(glob);
            }
        }
        Ok(Self {
            names: names.build().map_err(|e| Error::Pattern(e.to_string()))?,
            paths: paths.build().map_err(|e| Error::Pattern(e.to_string()))?,
            folded,
        })
    }

    /// Checks that every pattern works whether or not the job's drives ignore case (a
    /// pattern is folded for drives that do, which can change what it says).
    pub fn check(patterns: &[String]) -> Result<()> {
        Self::new(patterns, false)?;
        Self::new(patterns, true)?;
        Ok(())
    }

    /// Whether patterns match folded keys: one of the job's drives ignores upper and lower
    /// case, and keys are folded for both sides.
    pub fn folds(&self) -> bool {
        self.folded
    }

    /// A filter that only hides Charon's own files.
    pub fn none() -> Self {
        Self::new(&[], false).expect("an empty filter is always valid")
    }

    /// The pattern that leaves out exactly the item at `path` (relative, `/`-separated).
    /// Every character a pattern gives a meaning is written as itself: `*`, `?`, brackets
    /// and braces, and a backslash, which on macOS and Linux would make the next character
    /// plain and drop out of the name. Spaces at the end of the name (of any kind) are
    /// written in braces, `/notes{ }`: a pattern is read without the spaces around it.
    pub fn pattern_for(path: &str) -> String {
        let escaped = globset::escape(path.trim_matches('/')).replace('\\', "[\\]");
        let kept = escaped.trim_end();
        match &escaped[kept.len()..] {
            "" => format!("/{kept}"),
            spaces => format!("/{kept}{{{spaces}}}"),
        }
    }

    /// Whether the item at `path` (relative, `/`-separated, NFC) is left alone.
    pub fn excludes(&self, path: &str) -> bool {
        let name = Keys::name(path);
        if Self::is_internal(path, name) || Self::is_apple_double(name) {
            return true;
        }
        if self.folded {
            let key = Keys::fold(path);
            self.names.is_match(Keys::name(&key)) || self.paths.is_match(&key)
        } else {
            self.names.is_match(name) || self.paths.is_match(path)
        }
    }

    /// On a Mac, `._report.pdf` next to `report.pdf` on a drive that isn't a Mac drive
    /// (exFAT, FAT, many network shares) holds the details of `report.pdf` that drive can't
    /// keep itself: its Finder tags, its resource fork. macOS reads and writes it along with
    /// the file. Copied or removed as a file of its own, it would overwrite or strip the
    /// details of another file, so on a Mac it's never synced, whatever the patterns say.
    /// It goes along with its folder when everything else in it is removed.
    fn is_apple_double(name: &str) -> bool {
        cfg!(target_os = "macos") && name.len() > 2 && name.starts_with("._")
    }

    /// Charon's own folders and half-copied files, at any depth (a folder of another job
    /// can sit inside this one, with its own trash and marker).
    pub fn is_internal(path: &str, name: &str) -> bool {
        is_partial(name) || path.split('/').any(|part| part == TRASH_FOLDER || part == MARKER_FOLDER)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filter(patterns: &[&str]) -> ExcludeFilter {
        ExcludeFilter::new(&patterns.iter().map(|p| p.to_string()).collect::<Vec<_>>(), false).unwrap()
    }

    #[test]
    fn names_match_anywhere() {
        let f = filter(&["node_modules", "*.tmp", ".DS_Store"]);
        assert!(f.excludes("node_modules"));
        assert!(f.excludes("app/node_modules"));
        assert!(f.excludes("a/b/c.tmp"));
        assert!(f.excludes("photos/.DS_Store"));
        assert!(!f.excludes("photos/cat.jpg"));
    }

    #[test]
    fn paths_match_from_root() {
        let f = filter(&["build/cache"]);
        assert!(f.excludes("build/cache"));
        assert!(!f.excludes("src/build/cache"));
    }

    #[test]
    fn clutter_is_only_the_system_list() {
        assert!(is_clutter(".DS_Store"));
        assert!(is_clutter("._photo.jpg"));
        assert!(is_clutter("Icon\r"));
        assert!(!is_clutter("notes.tmp"));
        assert!(!is_clutter("node_modules"));
        assert!(is_clutter("thumbs.db") && is_clutter(".ds_store"), "drives that ignore case may spell them so");
    }

    #[test]
    fn a_leading_slash_matches_only_from_the_root() {
        let f = filter(&["/Photos Library"]);
        assert!(f.excludes("Photos Library"));
        assert!(!f.excludes("old/Photos Library"));
        let exact = ExcludeFilter::new(&[ExcludeFilter::pattern_for("Scans [2019]/raw")], false).unwrap();
        assert!(exact.excludes("Scans [2019]/raw"));
        assert!(!exact.excludes("Scans 2/raw"));
    }

    /// "Leave this out" excludes exactly the item, whatever its name holds: on macOS and
    /// Linux a name may hold a backslash, which a pattern otherwise reads as "the next
    /// character is plain", and a name from a Mac or Linux may end in a space, which a
    /// pattern is otherwise read without.
    #[test]
    fn the_pattern_for_an_item_leaves_out_exactly_that_item() {
        let mut names = vec!["Scans [2019]/raw", "a*b?c", "{x,y}", "[!z]", "dir/[a-c]", "notes ", "Docs/draft ", "Docs/plan\u{a0}", "Ab \u{3000}", " "];
        if cfg!(unix) {
            names.extend(["Q1\\Q2 report.pdf", "draft\\", "\\*", "a\\[b]", "tab\t", "a\\ "]);
        }
        for name in names {
            let pattern = ExcludeFilter::pattern_for(name);
            assert!(ExcludeFilter::check(std::slice::from_ref(&pattern)).is_ok(), "{pattern}");
            for folded in [false, true] {
                let filter = ExcludeFilter::new(std::slice::from_ref(&pattern), folded).unwrap();
                assert!(filter.excludes(name), "{pattern} leaves out {name}");
                for other in [name.replace('\\', ""), name.trim_end().to_string()] {
                    assert!(other == name || other.is_empty() || !filter.excludes(&other), "{pattern} leaves out only {name:?}, not {other:?}");
                }
            }
        }
        let backslash = ExcludeFilter::new(&[ExcludeFilter::pattern_for("Q1\\Q2.pdf")], false).unwrap();
        assert!(!backslash.excludes("Q1Q2.pdf") && !backslash.excludes("Q1/Q2.pdf"));
    }

    #[test]
    fn patterns_ignore_case_when_a_drive_does() {
        let f = ExcludeFilter::new(&["*.TMP".to_string(), "Build/Cache".to_string()], true).unwrap();
        assert!(f.excludes("a/notes.tmp"));
        assert!(f.excludes("build/cache"));
        assert!(!filter(&["*.TMP"]).excludes("a/notes.tmp"));
    }

    /// Keys fold every letter, so patterns must too: otherwise "ENTWÜRFE" would be synced
    /// into the folder "Entwürfe" the person excluded, which a drive that ignores case
    /// takes for the same folder.
    #[test]
    fn folded_patterns_ignore_the_case_of_every_letter() {
        let patterns = ["Entw\u{fc}rfe", "R\u{e9}sum\u{e9}*", "/Fotos/\u{c4}lter"].map(String::from);
        let folded = ExcludeFilter::new(&patterns, true).unwrap();
        for spelling in ["Entw\u{fc}rfe", "ENTW\u{dc}RFE", "entw\u{fc}rfe", "notes/ENTW\u{dc}RFE"] {
            assert!(folded.excludes(spelling), "{spelling}");
        }
        assert!(folded.excludes("R\u{c9}SUM\u{c9}.pdf"));
        assert!(folded.excludes("FOTOS/\u{e4}LTER"), "anchored patterns too");
        assert!(!folded.excludes("old/Fotos/\u{c4}lter"), "and they stay anchored");
        assert!(!folded.excludes("Entw\u{fc}rfe.txt"));
        assert!(!folded.excludes("Lebenslauf.pdf"));
        let decomposed = ExcludeFilter::new(&["Re\u{301}sume\u{301}*".to_string()], true).unwrap();
        assert!(decomposed.excludes("R\u{c9}SUM\u{c9}.pdf"), "a pattern typed decomposed");

        let exact = ExcludeFilter::new(&patterns, false).unwrap();
        assert!(exact.excludes("Entw\u{fc}rfe"));
        assert!(!exact.excludes("ENTW\u{dc}RFE"), "drives that tell case apart keep the names apart");
    }

    #[test]
    fn a_pattern_must_work_in_both_cases() {
        let fine = ["*.TMP", "[A-Z]*", "Entw\u{fc}rfe"].map(String::from);
        assert!(ExcludeFilter::check(&fine).is_ok());
        // From Z to a is a range; folded it would run backwards, from z to a.
        let backwards = ["[Z-a]".to_string()];
        assert!(ExcludeFilter::new(&backwards, false).is_ok());
        assert!(matches!(ExcludeFilter::check(&backwards), Err(Error::Pattern(_))));
    }

    #[test]
    fn patterns_ignore_case_in_every_alphabet_when_a_drive_does() {
        let f = ExcludeFilter::new(&["Entwürfe".to_string(), "/Δοκιμές/*.TMP".to_string()], true).unwrap();
        assert!(f.excludes("Projekte/ENTWÜRFE"));
        assert!(f.excludes("Projekte/entwürfe"));
        assert!(f.excludes("ΔΟΚΙΜΈΣ/notes.tmp"), "the final sigma too");
        assert!(!f.excludes("Projekte/Entwurf"));
        assert!(!filter(&["Entwürfe"]).excludes("Projekte/ENTWÜRFE"), "patterns match case exactly where both drives do");
    }

    #[test]
    fn drive_folders_are_known() {
        assert!(is_system_folder("System Volume Information"));
        assert!(is_system_folder("$RECYCLE.BIN"));
        assert!(is_system_folder(".Trash-1000"));
        assert!(!is_system_folder("Photos"));
    }

    #[test]
    fn a_temporary_name_tells_when_its_copy_started() {
        let now = 1_760_000_000_000;
        let name = format!(".charon-{:08x}{:08x}.part", now / 1000 - 60, 0xabcdefu32);
        assert!(is_partial(&name));
        assert_eq!(partial_started(&name, now), Some(now - 60_000));
        assert_eq!(partial_started(".charon-ffffffff00000000.part", now), None, "random digits of an older version");
        assert_eq!(partial_started(".charon-0000000100000000.part", now), None);
        assert_eq!(partial_started("report.pdf", now), None);
    }

    #[test]
    fn a_new_temporary_name_is_known_for_one() {
        let now = chrono::Utc::now().timestamp_millis();
        let name = new_partial_name();
        assert!(is_partial(&name) && !is_legacy_partial(&name));
        assert!(partial_started(&name, now).is_some_and(|started| (now - started).abs() < 5_000), "{name}");
        assert_ne!(new_partial_name(), name);
    }

    #[test]
    fn only_an_empty_file_with_the_claims_time_is_a_claim() {
        assert!(is_claim(&Entry::file(0, CLAIM_MODIFIED_MS)));
        assert!(!is_claim(&Entry::file(1, CLAIM_MODIFIED_MS)));
        assert!(!is_claim(&Entry::file(0, CLAIM_MODIFIED_MS + 1_000)));
        // Files from zip archives without dates get midnight of 1 January 1980 (local time).
        assert!(!is_claim(&Entry::file(0, 315_532_800_000)));
        assert!(!is_claim(&Entry::link(b"", CLAIM_MODIFIED_MS)));
    }

    #[test]
    fn apple_double_files_are_left_alone_on_a_mac() {
        let f = filter(&[]);
        assert_eq!(f.excludes("photos/._cat.jpg"), cfg!(target_os = "macos"));
        assert!(!f.excludes("photos/._"));
        assert!(!f.excludes("photos/_cat.jpg"));
    }

    #[test]
    fn own_files_are_always_hidden() {
        let f = ExcludeFilter::none();
        assert!(f.excludes(".charon-trash"));
        assert!(f.excludes(".charon-trash/2026/a.txt"));
        assert!(f.excludes("nested/job/.charon-trash/2026/a.txt"), "another job's trash inside this folder");
        assert!(f.excludes(".charon/marker.json"));
        assert!(f.excludes("docs/report.pdf.charon-partial"));
        assert!(f.excludes("docs/.charon-0123456789abcdef.part"));
        assert!(!f.excludes("docs/.charon-notes.part"));
        assert!(!f.excludes("docs/.charon-trash-notes.txt"));
        assert!(!f.excludes("docs/.charon.txt"));
    }

    #[test]
    fn charons_folder_names_are_told_in_other_case() {
        assert!(is_charon_folder_in_other_case(".Charon-Trash"));
        assert!(is_charon_folder_in_other_case(".CHARON"));
        assert!(is_charon_folder_in_other_case(".charon-tra\u{17f}h"), "the long s is an s where case is folded");
        assert!(!is_charon_folder_in_other_case(TRASH_FOLDER), "Charon's own names as they are written");
        assert!(!is_charon_folder_in_other_case(MARKER_FOLDER));
        assert!(!is_charon_folder_in_other_case(".Charon-Notes"));
        assert!(!is_charon_folder_in_other_case("Charon"));
    }
}
