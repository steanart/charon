//! The placeholders cloud services leave in place of the files they free up.
//!
//! iCloud Drive before macOS 14 turns `Report.pdf` into a hidden `.Report.pdf.icloud`, and
//! the virtual files of Nextcloud and ownCloud are `Report.pdf.nextcloud` and
//! `Report.pdf.owncloud`. The file itself is then missing from its folder, though it's
//! still there, in the cloud: missed, it would look removed.
//!
//! A file named so is a placeholder only where its service keeps files: in iCloud Drive's
//! folders on this Mac, and in a folder a Nextcloud or ownCloud client syncs. That's a folder
//! the clients' settings on this computer list (see `scan::clients`), or one that holds a
//! client's journal of the folder it syncs, `.sync_0123456789ab.db`, in it or above it: a
//! client on another computer may sync it (a drive that goes from one computer to another).
//! Anywhere else (a backup drive, a network share) it's a copy of a placeholder, made by a
//! tool that copied a cloud folder as it was, and an ordinary file: the file it names is
//! synced there like any other. Where Charon can't tell, it's taken for a placeholder. And
//! where a service keeps files, Charon never makes a file named like its placeholder: the
//! service would take it for its own, and the file it names for freed up.
//!
//! A copy of a whole synced folder holds a copy of the journal too, and Charon can't tell it
//! from a folder another computer's client syncs. So where only a journal says a client
//! syncs a folder, the note about each file left alone there names the journal: if no
//! client syncs that folder, the journal and the placeholder are old copies, for the person
//! to remove. A journal is the client's own and never synced or removed, wherever it is:
//! Charon never makes such a copy itself.

use crate::scan::clients::ClientSettings;
use crate::scan::Keys;
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// A cloud service that leaves placeholders in place of the files it frees up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloudService {
    /// iCloud Drive before macOS 14.
    ICloud,
    /// The Nextcloud and ownCloud clients, with virtual files turned on.
    SyncClient,
}

impl CloudService {
    /// The service's name, for a note.
    pub fn name(self) -> &'static str {
        match self {
            CloudService::ICloud => "iCloud Drive",
            CloudService::SyncClient => "Nextcloud or ownCloud",
        }
    }
}

/// A cloud service's placeholder for a file it freed up, told by its name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placeholder<'a> {
    /// The name of the file it stands for.
    pub item: &'a str,
    /// The service that names its placeholders so.
    pub service: CloudService,
}

impl<'a> Placeholder<'a> {
    /// The placeholder named `name`, if a cloud service names its placeholders so.
    pub fn named(name: &'a str) -> Option<Self> {
        let (item, service) = match name.strip_prefix('.').and_then(|hidden| hidden.strip_suffix(".icloud")) {
            Some(item) => (item, CloudService::ICloud),
            None => (name.strip_suffix(".nextcloud").or_else(|| name.strip_suffix(".owncloud"))?, CloudService::SyncClient),
        };
        (!matches!(item, "" | "." | "..")).then_some(Self { item, service })
    }

    /// The size of the file this placeholder stands for, as the placeholder at `path` says:
    /// iCloud Drive writes it into its placeholders, a property list of a few hundred bytes
    /// (under `NSURLFileSizeKey`). `None` when that can't be read, and for Nextcloud and
    /// ownCloud, whose placeholders say nothing about the file but its time.
    pub fn recorded_size(&self, path: &Path) -> Option<u64> {
        const MOST: u64 = 64 * 1024;
        // Only a file is read, never through a link (a link to a pipe would never answer).
        if self.service != CloudService::ICloud || !fs::symlink_metadata(path).ok()?.is_file() {
            return None;
        }
        let file = fs::File::open(path).ok()?;
        if file.metadata().ok()?.len() > MOST {
            return None;
        }
        let list = plist::Value::from_reader(io::BufReader::new(file)).ok()?;
        list.as_dictionary()?.get("NSURLFileSizeKey")?.as_unsigned_integer()
    }
}

/// Whether `name` is a Nextcloud or ownCloud client's journal of the folder it syncs, or
/// one of the files that go with it: `.sync_0123456789ab.db` (`._sync_0123456789ab.db`, or
/// `.csync_journal.db` from older clients), with its `-wal`, `-shm` or `-journal`.
pub fn is_sync_journal(name: &str) -> bool {
    let journal = ["-wal", "-shm", "-journal"].iter().find_map(|companion| name.strip_suffix(companion)).unwrap_or(name);
    journal == ".csync_journal.db" || CloudFolders::is_journal(journal)
}

/// The folders inside one scanned folder where cloud services keep files, by key (`""` for
/// the whole folder), with the service: there, a file named like the service's placeholders
/// is its own.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct CloudPlaces(Vec<(String, CloudService)>);

impl CloudPlaces {
    /// Whether `service` keeps files in the folder `folder` (by key).
    pub fn keep(&self, folder: &str, service: CloudService) -> bool {
        self.0.iter().any(|(place, kept)| *kept == service && (place.is_empty() || Keys::is_within(folder, place)))
    }

    /// The cloud service that would take an item at `key` for its placeholder: the item is
    /// named like one, in a folder where that service keeps files.
    pub fn service_taking(&self, key: &str) -> Option<CloudService> {
        let placeholder = Placeholder::named(Keys::name(key))?;
        self.keep(Keys::parent(key).unwrap_or(""), placeholder.service).then_some(placeholder.service)
    }

    /// The same places, for keys that ignore upper and lower case (see `Keys::fold`).
    pub fn fold(&mut self) {
        for (place, _) in &mut self.0 {
            *place = Keys::fold(place);
        }
    }

    fn add(&mut self, folder: String, service: CloudService) {
        if !self.0.iter().any(|(place, kept)| *place == folder && *kept == service) {
            self.0.push((folder, service));
        }
    }
}

/// Finds out where cloud services keep files in one folder Charon scans, and so which files
/// named like their placeholders there are their own.
pub(crate) struct CloudFolders {
    /// The folder's real path, `None` when it can't be found.
    real: Option<PathBuf>,
    /// Where the services keep files as far as this computer tells (iCloud Drive's folders,
    /// the clients' settings), and where Charon can't tell.
    places: CloudPlaces,
    /// The folders (by key) a sync client syncs only as far as a client's journal says, with
    /// that journal's name.
    journaled: Vec<(String, String)>,
    /// The folders looked into for a sync client's journal, by real path, and what was found.
    journals: HashMap<PathBuf, Journal>,
}

/// What a folder holds of a sync client's journal.
#[derive(Debug, Clone, PartialEq)]
enum Journal {
    /// It holds none.
    Absent,
    /// The journal of this name.
    Present(String),
    /// The folder can't be read: it may hold one.
    Unknown,
}

impl CloudFolders {
    /// Where cloud services keep files in the folder `root`, as far as can be told before
    /// looking inside it, with the sync clients' settings in `clients`. The folders they list
    /// that `keep_off` names are taken as written (see `ClientSettings::synced_folders`).
    pub fn of(root: &Path, clients: &ClientSettings, keep_off: &dyn Fn(&Path) -> bool) -> Self {
        Self::with(root, Self::icloud_folders(), clients.synced_folders(keep_off))
    }

    /// `of`, with iCloud Drive's folders on this computer, and the folders sync clients sync,
    /// by their real paths. `None`: they can't be found, so any folder may be one of iCloud
    /// Drive's, and only a sync client's journal tells a folder a client syncs.
    fn with(root: &Path, icloud: Option<Vec<PathBuf>>, clients: Option<Vec<PathBuf>>) -> Self {
        let real = fs::canonicalize(root).ok();
        let mut folders = Self { real: real.clone(), places: CloudPlaces::default(), journaled: Vec::new(), journals: HashMap::new() };
        folders.add_listed(icloud, CloudService::ICloud);
        if clients.is_some() {
            folders.add_listed(clients, CloudService::SyncClient);
        }
        // A folder holding a client's journal, the scanned one or one above it, is one a
        // client syncs too, though the settings here don't list it: a client on another
        // computer may sync it (a drive that goes from one computer to another).
        match real {
            Some(real) => {
                for folder in real.ancestors() {
                    if folders.synced_by_journal(folder, "") {
                        break;
                    }
                }
            }
            None => folders.places.add(String::new(), CloudService::SyncClient),
        }
        folders
    }

    /// Notes that `service` keeps files in the folders `listed` (by real path) that are the
    /// scanned folder, above it or inside it. `None`: any folder may be one.
    fn add_listed(&mut self, listed: Option<Vec<PathBuf>>, service: CloudService) {
        match (&self.real, listed) {
            (_, Some(listed)) if listed.is_empty() => {}
            (Some(real), Some(listed)) => {
                for place in listed {
                    if real.starts_with(&place) {
                        self.places.add(String::new(), service);
                    } else if let Some(inside) = place.strip_prefix(real).ok().and_then(Self::key_of) {
                        self.places.add(inside, service);
                    }
                }
            }
            _ => self.places.add(String::new(), service),
        }
    }

    /// Where cloud services keep files in the scanned folder, once every item in it was
    /// noted (see `note`).
    pub fn places(mut self) -> CloudPlaces {
        for (folder, _) in std::mem::take(&mut self.journaled) {
            self.places.add(folder, CloudService::SyncClient);
        }
        self.places
    }

    /// Notes an item the scan came to, at `disk` (relative, as spelled on disk): a sync
    /// client's journal means a client syncs the folder holding it.
    pub fn note(&mut self, disk: &str, is_file: bool) {
        let name = Keys::name(disk);
        if is_file && Self::is_journal(name) {
            self.add_journaled(Keys::normalize(Keys::parent(disk).unwrap_or("")), name);
        }
    }

    /// Whether the file at `disk` (relative, as spelled on disk), named like the placeholders
    /// of `service`, is one: the service keeps files in its folder. A sync client's journal in
    /// a folder the scan hasn't come to yet counts too.
    pub fn is_placeholder(&mut self, disk: &str, service: CloudService) -> bool {
        let folder = Keys::parent(disk).unwrap_or("");
        let key = Keys::normalize(folder);
        if self.places.keep(&key, service) || (service == CloudService::SyncClient && self.journal_over(&key).is_some()) {
            return true;
        }
        // The scanned folder itself, and the folders above it, were looked into already.
        let Some(real) = self.real.clone().filter(|_| service == CloudService::SyncClient && !folder.is_empty()) else { return false };
        let parts: Vec<&str> = folder.split('/').collect();
        for depth in (1..=parts.len()).rev() {
            let path = parts[..depth].iter().fold(real.clone(), |path, part| path.join(part));
            if self.synced_by_journal(&path, &parts[..depth].join("/")) {
                return true;
            }
        }
        false
    }

    /// The name of the sync client's journal that alone makes the folder of the placeholder at
    /// `disk` (of `service`) one a client syncs: no client's settings on this computer list
    /// it. `None` where they do, where Charon can't tell, and for other services.
    pub fn journal_of(&self, disk: &str, service: CloudService) -> Option<&str> {
        let folder = Keys::normalize(Keys::parent(disk).unwrap_or(""));
        if service != CloudService::SyncClient || self.places.keep(&folder, service) {
            return None;
        }
        self.journal_over(&folder)
    }

    /// The journal of the closest folder (by key) at or above `folder` that a sync client
    /// syncs as far as that journal says.
    fn journal_over(&self, folder: &str) -> Option<&str> {
        let holding = self.journaled.iter().filter(|(place, _)| place.is_empty() || Keys::is_within(folder, place));
        holding.max_by_key(|(place, _)| place.len()).map(|(_, journal)| journal.as_str())
    }

    /// Whether the folder at the real path `folder`, at `disk` in the scanned folder (as
    /// spelled on disk, `""` for the scanned folder and those above it), is one a sync client
    /// syncs as far as can be told from inside it: it holds a client's journal, or it can't
    /// be read and may hold one.
    fn synced_by_journal(&mut self, folder: &Path, disk: &str) -> bool {
        match self.journal_in(folder) {
            Journal::Absent => false,
            Journal::Present(journal) => {
                self.add_journaled(Keys::normalize(disk), &journal);
                true
            }
            Journal::Unknown => {
                self.places.add(Keys::normalize(disk), CloudService::SyncClient);
                true
            }
        }
    }

    /// What the folder at the real path `folder` holds of a sync client's journal.
    fn journal_in(&mut self, folder: &Path) -> Journal {
        if let Some(found) = self.journals.get(folder) {
            return found.clone();
        }
        let found = match fs::read_dir(folder) {
            Ok(items) => {
                let journal = items.flatten().find(|item| item.file_name().to_str().is_some_and(Self::is_journal) && item.file_type().is_ok_and(|kind| kind.is_file()));
                journal.and_then(|item| item.file_name().into_string().ok()).map_or(Journal::Absent, Journal::Present)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Journal::Absent,
            Err(_) => Journal::Unknown,
        };
        self.journals.insert(folder.to_path_buf(), found.clone());
        found
    }

    /// Notes that the journal named `journal` makes the folder `folder` (by key) one a sync
    /// client syncs.
    fn add_journaled(&mut self, folder: String, journal: &str) {
        if !self.journaled.iter().any(|(place, _)| *place == folder) {
            self.journaled.push((folder, journal.to_string()));
        }
    }

    /// Whether `name` is the journal a Nextcloud or ownCloud client keeps at the top of the
    /// folder it syncs: `.sync_0123456789ab.db`, or `._sync_0123456789ab.db`.
    fn is_journal(name: &str) -> bool {
        let id = name.strip_prefix(".sync_").or_else(|| name.strip_prefix("._sync_")).and_then(|rest| rest.strip_suffix(".db"));
        id.is_some_and(|id| !id.is_empty() && id.bytes().all(|digit| digit.is_ascii_hexdigit()))
    }

    /// The key of a relative path, `None` if part of it isn't valid Unicode.
    fn key_of(path: &Path) -> Option<String> {
        let parts: Option<Vec<&str>> = path.components().map(|part| part.as_os_str().to_str()).collect();
        Some(Keys::normalize(&parts?.join("/")))
    }

    /// iCloud Drive's folders on this Mac, by their real paths: `~/Library/Mobile Documents`
    /// (iCloud Drive, and the folders of apps that keep their documents in iCloud), and the
    /// Desktop and Documents folders, which iCloud Drive can keep too.
    #[cfg(target_os = "macos")]
    fn icloud_folders() -> Option<Vec<PathBuf>> {
        let home = std::env::home_dir()?;
        let folders = ["Library/Mobile Documents", "Desktop", "Documents"].iter().map(|folder| home.join(folder));
        Some(folders.map(|folder| fs::canonicalize(&folder).unwrap_or(folder)).collect())
    }

    /// Only a Mac's iCloud Drive leaves placeholders named so.
    #[cfg(not(target_os = "macos"))]
    fn icloud_folders() -> Option<Vec<PathBuf>> {
        Some(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "x").unwrap();
    }

    /// Whether the file at `disk`, named like a placeholder, is one there.
    fn is_placeholder(folders: &mut CloudFolders, disk: &str) -> bool {
        let placeholder = Placeholder::named(Keys::name(disk)).expect("named like a placeholder");
        folders.is_placeholder(disk, placeholder.service)
    }

    #[test]
    fn a_cloud_services_placeholder_names_the_file_it_stands_for() {
        let named = |name| Placeholder::named(name).map(|placeholder| (placeholder.item, placeholder.service));
        assert_eq!(named(".Report.pdf.icloud"), Some(("Report.pdf", CloudService::ICloud)));
        assert_eq!(named("..env.icloud"), Some((".env", CloudService::ICloud)));
        assert_eq!(named("Plans.odt.nextcloud"), Some(("Plans.odt", CloudService::SyncClient)));
        assert_eq!(named("Budget.xlsx.owncloud"), Some(("Budget.xlsx", CloudService::SyncClient)));
        assert_eq!(named(".Notes.txt.nextcloud"), Some((".Notes.txt", CloudService::SyncClient)));
        assert_eq!(named("Report.pdf.icloud"), None, "iCloud's placeholders are hidden");
        assert_eq!(named(".icloud"), None);
        assert_eq!(named("..icloud"), None);
        assert_eq!(named("...icloud"), None);
        assert_eq!(named(".nextcloud"), None);
        assert_eq!(named("Report.pdf"), None);
    }

    /// iCloud Drive leaves its placeholders only in its own folders. A copy of one anywhere
    /// else (a backup made with a tool that copied the folder as it was) is an ordinary file.
    #[test]
    fn icloud_placeholders_are_only_in_icloud_drives_folders() {
        let tmp = tempfile::tempdir().unwrap();
        let home = fs::canonicalize(tmp.path()).unwrap();
        let (drive, backup) = (home.join("Library/Mobile Documents/com~apple~CloudDocs"), home.join("Backup"));
        fs::create_dir_all(&drive).unwrap();
        fs::create_dir_all(&backup).unwrap();
        let icloud = || Some(vec![home.join("Library/Mobile Documents"), home.join("Documents")]);

        let mut inside = CloudFolders::with(&drive, icloud(), None);
        assert!(is_placeholder(&mut inside, "Docs/.Report.pdf.icloud"));
        assert!(is_placeholder(&mut inside, ".Plans.key.icloud"));
        assert!(!is_placeholder(&mut inside, "Docs/Report.pdf.nextcloud"), "no sync client keeps files here");
        let mut elsewhere = CloudFolders::with(&backup, icloud(), None);
        assert!(!is_placeholder(&mut elsewhere, "Docs/.Report.pdf.icloud"));
        assert!(elsewhere.places().0.is_empty());
        let mut above = CloudFolders::with(&home, icloud(), None);
        assert!(is_placeholder(&mut above, "Library/Mobile Documents/com~apple~CloudDocs/.Report.pdf.icloud"));
        assert!(is_placeholder(&mut above, "Documents/Taxes/.2025.pdf.icloud"));
        assert!(!is_placeholder(&mut above, "Backup/.Report.pdf.icloud"));
        assert!(!is_placeholder(&mut above, ".Report.pdf.icloud"));
        assert_eq!(above.places().0, vec![("Library/Mobile Documents".to_string(), CloudService::ICloud), ("Documents".to_string(), CloudService::ICloud)]);
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&drive, home.join("Shortcut")).unwrap();
            let mut through_a_link = CloudFolders::with(&home.join("Shortcut"), icloud(), None);
            assert!(is_placeholder(&mut through_a_link, ".Report.pdf.icloud"), "told by the real path");
        }
        // Where iCloud Drive's folders can't be found, any folder may be one.
        let mut unknown = CloudFolders::with(&backup, None, None);
        assert!(is_placeholder(&mut unknown, ".Report.pdf.icloud"));
        // Where there is no iCloud Drive, nothing is its placeholder.
        let mut none = CloudFolders::with(&drive, Some(Vec::new()), None);
        assert!(!is_placeholder(&mut none, ".Report.pdf.icloud"));
    }

    /// A Nextcloud or ownCloud client keeps its journal at the top of the folder it syncs:
    /// above the scanned folder, in it, or in a folder inside it, which the scan may come to
    /// after the placeholders in it. Where no client's settings can be read, that's how a
    /// folder a client syncs is told, and the journal is named for the notes.
    #[test]
    fn where_no_clients_settings_can_be_read_sync_client_placeholders_are_in_folders_with_a_journal() {
        let tmp = tempfile::tempdir().unwrap();
        let none = || Some(Vec::new());
        write(&tmp.path().join("Nextcloud/.sync_0123456789ab.db"));
        write(&tmp.path().join("Inside/ownCloud/._sync_ba9876543210.db"));
        fs::create_dir_all(tmp.path().join("Nextcloud/Docs")).unwrap();
        fs::create_dir_all(tmp.path().join("Inside/ownCloud/Docs")).unwrap();
        // Named like a journal, but a folder.
        fs::create_dir_all(tmp.path().join("Backup/Docs/.sync_0123456789ab.db")).unwrap();

        let mut below = CloudFolders::with(&tmp.path().join("Nextcloud/Docs"), none(), None);
        assert!(is_placeholder(&mut below, "Plans.odt.nextcloud"));
        assert!(is_placeholder(&mut below, "Old/Budget.xlsx.owncloud"));
        assert_eq!(below.journal_of("Old/Budget.xlsx.owncloud", CloudService::SyncClient), Some(".sync_0123456789ab.db"));
        assert_eq!(below.places().0, vec![(String::new(), CloudService::SyncClient)]);
        let mut copy = CloudFolders::with(&tmp.path().join("Backup"), none(), None);
        assert!(!is_placeholder(&mut copy, "Docs/Plans.odt.nextcloud"));
        assert!(copy.places().0.is_empty());

        let mut above = CloudFolders::with(&tmp.path().join("Inside"), none(), None);
        assert!(is_placeholder(&mut above, "ownCloud/Docs/Plans.odt.owncloud"), "before the scan comes to the journal");
        assert!(!is_placeholder(&mut above, "Plans.odt.owncloud"));
        above.note("ownCloud/._sync_ba9876543210.db", true);
        assert_eq!(above.places().0, vec![("ownCloud".to_string(), CloudService::SyncClient)]);
        let mut noted = CloudFolders::with(&tmp.path().join("Inside"), none(), None);
        noted.note("ownCloud/._sync_ba9876543210.db", true);
        noted.note("Other/.sync_.db", true);
        noted.note("Other/.sync_0123456789ab.db", false);
        noted.note("Other/.sync_notanumber.db", true);
        assert_eq!(noted.places().0, vec![("ownCloud".to_string(), CloudService::SyncClient)]);
    }

    /// Where the clients' settings can be read, they tell which folders a client syncs: at,
    /// above or inside the scanned folder, by real path, with or without a journal. A folder
    /// they don't list counts too when it holds a client's journal, in it or above it: a
    /// client on another computer may sync it. Charon can't tell that from a copy of a synced
    /// folder (a backup made by a tool that copied it as it was), so the journal is named for
    /// the notes, the closest one. A folder with neither holds no placeholders.
    #[test]
    fn sync_client_placeholders_are_in_the_folders_the_settings_list_or_a_journal_marks() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp = fs::canonicalize(tmp.path()).unwrap();
        let none = || Some(Vec::new());
        let clients = || Some(vec![tmp.join("Nextcloud"), tmp.join("Fresh")]);
        write(&tmp.join("Nextcloud/.sync_0123456789ab.db"));
        fs::create_dir_all(tmp.join("Nextcloud/Docs")).unwrap();
        // Listed, but the client hasn't written its journal yet.
        fs::create_dir_all(tmp.join("Fresh")).unwrap();
        write(&tmp.join("Backup/.sync_0123456789ab.db"));
        write(&tmp.join("Backup/Docs/._sync_ba9876543210.db"));
        fs::create_dir_all(tmp.join("Plain/Docs")).unwrap();

        for listed in ["Nextcloud/Docs", "Fresh"] {
            let mut folders = CloudFolders::with(&tmp.join(listed), none(), clients());
            assert!(is_placeholder(&mut folders, "Plans.odt.nextcloud"), "{listed}");
            assert_eq!(folders.journal_of("Plans.odt.nextcloud", CloudService::SyncClient), None, "{listed}: the settings say so");
            assert_eq!(folders.places().0, vec![(String::new(), CloudService::SyncClient)], "{listed}");
        }
        let mut journaled = CloudFolders::with(&tmp.join("Backup"), none(), clients());
        assert!(is_placeholder(&mut journaled, "Plans.odt.nextcloud"));
        assert!(is_placeholder(&mut journaled, "Docs/Old/Plans.odt.nextcloud"));
        assert_eq!(journaled.journal_of("Plans.odt.nextcloud", CloudService::SyncClient), Some(".sync_0123456789ab.db"));
        journaled.note("Docs/._sync_ba9876543210.db", true);
        assert_eq!(journaled.journal_of("Docs/Old/Plans.odt.nextcloud", CloudService::SyncClient), Some("._sync_ba9876543210.db"));
        assert_eq!(journaled.journal_of("Docs/.Plans.key.icloud", CloudService::ICloud), None);
        assert_eq!(journaled.places().0, vec![(String::new(), CloudService::SyncClient), ("Docs".to_string(), CloudService::SyncClient)]);

        let mut above = CloudFolders::with(&tmp, none(), clients());
        assert!(is_placeholder(&mut above, "Nextcloud/Docs/Plans.odt.nextcloud"));
        assert!(is_placeholder(&mut above, "Backup/Docs/Plans.odt.nextcloud"), "before the scan comes to the journal");
        assert!(!is_placeholder(&mut above, "Plain/Docs/Plans.odt.nextcloud"));
        assert_eq!(above.journal_of("Nextcloud/Docs/Plans.odt.nextcloud", CloudService::SyncClient), None);
        assert_eq!(above.journal_of("Backup/Docs/Plans.odt.nextcloud", CloudService::SyncClient), Some("._sync_ba9876543210.db"));
        let places = [("Nextcloud", CloudService::SyncClient), ("Fresh", CloudService::SyncClient), ("Backup/Docs", CloudService::SyncClient)];
        assert_eq!(above.places().0, places.map(|(place, service)| (place.to_string(), service)));
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(tmp.join("Fresh"), tmp.join("Shortcut")).unwrap();
            let mut through_a_link = CloudFolders::with(&tmp.join("Shortcut"), none(), clients());
            assert!(is_placeholder(&mut through_a_link, "Plans.odt.nextcloud"), "told by the real path");
        }
        // Settings that list only another folder, and no journal: no client syncs this one.
        let mut elsewhere = CloudFolders::with(&tmp.join("Plain/Docs"), none(), Some(vec![tmp.join("Backup/Docs")]));
        assert!(!is_placeholder(&mut elsewhere, "Plans.odt.nextcloud"));
        // A folder whose real path can't be found may be one.
        let mut unknown = CloudFolders::with(&tmp.join("Missing"), none(), clients());
        assert!(is_placeholder(&mut unknown, "Plans.odt.nextcloud"));
        assert_eq!(unknown.journal_of("Plans.odt.nextcloud", CloudService::SyncClient), None);
    }

    /// A sync client's journal and the files that go with it are told by their names.
    #[test]
    fn a_sync_clients_journal_is_told_by_its_name() {
        let journals = [
            ".sync_0123456789ab.db",
            "._sync_BA9876543210.db",
            ".csync_journal.db",
            ".sync_0123456789ab.db-wal",
            ".sync_0123456789ab.db-shm",
            "._sync_ba9876543210.db-journal",
            ".csync_journal.db-journal",
        ];
        for name in journals {
            assert!(is_sync_journal(name), "{name}");
        }
        let others = [
            ".sync_.db",
            ".sync_notes.db",
            ".sync_0123456789ab.db-wal-shm",
            "sync_0123456789ab.db",
            ".sync_0123456789ab.db.bak",
            "notes.db-wal",
            ".csync_journal.db-wal-shm",
        ];
        for name in others {
            assert!(!is_sync_journal(name), "{name}");
        }
    }

    /// Above the scanned folder, a folder that can't be read may hold a sync client's
    /// journal: what's named like its placeholders is taken for them.
    #[cfg(unix)]
    #[test]
    fn a_folder_above_that_cant_be_read_may_be_synced_by_a_client() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let (locked, root) = (tmp.path().join("locked"), tmp.path().join("locked/job"));
        fs::create_dir_all(&root).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o111)).unwrap();
        let readable = fs::read_dir(&locked).is_ok(); // root ignores permissions
        let mut folders = CloudFolders::with(&root, Some(Vec::new()), None);
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        if readable {
            return;
        }
        assert!(is_placeholder(&mut folders, "Plans.odt.nextcloud"));
    }
}
