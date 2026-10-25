//! A file, folder or link as Charon sees it, and a whole folder as a sorted map of them,
//! together with what the scan learned about the places it couldn't or mustn't touch.

use crate::scan::cloud::CloudPlaces;
use crate::scan::{CloudService, Keys, Refusal, Volume};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Some file systems (FAT, exFAT, SMB shares) store modification times in 2-second steps,
/// so times this close count as equal.
pub const MTIME_TOLERANCE_MS: i64 = 2_000;

const HOUR_MS: i64 = 3_600_000;
/// Some time zones and summer times move clocks by quarter hours (India +5:30, Nepal
/// +5:45, Lord Howe's half-hour summer time).
const QUARTER_HOUR_MS: i64 = 900_000;
/// The largest time-zone difference a drive's local times can move by.
const MAX_SHIFT_MS: i64 = 14 * HOUR_MS;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EntryKind {
    File,
    Dir,
    /// A symbolic link, carried as a link (never followed).
    Link,
}

/// Size and modification time of one item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub kind: EntryKind,
    /// Bytes of a file; length of a link's target.
    pub size: u64,
    /// Milliseconds since 1970.
    pub modified: i64,
    /// Links: a fingerprint of where the link points. 0 for files and folders.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub target: u64,
    /// Files: when the contents or details last changed on the drive (the "change time"),
    /// in nanoseconds since 1970. An edit that puts the old modification time back (some
    /// encrypted containers and tools do) still moves it. 0 where the drive doesn't keep a
    /// trustworthy one, and 0 matches anything.
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub changed: i64,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

fn is_zero_i64(n: &i64) -> bool {
    *n == 0
}

impl Entry {
    pub fn file(size: u64, modified: i64) -> Self {
        Self { kind: EntryKind::File, size, modified, target: 0, changed: 0 }
    }

    pub fn dir() -> Self {
        Self { kind: EntryKind::Dir, size: 0, modified: 0, target: 0, changed: 0 }
    }

    /// A link pointing at `target` (the raw bytes of the link's text).
    pub fn link(target: &[u8], modified: i64) -> Self {
        Self { kind: EntryKind::Link, size: target.len() as u64, modified, target: Self::fingerprint(target), changed: 0 }
    }

    /// The same file with its change time (0: unknown).
    pub fn changed_at(mut self, changed: i64) -> Self {
        if self.is_file() {
            self.changed = changed;
        }
        self
    }

    pub fn is_dir(&self) -> bool {
        self.kind == EntryKind::Dir
    }

    pub fn is_file(&self) -> bool {
        self.kind == EntryKind::File
    }

    pub fn is_link(&self) -> bool {
        self.kind == EntryKind::Link
    }

    /// Whether two items count as the same content: folders always do, files when size
    /// and modification time match, links when they point to the same place.
    pub fn same_as(&self, other: &Entry) -> bool {
        match (self.kind, other.kind) {
            (EntryKind::Dir, EntryKind::Dir) => true,
            (EntryKind::File, EntryKind::File) => {
                self.size == other.size && (self.modified - other.modified).abs() <= MTIME_TOLERANCE_MS
            }
            (EntryKind::Link, EntryKind::Link) => self.size == other.size && self.target == other.target,
            _ => false,
        }
    }

    /// `same_as` for items that may be missing: two missing items are the same too.
    pub fn same(a: Option<&Entry>, b: Option<&Entry>) -> bool {
        match (a, b) {
            (None, None) => true,
            (Some(a), Some(b)) => a.same_as(b),
            _ => false,
        }
    }

    /// Whether an item on one side is still exactly what was seen on *that same side*
    /// before: same kind, size, modification time to the millisecond and link target.
    /// (`same_as` compares the two sides with each other and allows for drives that round
    /// times; one drive always reports its own times the same way, so here nothing is allowed.)
    /// A file's change time counts too, where both looks know it.
    pub fn unchanged(&self, before: &Entry) -> bool {
        match (self.kind, before.kind) {
            (EntryKind::Dir, EntryKind::Dir) => true,
            _ => {
                self.kind == before.kind
                    && self.size == before.size
                    && self.modified == before.modified
                    && self.target == before.target
                    && (self.changed == 0 || before.changed == 0 || self.changed == before.changed)
            }
        }
    }

    /// Whether a file that changed since `before` kept its size, and its time stayed put
    /// or moved only the way drives move times without anyone editing: by a rounding of
    /// up to 2 seconds, or by whole quarter hours (a time-zone or daylight-saving shift).
    /// Such a file may well have the same contents as before; only reading them tells.
    pub fn drifted_from(&self, before: &Entry) -> bool {
        self.is_file()
            && before.is_file()
            && self.size == before.size
            && ((self.modified - before.modified).abs() <= MTIME_TOLERANCE_MS || Self::apart_by(self.modified, before.modified, QUARTER_HOUR_MS))
    }

    /// `unchanged` for items that may be missing.
    pub fn still(now: Option<&Entry>, before: Option<&Entry>) -> bool {
        match (now, before) {
            (None, None) => true,
            (Some(now), Some(before)) => now.unchanged(before),
            _ => false,
        }
    }

    /// `still`, on a drive that keeps times in local time (FAT, exFAT): a file of the same
    /// size whose time moved by whole hours (a daylight-saving change, another time zone)
    /// is still the same file.
    pub fn still_on(now: Option<&Entry>, before: Option<&Entry>, shifting: bool) -> bool {
        match (now, before) {
            (Some(now), Some(before)) if shifting && now.is_file() && before.is_file() && now.size == before.size => {
                now.modified == before.modified || Self::whole_hours_apart(now.modified, before.modified)
            }
            _ => Self::still(now, before),
        }
    }

    /// Whether two times differ by a whole number of hours (1 to 14, give or take the
    /// 2 seconds some drives round to): the mark of a time-zone or daylight-saving shift.
    /// Only whole hours count where nothing is read: a real edit saved a whole number of
    /// quarter hours after the last one must never pass as a shift.
    pub fn whole_hours_apart(a: i64, b: i64) -> bool {
        Self::apart_by(a, b, HOUR_MS)
    }

    /// Whether two times differ by a whole number of `step`s (up to 14 hours, give or take
    /// 2 seconds).
    fn apart_by(a: i64, b: i64, step: i64) -> bool {
        let difference = (a - b).abs();
        let steps = (difference + step / 2) / step;
        steps >= 1 && steps * step <= MAX_SHIFT_MS && (difference - steps * step).abs() <= MTIME_TOLERANCE_MS
    }

    /// FNV-1a: small and stable across Rust versions, so stored baselines stay valid.
    fn fingerprint(bytes: &[u8]) -> u64 {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in bytes {
            hash ^= *byte as u64;
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
        hash
    }
}

/// Something the scan left out, with the reason, for the review and the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    /// The path as it is on disk (shown to the person).
    pub path: String,
    /// Its key, when its name can be read. Nothing of that name is touched on either side:
    /// a copy into its place would fail on every sync.
    pub key: Option<String>,
    pub reason: String,
}

/// One of Charon's own folders the scan met (`.charon`, see `run::Marker`): the folder
/// holding it is a job's folder, or was one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CharonFolder {
    /// The folder holding it, as spelled on disk, relative to the scanned folder ("" for
    /// the scanned folder itself).
    pub folder: String,
    /// The jobs whose markers it holds.
    pub jobs: Vec<String>,
}

/// What a scan learned besides the items themselves.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ScanFacts {
    /// Key → the path as spelled on disk, where the two differ.
    pub(crate) spellings: BTreeMap<String, String>,
    /// Places Charon couldn't read completely (key → why). Nothing at or below them is
    /// touched, on either side.
    pub(crate) frozen: BTreeMap<String, String>,
    /// Folders that hold things Charon leaves alone (excluded items, links it can't carry,
    /// unreadable places). They are never removed.
    pub(crate) occupied: BTreeSet<String>,
    /// Names written two ways in one folder (Unicode forms, or upper/lower case once keys
    /// are folded). Also frozen.
    pub(crate) clashes: BTreeSet<String>,
    /// Items left out (not readable as a name, special files, links on Windows).
    pub(crate) skipped: Vec<Skipped>,
    /// Items left out without a note because their names are kept for Charon's own use
    /// (its trash, its marker, its half-copied files), for the drive's (its folders at the
    /// top) or for a cloud service's (its placeholders, a sync client's journal): key → the
    /// path as spelled on disk.
    /// Those names are matched as they are written, not by key, so an item whose key is the
    /// same but whose name is written in another case is left alone too, on either side.
    pub(crate) reserved: BTreeMap<String, String>,
    /// The folders where a cloud service keeps files: there, a file named like its
    /// placeholders is its own (see `scan::cloud`).
    pub(crate) clouds: CloudPlaces,
    /// Leftover half-copied files from an interrupted run: on-disk path, size and time.
    pub(crate) partials: Vec<(String, Entry)>,
    /// Charon's own folders met: at the top, and in the folders of other jobs that are the
    /// same place or inside this one.
    pub(crate) charon_folders: Vec<CharonFolder>,
    /// Which folder was scanned (see `RootId`).
    pub(crate) root: Option<crate::scan::RootId>,
    /// The drive it's on.
    pub(crate) volume: Option<Volume>,
    /// Whether keys ignore upper and lower case (see `Keys::fold`).
    pub(crate) folded: bool,
    /// A run found out that links can't be created on this drive (some network shares).
    pub(crate) no_links: bool,
    /// Items this drive refused to take in an earlier sync (key → why).
    pub(crate) refused: BTreeMap<String, Refusal>,
    /// Files a sync couldn't read, and that still can't be read. Also frozen.
    pub(crate) unread: BTreeSet<String>,
}

/// Every item in a folder, keyed by its NFC path relative to the folder, with `/`
/// separators. Only the items are stored (as a two-way baseline); the scan facts are not.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Snapshot {
    entries: BTreeMap<String, Entry>,
    #[serde(skip)]
    facts: ScanFacts,
}

impl Snapshot {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, path: impl Into<String>, entry: Entry) {
        self.entries.insert(path.into(), entry);
    }

    pub fn get(&self, path: &str) -> Option<&Entry> {
        self.entries.get(path)
    }

    pub fn contains(&self, path: &str) -> bool {
        self.entries.contains_key(path)
    }

    pub fn remove(&mut self, path: &str) {
        self.entries.remove(path);
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &Entry)> {
        self.entries.iter()
    }

    pub fn paths(&self) -> impl Iterator<Item = &String> {
        self.entries.keys()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Number of files and links (not folders).
    pub fn file_count(&self) -> usize {
        self.entries.values().filter(|e| !e.is_dir()).count()
    }

    /// Everything inside the folder `key` (not the folder itself).
    pub fn inside<'a>(&'a self, key: &str) -> impl Iterator<Item = (&'a String, &'a Entry)> + 'a {
        let prefix = format!("{key}/");
        self.entries
            .range(prefix.clone()..)
            .take_while(move |(path, _)| path.starts_with(&prefix))
    }

    /// The items present and equal in both snapshots: the state two folders agree on.
    pub fn agreement(a: &Snapshot, b: &Snapshot) -> Snapshot {
        a.entries
            .iter()
            .filter(|(path, entry)| b.get(path).is_some_and(|other| entry.same_as(other)))
            .map(|(path, entry)| (path.clone(), *entry))
            .collect()
    }

    // ---------------------------------------------------------------- scan facts

    /// The path of `key` as it is spelled on disk.
    pub fn spelling<'a>(&'a self, key: &'a str) -> &'a str {
        self.facts.spellings.get(key).map(String::as_str).unwrap_or(key)
    }

    /// Whether this snapshot knows a spelling for `key` (the item exists on this side).
    pub fn has_spelling(&self, key: &str) -> bool {
        self.facts.spellings.contains_key(key)
    }

    /// Why `key` (or a folder around it) couldn't be read completely, if it couldn't.
    pub fn frozen(&self, key: &str) -> Option<&str> {
        if let Some(reason) = self.facts.frozen.get(key) {
            return Some(reason);
        }
        Keys::ancestors(key).find_map(|folder| self.facts.frozen.get(folder)).map(String::as_str)
    }

    /// Every unreadable place, with the reason.
    pub fn frozen_places(&self) -> impl Iterator<Item = (&String, &String)> {
        self.facts.frozen.iter()
    }

    /// Whether the folder `key` holds things Charon leaves alone.
    pub fn is_occupied(&self, key: &str) -> bool {
        self.facts.occupied.contains(key)
    }

    /// Whether `key` was frozen because its name is written two ways in one folder.
    pub fn is_clash(&self, key: &str) -> bool {
        self.facts.clashes.contains(key)
    }

    pub fn skipped(&self) -> &[Skipped] {
        &self.facts.skipped
    }

    /// The items left out because their names are kept for Charon's or the drive's own use
    /// (see `ScanFacts::reserved`): key → path as spelled on disk.
    pub fn reserved(&self) -> impl Iterator<Item = (&String, &String)> {
        self.facts.reserved.iter()
    }

    /// Whether the scan left out an item with the key `key` here: with a note (see
    /// `skipped`), or because its name is kept for Charon's or the drive's own use.
    pub fn leaves_alone(&self, key: &str) -> bool {
        self.facts.reserved.contains_key(key) || self.facts.skipped.iter().any(|skipped| skipped.key.as_deref() == Some(key))
    }

    /// The cloud service that would take an item at `key` here for its placeholder: the item
    /// is named like one, in a folder where that service keeps files (see `scan::cloud`).
    pub fn placeholder_service(&self, key: &str) -> Option<CloudService> {
        self.facts.clouds.service_taking(key)
    }

    /// Charon's own folders met in the folder, with the markers they hold.
    pub fn charon_folders(&self) -> &[CharonFolder] {
        &self.facts.charon_folders
    }

    /// Leftover half-copied files: on-disk path and what they looked like.
    pub fn partials(&self) -> &[(String, Entry)] {
        &self.facts.partials
    }

    /// The drive this folder is on, when it was looked at.
    pub fn volume(&self) -> Option<&Volume> {
        self.facts.volume.as_ref()
    }

    /// Whether the drive keeps times in local time (see `Volume::shifts_times`).
    pub fn shifts_times(&self) -> bool {
        self.facts.volume.as_ref().is_some_and(Volume::shifts_times)
    }

    /// Whether symbolic links can be created on this folder's drive.
    pub fn holds_links(&self) -> bool {
        !self.facts.no_links && self.facts.volume.as_ref().is_none_or(Volume::holds_links)
    }

    /// Notes that this folder's drive turned out not to hold links (a run failed to create one).
    pub fn refuse_links(&mut self) {
        self.facts.no_links = true;
        if let Some(volume) = self.facts.volume.as_mut() {
            volume.refuses_links = true;
        }
    }

    /// Notes that this folder's drive refused to take the item `key` in an earlier sync.
    pub fn refuse(&mut self, key: &str, why: Refusal) {
        self.facts.refused.insert(key.to_string(), why);
    }

    /// Why this folder's drive refused the item `key` in an earlier sync, if it did.
    pub fn refused(&self, key: &str) -> Option<Refusal> {
        self.facts.refused.get(key).copied()
    }

    /// The files a sync couldn't read, and that still can't be read (see `freeze_unread`).
    pub fn unread(&self) -> impl Iterator<Item = &String> {
        self.facts.unread.iter()
    }

    pub fn is_folded(&self) -> bool {
        self.facts.folded
    }

    /// The key for an on-disk relative path, as this snapshot spells keys.
    pub fn key_for(&self, disk: &str) -> String {
        let key = Keys::normalize(disk);
        if self.facts.folded {
            Keys::fold(&key)
        } else {
            key
        }
    }

    /// Turns every key into its folded form (see `Keys::fold`), because one of the two
    /// drives ignores upper and lower case. Names in one folder that only differ in case
    /// can't both exist on such a drive: they are left alone (frozen), and so is everything
    /// inside them.
    pub fn fold(&mut self) {
        if self.facts.folded {
            return;
        }
        let entries = std::mem::take(&mut self.entries);
        let spellings = std::mem::take(&mut self.facts.spellings);
        let mut twins = BTreeSet::new();
        for (key, entry) in entries {
            let folded = Keys::fold(&key);
            if self.entries.contains_key(&folded) {
                twins.insert(folded);
                continue;
            }
            let disk = spellings.get(&key).cloned().unwrap_or(key);
            if disk != folded {
                self.facts.spellings.insert(folded.clone(), disk);
            }
            self.entries.insert(folded, entry);
        }
        let frozen = std::mem::take(&mut self.facts.frozen);
        for (key, reason) in frozen {
            let folded = Keys::fold(&key);
            // What's left alone is named in its note as spelled on disk too (a file only in
            // the cloud, as its placeholder spells it).
            if folded != key {
                self.facts.spellings.entry(folded.clone()).or_insert(key);
            }
            self.facts.frozen.entry(folded).or_insert(reason);
        }
        self.facts.occupied = std::mem::take(&mut self.facts.occupied).iter().map(|key| Keys::fold(key)).collect();
        self.facts.clashes = std::mem::take(&mut self.facts.clashes).iter().map(|key| Keys::fold(key)).collect();
        self.facts.refused = std::mem::take(&mut self.facts.refused).into_iter().map(|(key, why)| (Keys::fold(&key), why)).collect();
        for (key, disk) in std::mem::take(&mut self.facts.reserved) {
            self.facts.reserved.entry(Keys::fold(&key)).or_insert(disk);
        }
        for skipped in &mut self.facts.skipped {
            skipped.key = skipped.key.as_deref().map(Keys::fold);
        }
        self.facts.clouds.fold();
        self.facts.unread = std::mem::take(&mut self.facts.unread).iter().map(|key| Keys::fold(key)).collect();
        for twin in twins {
            self.freeze(&twin, "another item in this folder has the same name in different upper/lower case, and one of the drives can't hold both, so Charon leaves them alone");
            self.facts.clashes.insert(twin);
        }
        self.facts.folded = true;
    }

    /// The identity of the scanned folder, when the system can tell.
    pub fn root(&self) -> Option<crate::scan::RootId> {
        self.facts.root
    }

    pub(crate) fn facts_mut(&mut self) -> &mut ScanFacts {
        &mut self.facts
    }

    /// Marks `key` as unreadable. Its folders can't be removed either.
    pub(crate) fn freeze(&mut self, key: &str, reason: impl Into<String>) {
        self.facts.frozen.entry(key.to_string()).or_insert_with(|| reason.into());
        self.occupy_folders_of(key);
        self.facts.occupied.insert(key.to_string());
    }

    /// Notes an item the scan leaves out, with the reason (`disk`: its path as spelled on
    /// disk; `key`: its key, when its name can be read). Its folders can't be removed.
    pub(crate) fn leave_out(&mut self, disk: String, key: Option<&str>, reason: impl Into<String>) {
        if let Some(key) = key {
            self.occupy_folders_of(key);
        }
        self.facts.skipped.push(Skipped { path: disk, key: key.map(str::to_string), reason: reason.into() });
    }

    /// Notes an item left out because its name is kept for Charon's or the drive's own use
    /// (see `ScanFacts::reserved`).
    pub(crate) fn reserve(&mut self, key: &str, disk: &str) {
        self.facts.reserved.insert(key.to_string(), disk.to_string());
    }

    /// Marks the file `key` as one a sync couldn't read, and that still can't be read: it is
    /// left alone like a place a scan can't read, and while it can't be copied, nothing is
    /// removed on the other side (see `plan::Ground::hold_removals`).
    pub(crate) fn freeze_unread(&mut self, key: &str, reason: impl Into<String>) {
        self.freeze(key, reason);
        self.facts.unread.insert(key.to_string());
    }

    /// Marks `folder` (and the folders around it) as holding something Charon leaves alone.
    pub(crate) fn occupy(&mut self, folder: &str) {
        if self.facts.occupied.insert(folder.to_string()) {
            self.occupy_folders_of(folder);
        }
    }

    /// Marks every folder around `key` as holding something Charon leaves alone.
    pub(crate) fn occupy_folders_of(&mut self, key: &str) {
        for folder in Keys::ancestors(key) {
            if !self.facts.occupied.insert(folder.to_string()) {
                break; // its own folders are marked already
            }
        }
    }
}

impl FromIterator<(String, Entry)> for Snapshot {
    fn from_iter<T: IntoIterator<Item = (String, Entry)>>(iter: T) -> Self {
        Snapshot { entries: iter.into_iter().collect(), facts: ScanFacts::default() }
    }
}

impl IntoIterator for Snapshot {
    type Item = (String, Entry);
    type IntoIter = std::collections::btree_map::IntoIter<String, Entry>;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.into_iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_compare_by_target() {
        let a = Entry::link(b"../shared", 1);
        assert!(a.same_as(&Entry::link(b"../shared", 99_999)));
        assert!(!a.same_as(&Entry::link(b"../other", 1)));
        assert!(!a.same_as(&Entry::file(9, 1)));
    }

    #[test]
    fn one_side_compares_exactly_two_sides_allow_for_rounding() {
        let a = Entry::file(5, 10_000);
        assert!(a.same_as(&Entry::file(5, 11_500)), "another drive may round the time");
        assert!(!a.unchanged(&Entry::file(5, 11_500)), "the same drive reports the same time");
        assert!(a.unchanged(&Entry::file(5, 10_000)));
        assert!(Entry::dir().unchanged(&Entry::dir()));
        assert!(Entry::still(None, None));
        assert!(!Entry::still(Some(&a), None));
    }

    #[test]
    fn whole_hour_shifts_on_fat_drives_are_no_change() {
        let before = Entry::file(5, 1_000_000_000);
        let shifted = Entry::file(5, 1_000_000_000 + 3_600_000);
        assert!(Entry::still_on(Some(&shifted), Some(&before), true));
        assert!(!Entry::still_on(Some(&shifted), Some(&before), false));
        let edited = Entry::file(6, 1_000_000_000 + 3_600_000);
        assert!(!Entry::still_on(Some(&edited), Some(&before), true));
        let odd = Entry::file(5, 1_000_000_000 + 3_000_000);
        assert!(!Entry::still_on(Some(&odd), Some(&before), true));
        assert!(Entry::whole_hours_apart(0, -2 * 3_600_000 + 1_000));
        assert!(!Entry::whole_hours_apart(0, 15 * 3_600_000));
        // Half an hour only counts where the contents are read to be sure.
        let half = Entry::file(5, 1_000_000_000 + 1_800_000);
        assert!(!Entry::still_on(Some(&half), Some(&before), true));
        assert!(half.drifted_from(&before));
        assert!(!Entry::file(5, 1_000_000_000 + 1_000_000).drifted_from(&before));
        assert!(!Entry::file(6, 1_000_000_000).drifted_from(&before));
    }

    #[test]
    fn a_change_time_counts_where_both_looks_know_it() {
        let before = Entry::file(5, 10_000).changed_at(7);
        assert!(Entry::file(5, 10_000).changed_at(7).unchanged(&before));
        assert!(!Entry::file(5, 10_000).changed_at(8).unchanged(&before), "an edit that put the old time back");
        assert!(Entry::file(5, 10_000).unchanged(&before), "an unknown change time matches");
        assert!(Entry::file(5, 10_000).changed_at(8).same_as(&before), "the two sides never compare change times");
        assert!(Entry::file(5, 10_000).changed_at(8).drifted_from(&before));
    }

    #[test]
    fn folding_unites_spellings_and_freezes_case_twins() {
        let mut s = Snapshot::new();
        s.insert("Photos", Entry::dir());
        s.insert("Photos/A.jpg", Entry::file(1, 1));
        s.insert("Readme.md", Entry::file(1, 1));
        s.insert("README.md", Entry::file(2, 1));
        s.fold();
        assert!(s.get("photos/a.jpg").is_some());
        assert_eq!(s.spelling("photos/a.jpg"), "Photos/A.jpg");
        assert!(s.frozen("readme.md").is_some());
        assert!(s.is_clash("readme.md"));
        assert_eq!(s.key_for("PHOTOS/a.JPG"), "photos/a.jpg");
    }

    /// Where a drive ignores case, a note about something left alone still names it as it's
    /// spelled on disk, not by its folded key.
    #[test]
    fn folding_keeps_the_spelling_of_what_is_left_alone() {
        let mut s = Snapshot::new();
        s.insert("Docs", Entry::dir());
        s.freeze("Docs/Report.pdf", "it's only in the cloud");
        s.freeze("Mounted", "another drive is mounted here");
        s.insert("Notes.txt", Entry::file(1, 1));
        s.freeze("Notes.txt", "can't be read");
        s.fold();
        assert!(s.frozen("docs/report.pdf").is_some());
        assert_eq!(s.spelling("docs/report.pdf"), "Docs/Report.pdf");
        assert_eq!(s.spelling("mounted"), "Mounted");
        assert_eq!(s.spelling("notes.txt"), "Notes.txt");
        let named: Vec<String> = s.frozen_places().map(|(key, _)| s.spelling(key).to_string()).collect();
        assert_eq!(named, vec!["Docs/Report.pdf", "Mounted", "Notes.txt"]);
    }

    #[test]
    fn folding_keeps_what_the_scan_left_out_under_its_folded_key() {
        let mut s = Snapshot::new();
        s.leave_out("Music/Channel".into(), Some("Music/Channel"), "isn't a file or a folder (a socket, pipe or device)");
        s.leave_out("bad\u{fffd}name".into(), None, "its name isn't valid Unicode, so Charon can't carry it");
        s.reserve(".charon-trash", ".charon-trash");
        s.fold();
        assert!(s.leaves_alone("music/channel"));
        assert!(s.leaves_alone(".charon-trash"));
        assert!(!s.leaves_alone("music"));
        assert!(s.is_occupied("music"), "the folder around a left-out item stays");
        assert_eq!(s.skipped()[0].path, "Music/Channel", "the note keeps the name as written");
        assert_eq!(s.reserved().collect::<Vec<_>>(), vec![(&".charon-trash".to_string(), &".charon-trash".to_string())]);
    }

    #[test]
    fn frozen_covers_everything_inside() {
        let mut s = Snapshot::new();
        s.freeze("a/b", "permission denied");
        assert_eq!(s.frozen("a/b/c.txt"), Some("permission denied"));
        assert_eq!(s.frozen("a/b"), Some("permission denied"));
        assert_eq!(s.frozen("a"), None);
        assert!(s.is_occupied("a"), "the folder around an unreadable place can't be removed");
    }

    #[test]
    fn inside_lists_only_the_folder_contents() {
        let s: Snapshot = ["a", "a/x", "a/y/z", "a-b", "ab"].iter().map(|p| (p.to_string(), Entry::dir())).collect();
        let inside: Vec<&str> = s.inside("a").map(|(p, _)| p.as_str()).collect();
        assert_eq!(inside, vec!["a/x", "a/y/z"]);
    }

    #[test]
    fn stored_form_is_the_bare_map() {
        let mut s = Snapshot::new();
        s.insert("a.txt", Entry::file(1, 2));
        s.freeze("x", "nope");
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(json, r#"{"a.txt":{"kind":"file","size":1,"modified":2}}"#);
        let back: Snapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.len(), 1);
    }
}
