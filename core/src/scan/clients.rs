//! Which folders the Nextcloud and ownCloud clients on this computer sync, as their own
//! settings say: the `localPath` of each folder in `nextcloud.cfg` or `owncloud.cfg`. A
//! client keeps that file in the folder for settings: `~/.config` on Linux (or the client's
//! own folder in `~/.var/app` when it's a Flatpak), `~/Library/Preferences` on macOS, and
//! `%APPDATA%` or `%LOCALAPPDATA%` on Windows.
//!
//! A client also keeps a journal at the top of each folder it syncs, so a folder holding one
//! counts as synced by a client too, though the settings here don't list it: a client on
//! another computer may sync it. A copy of a synced folder (a backup made by a tool that
//! copied it as it was) holds a copy of the journal as well, which Charon can't tell apart,
//! so the notes say when only a journal makes a folder count (see `scan::cloud`).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Each client's folder and settings file in the folder for settings, and its name as a
/// Flatpak.
const CLIENTS: [(&str, &str, &str); 2] =
    [("Nextcloud", "nextcloud.cfg", "com.nextcloud.desktopclient.nextcloud"), ("ownCloud", "owncloud.cfg", "com.owncloud.desktopclient.owncloud")];

/// The settings files of the Nextcloud and ownCloud clients.
#[derive(Debug)]
pub(crate) struct ClientSettings(Vec<PathBuf>);

impl ClientSettings {
    /// Where the clients keep their settings on this computer.
    pub fn here() -> Self {
        Self(Self::files())
    }

    /// The settings files at `files`.
    #[cfg(test)]
    pub fn at(files: Vec<PathBuf>) -> Self {
        Self(files)
    }

    /// The folders the clients sync, by their real paths, but for those `keep_off` names, which
    /// are as written: their drive isn't asked (see `Scanner::keeping_off`). `None` when
    /// Charon can't tell: no client's settings can be read, or none lists a folder, or one
    /// client's settings are there but can't be read.
    pub fn synced_folders(&self, keep_off: &dyn Fn(&Path) -> bool) -> Option<Vec<PathBuf>> {
        let mut folders = Vec::new();
        for file in &self.0 {
            match fs::read(file) {
                Ok(text) => folders.extend(Self::local_paths(&text)?),
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => {
                    log::debug!("couldn't read the settings {}: {e}", file.display());
                    return None;
                }
            }
        }
        let real = |folder: PathBuf| if keep_off(&folder) { folder } else { fs::canonicalize(&folder).unwrap_or(folder) };
        (!folders.is_empty()).then(|| folders.into_iter().map(real).collect())
    }

    /// The settings files in the folders for settings on this computer.
    fn files() -> Vec<PathBuf> {
        let folders = Self::settings_folders();
        let in_folders = folders.iter().flat_map(|folder| CLIENTS.iter().map(move |(app, file, _)| folder.join(app).join(file)));
        in_folders.chain(Self::flatpak_files()).collect()
    }

    /// Each client's settings in its own folder for settings, when it's a Flatpak.
    #[cfg(not(any(windows, target_os = "macos")))]
    fn flatpak_files() -> Vec<PathBuf> {
        let Some(home) = std::env::home_dir() else { return Vec::new() };
        CLIENTS.iter().map(|(app, file, flatpak)| home.join(".var/app").join(flatpak).join("config").join(app).join(file)).collect()
    }

    /// Windows and macOS have no Flatpaks.
    #[cfg(any(windows, target_os = "macos"))]
    fn flatpak_files() -> Vec<PathBuf> {
        Vec::new()
    }

    #[cfg(windows)]
    fn settings_folders() -> Vec<PathBuf> {
        ["APPDATA", "LOCALAPPDATA"].into_iter().filter_map(std::env::var_os).map(PathBuf::from).collect()
    }

    #[cfg(target_os = "macos")]
    fn settings_folders() -> Vec<PathBuf> {
        std::env::home_dir().map(|home| home.join("Library/Preferences")).into_iter().collect()
    }

    #[cfg(not(any(windows, target_os = "macos")))]
    fn settings_folders() -> Vec<PathBuf> {
        let configured = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).filter(|folder| folder.is_absolute());
        configured.or_else(|| std::env::home_dir().map(|home| home.join(".config"))).into_iter().collect()
    }

    /// The folders listed in the text of a client's settings file: the value of every
    /// `localPath`, written the way Qt writes it (see `unescape`). `None` if one isn't text.
    fn local_paths(text: &[u8]) -> Option<Vec<PathBuf>> {
        let text = text.strip_prefix(b"\xef\xbb\xbf").unwrap_or(text);
        let mut folders = Vec::new();
        for line in text.split(|&byte| byte == b'\n') {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if line.trim_ascii_start().starts_with(b";") {
                continue;
            }
            let Some(equals) = line.iter().position(|&byte| byte == b'=') else { continue };
            let key = line[..equals].trim_ascii();
            if key == b"localPath" || key.ends_with(b"\\localPath") {
                folders.extend(Self::unescape(&line[equals + 1..])?.into_iter().map(PathBuf::from).filter(|folder| folder.is_absolute()));
            }
        }
        Some(folders)
    }

    /// The strings in a value of a settings file, the way Qt writes them: spaces around the
    /// value left out, `"` around what keeps them, `\` before a character written another
    /// way (`\x` and hex digits, or octal digits, for one UTF-16 code unit), and `,` between
    /// the strings of a list. A `;` outside quotes begins a comment. `None` if it isn't text.
    fn unescape(value: &[u8]) -> Option<Vec<String>> {
        let mut strings = Vec::new();
        let mut units: Vec<u16> = Vec::new();
        let (mut quoted, mut was_quoted) = (false, false);
        // Spaces at the end are left out back to here, but not those written as escapes.
        let mut kept = 0;
        let mut at = Self::after_spaces(value, 0);
        while let Some(&byte) = value.get(at) {
            match byte {
                b'\\' => {
                    let Some(&code) = value.get(at + 1) else { break };
                    at += 2;
                    match code {
                        b'x' => {
                            let digits = value[at..].iter().take_while(|digit| digit.is_ascii_hexdigit()).count();
                            if digits > 0 {
                                units.push(Self::number(&value[at..at + digits], 16));
                            }
                            at += digits;
                        }
                        b'0'..=b'7' => {
                            let digits = value[at..].iter().take_while(|digit| matches!(digit, b'0'..=b'7')).count();
                            units.push(Self::number(&value[at - 1..at + digits], 8));
                            at += digits;
                        }
                        _ => units.extend(Self::escaped(code)),
                    }
                    kept = units.len();
                }
                b'"' => {
                    at += 1;
                    was_quoted = true;
                    quoted = !quoted;
                    if !quoted {
                        at = Self::after_spaces(value, at);
                        kept = units.len();
                    }
                }
                b';' if !quoted => break,
                b',' if !quoted => {
                    strings.push(Self::string(&mut units, was_quoted, kept)?);
                    was_quoted = false;
                    kept = 0;
                    at = Self::after_spaces(value, at + 1);
                }
                _ => {
                    let end = value[at + 1..].iter().position(|byte| matches!(byte, b'\\' | b'"' | b',' | b';')).map_or(value.len(), |found| at + 1 + found);
                    units.extend(std::str::from_utf8(&value[at..end]).ok()?.encode_utf16());
                    at = end;
                }
            }
        }
        strings.push(Self::string(&mut units, was_quoted, kept)?);
        Some(strings)
    }

    /// The character written as `\` and `code`, `None` for one Qt leaves out.
    fn escaped(code: u8) -> Option<u16> {
        let character = match code {
            b'a' => 0x07,
            b'b' => 0x08,
            b'f' => 0x0c,
            b'n' => b'\n',
            b'r' => b'\r',
            b't' => b'\t',
            b'v' => 0x0b,
            b'"' | b'?' | b'\'' | b'\\' => code,
            _ => return None,
        };
        Some(character.into())
    }

    /// The UTF-16 code unit written as `digits` in `radix`, as Qt reads it.
    fn number(digits: &[u8], radix: u16) -> u16 {
        digits.iter().fold(0, |unit: u16, &digit| unit.wrapping_mul(radix).wrapping_add((digit as char).to_digit(radix.into()).unwrap_or(0) as u16))
    }

    /// Where the spaces and tabs from `at` on end.
    fn after_spaces(value: &[u8], at: usize) -> usize {
        at + value[at..].iter().take_while(|byte| matches!(byte, b' ' | b'\t')).count()
    }

    /// The string read into `units`, which are emptied: the spaces at its end left out, back
    /// to `kept`, unless it was in quotes.
    fn string(units: &mut Vec<u16>, quoted: bool, kept: usize) -> Option<String> {
        if !quoted {
            while units.len() > kept && units.last().is_some_and(|&unit| unit == u16::from(b' ') || unit == u16::from(b'\t')) {
                units.pop();
            }
        }
        let string = String::from_utf16(units).ok();
        units.clear();
        string
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    /// How Qt writes a value: spaces around it left out, quotes around what keeps them,
    /// escapes for other characters than ASCII (Qt 5) and for `\` and `"`, commas between
    /// the strings of a list.
    #[test]
    fn a_value_is_read_the_way_qt_writes_it() {
        let read = |value: &str| ClientSettings::unescape(value.as_bytes());
        let one = |text: &str| Some(vec![text.to_string()]);
        assert_eq!(read("/home/me/Nextcloud/"), one("/home/me/Nextcloud/"));
        assert_eq!(read("  /home/me/Notes   "), one("/home/me/Notes"));
        assert_eq!(read("/home/me/Notes ; moved here in May"), one("/home/me/Notes"));
        assert_eq!(read("\"/home/me/Notes; May\""), one("/home/me/Notes; May"));
        assert_eq!(read("\"/home/me/Work, 2026/ \""), one("/home/me/Work, 2026/ "));
        assert_eq!(read(r"/home/me/Caf\xe9 \xe9t\xe9/Caf\xe9\x65 \xd83d\xde00"), one("/home/me/Café été/Cafée 😀"));
        assert_eq!(read("/home/me/Café"), one("/home/me/Café"), "Qt 6 writes UTF-8");
        assert_eq!(read(r#"/home/me/Back\\slash \"quoted\" \101\x20"#), one("/home/me/Back\\slash \"quoted\" A "));
        assert_eq!(read(r#" a , "b, c" ,d\t"#), Some(vec!["a".to_string(), "b, c".to_string(), "d\t".to_string()]));
        assert_eq!(read(r"\xd800"), None, "half a character isn't text");
    }

    /// The settings of a Nextcloud client with folders in two accounts.
    #[test]
    fn a_clients_settings_list_the_folders_it_syncs() {
        let text = "\u{feff}[General]\r\n\
            clientVersion=3.13.0\r\n\
            \r\n\
            [Accounts]\r\n\
            0\\Folders\\1\\localPath=/home/me/Nextcloud/\r\n\
            0\\Folders\\1\\journalPath=.sync_0123456789ab.db\r\n\
            0\\FoldersWithPlaceholders\\2\\localPath = \"/home/me/Work, 2026/\"\r\n\
            ;0\\Folders\\3\\localPath=/home/me/Commented\r\n\
            0\\Folders\\4\\localPath=Relative\r\n\
            1\\Folders\\1\\localPath=C:/Users/Me/Nextcloud/\r\n\
            version=2\r\n";
        // A whole path is from `/` on macOS and Linux, and from a drive on Windows.
        let whole: &[&str] = if cfg!(windows) { &["C:/Users/Me/Nextcloud/"] } else { &["/home/me/Nextcloud/", "/home/me/Work, 2026/"] };
        assert_eq!(ClientSettings::local_paths(text.as_bytes()), Some(whole.iter().map(PathBuf::from).collect()));
        assert_eq!(ClientSettings::local_paths(b"[Accounts]\n0\\Folders\\1\\localPath=/a\\xd800\n"), None);
    }

    /// The folders the clients sync are told by their real paths (but for those whose drive
    /// isn't to be asked), from every client's settings. Where no settings can be read, or none lists a folder, or one client's
    /// can't be read, Charon can't tell.
    #[test]
    fn the_folders_the_clients_sync_are_read_from_each_ones_settings() {
        let tmp = tempfile::tempdir().unwrap();
        // As a client writes a folder: with `/` between its parts on Windows too.
        let written = |folder: &Path| format!("{}/", folder.display().to_string().replace('\\', "/"));
        let settings = |file: &str, text: &str| {
            let path = tmp.path().join("settings").join(file);
            write(&path, text);
            path
        };
        let listing = |folder: &str| format!("[Accounts]\n0\\Folders\\1\\localPath={folder}\n");
        let (nextcloud, owncloud) = (tmp.path().join("Nextcloud"), tmp.path().join("ownCloud"));
        fs::create_dir_all(nextcloud.join("Docs")).unwrap();
        let nextcloud_cfg = settings("nextcloud.cfg", &listing(&format!("{}Docs/../", written(&nextcloud))));
        let owncloud_cfg = settings("owncloud.cfg", &listing(&written(&owncloud)));
        let missing = tmp.path().join("settings/missing.cfg");

        let both = ClientSettings::at(vec![nextcloud_cfg.clone(), missing.clone(), owncloud_cfg]);
        let not_there = PathBuf::from(written(&owncloud));
        let as_written = PathBuf::from(format!("{}Docs/../", written(&nextcloud)));
        assert_eq!(both.synced_folders(&|folder| folder.starts_with(&nextcloud)), Some(vec![as_written, not_there.clone()]), "a folder whose drive isn't asked is kept as written");
        assert_eq!(both.synced_folders(&|_| false), Some(vec![fs::canonicalize(&nextcloud).unwrap(), not_there]), "a folder that isn't there is kept as written");
        assert_eq!(ClientSettings::at(vec![missing]).synced_folders(&|_| false), None);
        assert_eq!(ClientSettings::at(vec![settings("empty.cfg", "[General]\nversion=2\n")]).synced_folders(&|_| false), None);
        // A folder where a settings file should be can't be read as one.
        assert_eq!(ClientSettings::at(vec![nextcloud_cfg, nextcloud]).synced_folders(&|_| false), None);
    }
}
