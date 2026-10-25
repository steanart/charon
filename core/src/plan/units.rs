//! Files that only make sense together, and must never be mixed from two versions.
//!
//! * An app's library or document package: a Photos or Final Cut library, a Logic
//!   project, a Pages document, a disk image in bands… On macOS it looks like one file,
//!   but it's a folder of many, written together by one app.
//! * An SQLite database and its journal files (`name-wal`, `name-shm`, `name-journal`):
//!   the journal holds changes that only fit the database it was written for.
//!
//! When such a unit changed on both sides of a two-way job, syncing some of its files one
//! way and others the other way would leave a mix that the app may not open, or would
//! quietly damage. So the whole unit becomes one conflict, and the person chooses whose
//! version to keep as a whole (see `plan::Resolution`). A run carries a unit's files
//! together too: all of them, or none when anything of the unit changed meanwhile, and
//! what it no longer has goes only once all its new versions have landed (see
//! `run::Executor`). What only adds to a library (a new file that isn't a database's) lands
//! even when the rest of it waits: the version there goes on as it was, with an item it
//! doesn't use. A library the receiving side doesn't have yet has no version there to mix
//! with: its files go one by one, and only a database in it goes with its journals.

use crate::scan::{Entry, Keys, Snapshot};

/// The endings of folders that hold one app's library or document.
const PACKAGES: &[&str] = &[
    "app",
    "aplibrary",
    "band",
    "dtbase2",
    "fcpbundle",
    "imovielibrary",
    "key",
    "logicx",
    "lrlibrary",
    "musiclibrary",
    "numbers",
    "pages",
    "photolibrary",
    "photoslibrary",
    "rtfd",
    "scriv",
    "sparsebundle",
    "tvlibrary",
];

/// The journal files SQLite keeps next to a database.
const JOURNALS: &[&str] = &["-wal", "-shm", "-journal"];

/// A group of files that only make sense together, named by its key.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Unit {
    /// An app's library or document package: the key of its folder.
    Package(String),
    /// An SQLite database: the key of the database file (its journals are named after it).
    Database(String),
}

impl Unit {
    /// The unit `key` belongs to in a job whose two sides are `source` and `destination`,
    /// if any. A package is a folder, and a database a file with a journal next to it: an
    /// item that only has such a name is an ordinary one.
    pub fn of(key: &str, source: &Snapshot, destination: &Snapshot) -> Option<Unit> {
        if let Some(package) = Self::package_of(key, source, destination) {
            return Some(Unit::Package(package.to_string()));
        }
        Self::database_of(key, source, destination).map(Unit::Database)
    }

    /// The database `key` is a file of (the database itself or one of its journals), even
    /// inside an app library: the key of the database file. A database is only one with a
    /// journal next to it, and a journal only one next to its database (a note called
    /// "travel-journal" alone is an ordinary file). Both are files: a folder "notes" next to
    /// a file "notes-journal" is no database, and neither is a file next to a folder.
    pub fn database_of(key: &str, source: &Snapshot, destination: &Snapshot) -> Option<String> {
        if Self::journal_ending(key).is_some() {
            return Self::journal_of(key, source, destination);
        }
        let journaled = JOURNALS.iter().any(|journal| Self::a_file(&format!("{key}{journal}"), source, destination));
        (journaled && Self::no_other_kind(key, source, destination, Entry::is_file)).then(|| key.to_string())
    }

    /// The database `key` is a journal of, if it is one: a file named like one, next to its
    /// database (see `database_of`). The key of the database file.
    pub fn journal_of(key: &str, source: &Snapshot, destination: &Snapshot) -> Option<String> {
        let database = &key[..key.len() - Self::journal_ending(key)?.len()];
        (Self::a_file(database, source, destination) && Self::no_other_kind(key, source, destination, Entry::is_file)).then(|| database.to_string())
    }

    /// The journal ending of `key`'s name, if it has one (a name that is only an ending has
    /// none).
    fn journal_ending(key: &str) -> Option<&'static str> {
        let name = key.rsplit('/').next().unwrap_or(key);
        JOURNALS.iter().copied().find(|journal| name.len() > journal.len() && name.ends_with(journal))
    }

    /// The key that stands for the whole unit (the package folder, or the database file).
    pub fn key(&self) -> &str {
        match self {
            Unit::Package(key) | Unit::Database(key) => key,
        }
    }

    /// Whether `key` is part of this unit.
    pub fn holds(&self, key: &str) -> bool {
        match self {
            Unit::Package(folder) => Keys::is_within(key, folder),
            Unit::Database(database) => key == database || JOURNALS.iter().any(|journal| key.strip_prefix(database.as_str()) == Some(journal)),
        }
    }

    /// The keys of a database's files (the database and its journals).
    pub fn database_files(database: &str) -> Vec<String> {
        std::iter::once(database.to_string()).chain(JOURNALS.iter().map(|journal| format!("{database}{journal}"))).collect()
    }

    /// "an app library" or "a database", for messages.
    pub fn describe(&self) -> &'static str {
        match self {
            Unit::Package(_) => "an app library or document",
            Unit::Database(_) => "a database",
        }
    }

    /// The topmost folder above or at `key` that is a package: named like one, and a folder
    /// on each side that has it.
    fn package_of<'k>(key: &'k str, source: &Snapshot, destination: &Snapshot) -> Option<&'k str> {
        let mut end = 0;
        for part in key.split('/') {
            end += part.len();
            if Self::is_package_name(part) && Self::no_other_kind(&key[..end], source, destination, Entry::is_dir) {
                return Some(&key[..end]);
            }
            end += 1; // the slash
        }
        None
    }

    fn is_package_name(name: &str) -> bool {
        match name.rsplit_once('.') {
            Some((stem, ending)) if !stem.is_empty() => PACKAGES.iter().any(|package| ending.eq_ignore_ascii_case(package)),
            _ => false,
        }
    }

    /// Whether a side has a file at `key`, and neither has anything else there.
    fn a_file(key: &str, source: &Snapshot, destination: &Snapshot) -> bool {
        (source.get(key).is_some_and(Entry::is_file) || destination.get(key).is_some_and(Entry::is_file))
            && Self::no_other_kind(key, source, destination, Entry::is_file)
    }

    /// Whether each side that has an item at `key` has one of the kind `wanted` says.
    fn no_other_kind(key: &str, source: &Snapshot, destination: &Snapshot, wanted: fn(&Entry) -> bool) -> bool {
        [source, destination].iter().all(|side| side.get(key).is_none_or(wanted))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(items: &[(&str, Entry)]) -> Snapshot {
        items.iter().map(|(key, entry)| (key.to_string(), *entry)).collect()
    }

    fn file() -> Entry {
        Entry::file(1, 10_000)
    }

    #[test]
    fn files_inside_a_library_belong_to_it() {
        let library = Some(Unit::Package("Pictures/Photos Library.photoslibrary".into()));
        let source = snap(&[
            ("Pictures", Entry::dir()),
            ("Pictures/Photos Library.photoslibrary", Entry::dir()),
            ("Pictures/Photos Library.photoslibrary/database", Entry::dir()),
            ("Pictures/Photos Library.photoslibrary/database/Photos.sqlite-wal", file()),
            ("Pictures/cat.jpg", file()),
            ("notes", Entry::dir()),
            ("notes/.pages", file()),
        ]);
        let destination = snap(&[("Docs", Entry::dir()), ("Docs/Report.PAGES", Entry::dir()), ("Docs/Report.PAGES/Index.zip", file())]);
        assert_eq!(Unit::of("Pictures/Photos Library.photoslibrary", &source, &destination), library);
        assert_eq!(Unit::of("Pictures/Photos Library.photoslibrary/database/Photos.sqlite-wal", &source, &destination), library);
        assert_eq!(Unit::of("Docs/Report.PAGES/Index.zip", &source, &destination), Some(Unit::Package("Docs/Report.PAGES".into())));
        assert_eq!(Unit::of("Pictures/cat.jpg", &source, &destination), None);
        assert_eq!(Unit::of("notes/.pages", &source, &destination), None, "a name that is only an ending isn't a package");
    }

    /// Pages, Numbers and Keynote save single files too, and "server.key" is a private key.
    #[test]
    fn a_file_named_like_a_package_is_an_ordinary_file() {
        let source = snap(&[("Budget.numbers", file()), ("server.key", file()), ("Talk.key", Entry::dir()), ("Talk.key/Index.zip", file())]);
        let destination = snap(&[("Budget.numbers", Entry::file(3, 20_000)), ("Talk.key", file())]);
        assert_eq!(Unit::of("Budget.numbers", &source, &destination), None);
        assert_eq!(Unit::of("server.key", &source, &destination), None);
        assert_eq!(Unit::of("server.key", &source, &Snapshot::new()), None, "on one side only, too");
        assert_eq!(Unit::of("Talk.key/Index.zip", &source, &destination), None, "a folder on one side and a file on the other is no package");
        assert_eq!(Unit::of("Talk.key/Index.zip", &source, &Snapshot::new()), Some(Unit::Package("Talk.key".into())));
    }

    #[test]
    fn a_database_and_its_journals_belong_together() {
        let database = Some(Unit::Database("app/data.db".into()));
        let source = snap(&[("app", Entry::dir()), ("app/data.db", file()), ("app/data.db-wal", file())]);
        let none = Snapshot::new();
        assert_eq!(Unit::of("app/data.db-wal", &source, &none), database);
        assert_eq!(Unit::of("app/data.db-shm", &source, &none), database);
        assert_eq!(Unit::of("app/data.db-journal", &none, &source), database);
        assert_eq!(Unit::of("app/data.db", &source, &none), database);
        let alone = snap(&[("app/data.db", file()), ("notes/travel-journal", file())]);
        assert_eq!(Unit::of("app/data.db", &alone, &none), None, "without a journal it's an ordinary file");
        assert_eq!(Unit::of("notes/travel-journal", &alone, &none), None, "without its database it's an ordinary file");
        let library = snap(&[("Lib.photoslibrary/database/Photos.sqlite", file())]);
        assert_eq!(
            Unit::database_of("Lib.photoslibrary/database/Photos.sqlite-wal", &library, &none),
            Some("Lib.photoslibrary/database/Photos.sqlite".into()),
            "inside a library too"
        );
        assert_eq!(Unit::journal_of("app/data.db-wal", &source, &none), Some("app/data.db".into()));
        assert_eq!(Unit::journal_of("app/data.db", &source, &none), None, "the database itself is no journal");
        assert_eq!(Unit::journal_of("notes/travel-journal", &alone, &none), None, "without its database it's an ordinary file");
        let unit = Unit::Database("app/data.db".into());
        assert!(unit.holds("app/data.db-wal") && unit.holds("app/data.db") && !unit.holds("app/data.db2"));
    }

    /// A folder "notes" next to a file "notes-journal", or a folder "Garden-journal" next to
    /// a folder "Garden": names that only look like a database and its journal.
    #[test]
    fn folders_are_never_a_database_or_its_journal() {
        let source = snap(&[("notes", Entry::dir()), ("notes/a.txt", file()), ("notes-journal", file()), ("Garden", Entry::dir())]);
        let destination = snap(&[("Garden-journal", Entry::dir()), ("plan", file()), ("plan-wal", Entry::dir())]);
        for key in ["notes", "notes-journal", "notes/a.txt", "Garden", "Garden-journal", "plan", "plan-wal"] {
            assert_eq!(Unit::of(key, &source, &destination), None, "{key}");
        }
        // A database that is a file on one side and a folder on the other isn't one either.
        let source = snap(&[("data.db", file()), ("data.db-wal", file())]);
        let destination = snap(&[("data.db", Entry::dir())]);
        assert_eq!(Unit::of("data.db-wal", &source, &destination), None);
        assert_eq!(Unit::of("data.db", &source, &destination), None);
    }
}
