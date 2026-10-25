//! Reading a folder into a snapshot.
//!
//! * `clients.rs` – which folders Nextcloud and ownCloud clients sync (`ClientSettings`)
//! * `cloud.rs`   – the placeholders cloud services leave for files they free up (`Placeholder`)
//! * `entry.rs`   – one file, folder or link (`Entry`) and a whole folder (`Snapshot`)
//! * `contents.rs` – comparing two files byte for byte (`Contents`)
//! * `filter.rs`  – exclude patterns (`ExcludeFilter`) and system clutter
//! * `keys.rs`    – paths as Charon compares them (`Keys`: NFC, `/`-separated)
//! * `root.rs`    – which folder a path really is (`RootId`: drive and folder number)
//! * `scanner.rs` – walking a folder (`Scanner`), noting every place it couldn't read
//! * `volume.rs`  – what kind of drive a folder is on and what it can hold (`Volume`)

mod clients;
mod cloud;
mod contents;
mod entry;
mod filter;
mod keys;
mod root;
mod scanner;
mod volume;

pub use cloud::{is_sync_journal, CloudService, Placeholder};
pub use contents::Contents;
pub use entry::{CharonFolder, Entry, EntryKind, ScanFacts, Skipped, Snapshot, MTIME_TOLERANCE_MS};
pub use filter::{
    is_charon_folder_in_other_case, is_claim, is_clutter, is_legacy_partial, is_partial, is_system_folder, new_partial_name, partial_started, ExcludeFilter,
    CLAIM_MODIFIED_MS, MARKER_FOLDER, PARTIAL_EXTENSION, PARTIAL_PREFIX, PARTIAL_SUFFIX, TRASH_FOLDER,
};
pub use keys::Keys;
pub use root::{Place, RootId};
pub use scanner::{Scanner, PRIVACY_REFUSAL};
pub use volume::{never_download_on_this_thread, FsKind, Refusal, Volume};
#[cfg(test)]
pub(crate) use volume::pretend;
