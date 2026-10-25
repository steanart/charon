# Developing Charon

Charon is a [Tauri 2](https://tauri.app) desktop app: a Rust sync engine (`core/`, crate
`charon-core`), a thin Rust app around it (`src-tauri/`, crate `charon`) and a plain
HTML/CSS/JavaScript interface (`ui/`). There's no Node.js and no frontend build step.

Two-way sync is what Charon is for, and nothing matters more than never losing someone's files.
By default a sync never changes anything by itself: Charon prepares the list of changes, shows
it, and carries it out only after the person confirms it. A job can be switched to *Full
automation*, behind two warnings, and then syncs without showing the list first, still inside
the safety checks described below.

## Requirements

- **Rust 1.90+** (`rustup update`)
- **Tauri CLI:** `cargo install tauri-cli --version "^2" --locked`
- Per system:
  - **macOS:** Xcode command line tools (`xcode-select --install`)
  - **Windows:** Microsoft C++ Build Tools ("Desktop development with C++"); WebView2 is built
    into Windows 10/11
  - **Linux (Debian/Ubuntu):**
    `sudo apt install libwebkit2gtk-4.1-dev libayatana-appindicator3-dev librsvg2-dev patchelf libdbus-1-dev pkg-config build-essential`
    (the release build on Ubuntu 22.04 in `build.yml` installs `libappindicator3-dev` instead, and
    `xdg-utils`)

## Running, testing and building

```sh
cargo tauri dev                # run in development
cargo test -p charon-core      # the sync engine: unit tests and end-to-end tests with real folders
cargo test -p charon           # the app's own unit tests
cargo tauri build              # installers for the system you're on
```

A development build keeps its own data folder (`dev.stean.charon-dev`, next to the installed
app's), so trying something out never touches the real jobs or their two-way memory. It never
changes launch at login, and it can run next to the installed app: only an installed build
brings the running Charon's window up when it's launched a second time. Its log file is
`charon-dev.log`.

### The tests

Unit tests live next to the code they test, in a `#[cfg(test)] mod tests` at the end of the
file: the planners, the guard, the stores, the executor, the scanner and so on in `core/src`,
and the data folder, notifications and update check in `src-tauri/src`. `cargo test -p charon`
runs the app's; the release workflow runs only the engine's.

The end-to-end tests in `core/tests/` drive the real engine on real folders in temporary
directories:

- `safety_regressions.rs`: the permanent safety net. The first part turns around every
  data-loss probe written against Charon 0.2 for the code review: each one asserts that the loss
  can no longer happen. The second part checks what 0.3 promises: nothing changes without the
  person's OK (unless full automation is on and the job had its first confirmed sync), a list
  can't be confirmed once it no longer matches the folders or the job, unreadable places never
  turn into removals, excluded items are never deleted, and edits made after a list was made are
  never overwritten. Every test asserts the safe outcome. A change to how files are copied,
  replaced or removed must keep all of them passing, and should add one here.
- `engine_runs.rs`: looks, lists and runs through the engine, end to end.
- `run_lifecycle.rs`: how runs start, stop and wait when something around them goes wrong, such
  as a run cut off before it was recorded, a record that can't be saved, or Charon starting
  again.
- `slow_damage.rs`: full automation and damage that spreads slowly: the long count of what
  automatic runs removed or replaced, and a trash that only the person empties.
- `run_order.rs`: the order a run carries out its steps in, the removals it holds back when a
  copy fails or a file changes during the run, and how app libraries and databases land.
- `drive_comebacks.rs`: drives that come back during a run without its last changes, which must
  never be recorded as done (see *The last look* below).
- `batch_h.rs`, `long_paths.rs` and `drive_links.rs`: names, links, paths near a drive's limit,
  and what a drive refuses.
- `batch_i.rs` and `case_probes.rs`: the scanner, network shares, cloud folders, and asking a
  drive how it treats upper and lower case.
- `client_settings.rs`: which folders a Nextcloud or ownCloud client syncs, by the client's own
  settings on this computer and by its journal in a folder they don't list. It sets the
  variables that lead Charon to those settings, so it's a single test that goes through each
  case in turn.
- `names_left_alone.rs`, `items_left_alone.rs` and `leftover_claims.rs`: names Charon leaves
  alone stay alone on both sides, in any case, and its own leftovers are never taken for the
  person's files.
- `unreadable_files.rs`: files a scan can list but a copy can't read.
- `silent_shares.rs`: a network share whose server stopped answering, which must hold up
  neither the window, nor the autopilot, nor the other jobs (see *Folders that don't answer*
  below).
- `overlapping_jobs.rs`: jobs that share folders, and other syncs at work in the same folders
  (see *Overlapping jobs and other Charons* below): clashes refused when saved or found later
  through a link, copies of a `.charon` folder that block nothing, confirmed and automatic
  syncs that wait for another Charon or a neighbouring job, notes a sync cut short left behind,
  changes made during a sync, an older Charon running, and a state that can't be saved. A share
  mounted under two names can't be made without special rights, so a link made after the jobs
  were saved plays it; the unit tests in `engine/overlaps.rs` play a second mount past the path
  checks.

Some tests in `safety_regressions.rs`, `unreadable_files.rs` and `drive_links.rs` need a file or
folder Charon can't read or write. A process running as root with `CAP_DAC_OVERRIDE` can read
and write whatever the permissions say, so there these tests would prove nothing: they skip,
and write a line starting with `SKIPPED` straight to stderr, so a skipped safety test never
looks like one that passed. With `CHARON_REQUIRE_PERMISSION_TESTS=1` they fail instead of
skipping. As root, run them without those capabilities:

```sh
capsh --drop=cap_dac_override,cap_dac_read_search -- -c "cargo test -p charon-core"
```

`silent_shares.rs` runs on Linux only. It plays the share with a FUSE drive whose program never
answers, and mounting one takes `CAP_SYS_ADMIN` and `/dev/fuse`: run it as root (the `capsh`
command above keeps that right). Without them it skips the same way, and with
`CHARON_REQUIRE_PERMISSION_TESTS=1` it fails instead.

Two tests in `safety_regressions.rs` run on Linux only too: they mount a folder of the same
drive inside a job's folder (a bind mount) to check that what goes into the trash from there
gets there, a database's old journal among it, and mounting takes `CAP_SYS_ADMIN` (the `capsh`
command above keeps it). Without it the tests skip the same way, and with
`CHARON_REQUIRE_PERMISSION_TESTS=1` they fail instead.

The tests about upper and lower case in `names_left_alone.rs` need a folder on a drive that
ignores case for every letter. The temporary folder is one on macOS and Windows; on Linux, set
`CHARON_CASE_INSENSITIVE_FOLDER` to a folder on such a drive (a case-folding ext4 folder, say).
Without one they skip the same way, and with `CHARON_REQUIRE_CASE_TESTS=1` they fail instead.

`build.yml` sets none of these variables. The tests that need folder permissions or symbolic
links are Unix only (`#[cfg(unix)]`). One test waits for a list to expire (15 minutes) and is
ignored by default: `cargo test -p charon-core --test safety_regressions -- --ignored`.

### Working on the interface only

Serve `ui/` with any local web server (`python3 -m http.server -d ui`) and open it. Outside the
app, `ui/js/api.js` loads `ui/js/demo/backend.js`, a pretend Rust side with example jobs: a
two-way job with conflicts, jobs with full automation, places that couldn't be read, a drive
that isn't connected and a paused job. It answers every command and sends every event the real
one does, so keep it in step when you add either. From the browser console:

- `charonDemo.syncAll()`: the tray's *Sync All…*
- `charonDemo.showWaiting()`: the tray's *Review Waiting Jobs…*
- `charonDemo.quit()`: the tray's *Quit Charon* while jobs wait for an OK or have files to
  compare
- `charonDemo.quitting()`: Charon quits while a sync runs, and stops it safely first
- `charonDemo.change("projects")`: something changes in a job's folders, so its open list goes
  stale
- `charonDemo.refuseNext()`: the next *Confirm* is refused, to see how that looks

### Per system

Each system builds its own installers; a Mac can't build the Windows or Linux versions locally.

| | macOS | Windows | Linux |
|---|---|---|---|
| Installers | `.app`, `.dmg` | `.msi`, `-setup.exe` | `.deb`, `.rpm`, `.AppImage` |
| Folder watching | FSEvents | ReadDirectoryChangesW | inotify |
| Tray icon | menu bar (black, tinted by macOS) | system tray (white) | system tray (white) |
| Launch at login | LaunchAgent | registry | autostart entry |

If the project sits on a disk without file-lock support (some external or network drives),
incremental compilation is already switched off in `Cargo.toml`. For faster builds, put the
build output on the internal disk, for example
`export CARGO_TARGET_DIR="$HOME/Library/Caches/charon-target"`.

## Releases (all three systems)

`.github/workflows/build.yml` (*Build Charon*) builds Charon on GitHub's servers for macOS (Apple
Silicon and Intel), Windows and Linux. Each build first runs the engine's tests
(`cargo test -p charon-core`; failing tests also show as annotations on the run's page), then
makes the installers and gives them fixed names without the version:
`Charon-macOS-Apple-Silicon.dmg`, `Charon-macOS-Intel.dmg`, `Charon-Windows-Setup.exe`,
`Charon-Windows.msi`, `Charon-Linux.AppImage`, `Charon-Linux.deb` and `Charon-Linux.rpm`. So
`https://github.com/steanart/charon/releases/latest/download/<name>` always gives the newest
published version. Every run keeps the installers as downloadable artifacts of the run.

The workflow starts in two ways:

- **Push a version tag** (`git tag v0.3.0 && git push origin v0.3.0`). Once every build has
  succeeded, a release job puts all the installers into a **draft** release for that tag.
  Nothing is public yet.
- **Start it by hand** under *Actions → Build Charon → Run workflow*. With the version left
  empty it's a test build: the installers are only kept as artifacts of the run. With a version
  (`v0.3.0`), the same draft release is made for the built commit, and GitHub creates the tag
  when the release is published (handy when git can't push tags).

If a release with that tag exists already, the release job leaves it as it is. (Publishing a
draft creates its tag, which can start the workflow again.)

`.github/workflows/publish.yml` (*Publish Charon*) makes a draft public. Start it by hand under
*Actions → Publish Charon → Run workflow* with the version of the draft (`v0.3.0`) and, if you
like, release notes in Markdown (left empty, the draft's notes stay). It sets the notes, publishes
the release and marks it as the latest. From then on `releases/latest` points to it, and the
copies of Charon already out there see it as an update. A draft can also be published by hand
on GitHub.

### Signing the macOS version

The workflow signs and notarises the macOS installers when the repository has these secrets
(*Settings → Secrets and variables → Actions*):

| Secret | What it is |
|---|---|
| `APPLE_CERTIFICATE` | the *Developer ID Application* certificate as a `.p12`, base64-encoded |
| `APPLE_CERTIFICATE_PASSWORD` | the password chosen when exporting the `.p12` |
| `APPLE_SIGNING_IDENTITY` | e.g. `Developer ID Application: Name (TEAMID)` |
| `APPLE_ID` | the Apple ID e-mail of the developer account |
| `APPLE_PASSWORD` | an app-specific password for that Apple ID (account.apple.com) |
| `APPLE_TEAM_ID` | the 10-character team ID |

With the certificate and identity but without `APPLE_ID`, `APPLE_PASSWORD` and `APPLE_TEAM_ID`,
the app is signed but not notarised. Without the certificate (in a fork, for example), the app
gets an ad-hoc signature: it works, but macOS asks to confirm it under *System Settings →
Privacy & Security → Open Anyway*.

## How the engine decides

Every look at a job, every list the person confirms and every run of a job with full automation
follows the same path: a **survey** looks at both folders and plans, the **guard** and the
other checks say what deserves a look, and only a confirmed list (or an automatic run with
nothing to hold it back) is **carried out**. Afterwards Charon **records** what really
happened.

### Looking at both folders: the survey

`core/src/engine/survey.rs` makes a survey for one of three purposes: a background look
(`Check`), a list the person will see (`Review`), or a run nobody looks at (`Automatic`). A
survey never changes the person's files. (Where neither the system nor the names in a folder
show whether its drive ignores case, as in an empty folder, the survey creates an empty file of
Charon's own under a fresh temporary name, `.charon-<16 hex digits>.part`, at the top of the
folder and removes it at once. The name is unique, so looks at the same folder at the same time
each get the drive's own answer, and every look passes over such names.) It:

1. Checks that both folders are there (otherwise the job is *Offline*), reads the job's markers
   (`.charon/<job>.json`) at the top of both folders, and finds out what kind of drive each
   folder is on (`scan/volume.rs`): APFS, exFAT, an SMB share and so on, whether it ignores
   upper and lower case, and what it can hold. On Linux, a drive run by a program reports its
   real kind from `/proc/self/mountinfo` (`fuse.sshfs`, `fuse.rclone`, `fuseblk`). On Windows,
   a folder counts as on a network share when its real path (through links and mapped drives)
   is a UNC path or its drive is a network drive. A name's length is measured as the drive
   measures it (`Volume::cannot_hold`): in UTF-16 units on the drives made for Windows, on APFS
   and on a Linux `fuseblk` drive; in UTF-16 units of the decomposed name on Mac OS Extended;
   in bytes elsewhere. Where a drive
   can't be asked how it treats case this time, what it answered at the last sync holds while
   it's the same drive (`Volume::case_guessed`, kept in the job's record of its drives), so a
   guess never makes Charon distrust its memory. When the last sync could only guess and the
   drive can now be asked and answers otherwise, the memory isn't trusted, and the list says
   Charon couldn't ask that drive at the last sync.
2. Scans both folders at once, each on its own thread (`scan/scanner.rs`). On macOS and Linux,
   symbolic links are recorded as links and never followed; on Windows, links and junctions are
   left out with a note, and so are names Windows can't handle. Also left out: the job's exclude
   patterns; Charon's own names at any depth (`.charon`, `.charon-trash`,
   `.charon-<16 hex digits>.part`, links under such names and Charon 0.2's `*.charon-partial`)
   and, where either drive ignores case, an item of the person's named like one of Charon's
   folders in other case (`.CHARON-TRASH`), which would be Charon's own folder on such a drive;
   empty files dated 2 January 1980 00:00:00 UTC, which an interrupted rename left (see
   *Renames* below); on a Mac, every `._*` file, whatever the patterns say (on drives that
   aren't Mac drives it holds another file's tags and resource fork); names that aren't valid
   Unicode; sockets, pipes and devices; and, at the top of the job's folder, items named like
   the folders drives keep for themselves (`$RECYCLE.BIN`, `System Volume Information`,
   `.Trashes`, `.Spotlight-V100`, `lost+found`, `.Trash-*` and the like), with a note when the
   job's folder isn't the top of its drive. An item left out with a note keeps its name out of
   the sync on both sides, under that one note: nothing is copied into its place, and a mirror
   doesn't remove the other side's item of that name. When either drive ignores case, the
   exclude patterns match the folded key (step 3), so they ignore the case of every letter,
   `Entwürfe` and `ENTWÜRFE` alike; a pattern that only breaks once folded is refused when the
   job is saved (`ExcludeFilter::check`). A new job starts with the exclude patterns
   `.DS_Store`, `._*`, `.Spotlight-V100`, `.Trashes`, `.fseventsd`, `Thumbs.db`, `desktop.ini`,
   `$RECYCLE.BIN`, `~$*` and `*.tmp`, so files matching `*.tmp` or `~$*` aren't synced until the
   pattern is removed. A place that can't be read is recorded as *frozen*, never as empty, and
   nothing at or inside it is touched on either side. Files and folders that are only in the
   cloud (recognised by their attributes on macOS and Windows) and drives mounted inside the
   folder are frozen too; on macOS the scan threads are also set never to download a file that's
   only in the cloud. So is a file a cloud service freed up and left a placeholder for
   (`scan/cloud.rs`): `.Name.icloud` from iCloud Drive before macOS 14, in iCloud Drive's
   folders on this Mac (`~/Library/Mobile Documents`, `~/Desktop` and `~/Documents`, by their
   real paths), and `Name.nextcloud` or `Name.owncloud` in a folder a Nextcloud or ownCloud
   client syncs: one the clients' settings on this computer list, by its real path
   (`scan/clients.rs`: the `localPath` of each folder in `nextcloud.cfg` or `owncloud.cfg`, in
   the client's folder in `$XDG_CONFIG_HOME` or `~/.config` on Linux, or in
   `~/.var/app/<id>/config` for a Flatpak, in `~/Library/Preferences` on macOS, and in
   `%APPDATA%` or `%LOCALAPPDATA%` on Windows), or one with a client's journal (`.sync_<hex>.db`
   or `._sync_<hex>.db`) in it or above it, whatever the settings say: a client on another
   computer may sync it. A copy of a synced folder holds a copy of the journal too, so where
   only a journal makes a folder count, the note about each file left alone there names the
   journal: if no client on any computer syncs that folder, the journal and the placeholder are
   old copies, for the person to remove. The placeholder is never synced or removed, and where
   the receiving side's service keeps files, nothing named like its placeholder is made there.
   Anywhere else a file named so is an ordinary file; where Charon can't look (iCloud Drive's
   folders on a Mac without a home folder, or the real path of the scanned folder, or a folder
   that may hold a journal, can't be read), it is taken for a placeholder, and where no client's
   settings can be read, or they list no folder, the journals alone tell. A client's
   journal file itself (`.sync_<hex>.db`, `._sync_<hex>.db` or the older `.csync_journal.db`,
   with its `-wal`, `-shm` or `-journal`) is the client's own: it's left out without a note at
   any depth, before the exclude patterns are matched, and keeps its folder from being removed,
   and an item of that name on the other side is left alone too, with a note. An item named
   exactly `.charon` or `.charon-trash` is Charon's only when it's a folder holding nothing but
   Charon's own files and system clutter (`Scanner::looks_like_charons`); otherwise it's the
   person's, and is left out with a note.
3. Turns every path into a *key* (`scan/keys.rs`): relative to the job's folder, `/`-separated
   and in Unicode NFC, and folded when either drive ignores case (each letter to the lower case
   of its upper case, so ς and σ, or µ and μ, are one letter, and the dotless ı meets i, as it
   does on NTFS, exFAT and FAT). Two names in one folder that end up as the same key are a name
   clash, and are frozen too.
4. Two-way: loads the memory of the last sync (`store/baselines.rs`) and decides whether to
   trust it. Mirror and Backup: loads the record of copies whose dates didn't stick
   (`store/landings.rs`).
5. Two-way: reads pairs of files byte for byte where the dates alone can't tell
   (`compare_contents`): pairs of the same size that have no record and different dates, or
   that changed on both sides since the record (whatever their dates), and a file that changed
   on one side but kept its size while its time stayed put or moved only by a rounding or by
   whole quarter hours. Identical pairs are recorded as in sync. Pairs that look alike but
   differ become conflicts when both sides changed since the record or there's no record to go
   by; when only one side changed, that change crosses. Without a record, a pair of the same
   size whose times are at most 2 seconds apart is taken as identical without being read,
   except where the survey's `Guide` says otherwise: a pair found to differ before and left for
   the person (`Memory::unsettled`), and, when the comparison is a fresh one, a pair that the
   record Charon doesn't trust shows changed since on either side, are read all the same. A
   pair whose contents can't be read counts as differing for that look. A pair read before in
   this session isn't read again while both sides are exactly as they were then and both folders
   carry the markers they carried then (`Reads`, in the job's live state; a sync of Charon's own
   carries them on to the markers it writes). Folders without a marker Charon can read (before
   the job's first two-way sync, or with a damaged marker) keep none, as `store/same_contents.rs`
   keeps none for them: nothing tells them from another drive under the same name. So each list
   before the first sync reads such pairs again, and the first list after that sync reads a
   conflict it left once more. A background look reads at most 1 GB
   (`CHECK_COMPARE_BYTES`); lists and runs read everything. Past that, a background look goes by
   what full reads found before: identical, while both sides are exactly as they were then
   (`store/same_contents.rs`, kept across restarts while both folders carry the markers they
   carried then), or different and left for the person. Any other pair is *not compared yet*
   (`Survey::uncompared`, `Look::uncompared`): the plan leaves it as it is, and the job is never
   *In sync* while there are any. Its row says "1 file not compared yet", and shows *Files to
   compare* when that's all the look found. When a folder's marker changed while the look read
   (another drive under the same name), what it read isn't kept for later looks
   (`still_marked`).
6. Plans (`plan::plan_for`), then runs the guard and the other checks. Each check can add a
   **warning** to the list, a **hold** that stops a run nobody looks at, and the need for a
   **tick** before the list can be confirmed.

### Mirror and Backup

`plan/one_way.rs`. The destination follows the source. Two items count as the same when they're
both folders, files of the same size whose modification times are at most 2 seconds apart (FAT,
exFAT and network shares round them), or links pointing to the same place. So an edit that
keeps a file's size and its time (within those 2 seconds) isn't seen, and that file isn't copied
again until one of them changes. A new or different file is copied, except:

- a copy whose date didn't stick on the destination's drive (`landings/<job>.json`), while
  neither side has changed since it landed. This record only ever spares a copy; it never
  justifies removing or replacing anything;
- on FAT and exFAT, which keep local times, a file of the same size whose time is a whole number
  of hours off (a daylight-saving change or another time zone).

Mirror also removes, item by item, everything in the destination that isn't in the source,
including files that were only ever put in the destination. Backup never removes anything.
When an item of another kind is in the way (a file where the source has a folder), Mirror
removes it right before the new item takes its place, into `.charon-trash` whatever the
setting, unless it's a folder holding items Charon leaves alone; Backup leaves it alone with a
note. A link in the source where the destination has a file or folder never removes it,
in either mode.

Mirror and Backup check that they're looking at the folders of the last sync: the destination
must carry the marker token Charon left there, and the source's marker (where Charon could
write one) must have the same token and counter. A source whose counter went down is probably
an older copy of the drive, which would replace the newest backups.

### Two-way

`plan/two_way.rs` and `plan/baseline.rs`. When a file is missing on one side, Charon has to know
whether it was removed there or is new on the other side. So it keeps a *memory* (the
baseline): for every item both folders agreed on after the last sync, how it looked *on each
side*. Each side is compared with its own record, exactly: kind, size, modification time to the
millisecond, link target, and the drive's change time where both looks know one. On FAT and
exFAT, a whole-hour shift of a file of the same size doesn't count as a change.

| source vs memory | destination vs memory | what happens |
|---|---|---|
| same | same | nothing |
| changed | same | the source's change crosses (a new item, an edit or a removal) |
| same | changed | the destination's change crosses |
| changed | changed differently | conflict, settled by the job's rule (*Keep both*, *Newest wins* or *Ask me*) |

- Both sides changed in the same way (they look alike now and weren't found to differ): nothing
  to do.
- An edit on one side and a removal on the other: the edit wins and is copied back.
- A file whose details alone changed on one side (only its change time moved: tags,
  permissions) and that was really edited on the other side (its size changed, or its time moved
  by more than a rounding of 2 seconds or whole quarter hours, `Entry::drifted_from`): the edit
  crosses, as a change of that side alone (`changes_that_cross`). When the edited side's time
  only moved the way drives move times, either side may hold the edit, and the pair stays a
  conflict unless its contents were read and found the same.
- A file whose time is now older than its own record (restored from a copy, or a drive that
  lost its last writes) is treated as a conflict, never as an edit that crosses, and *Newest
  wins* never settles it.
- *Keep both* keeps the destination's version as `name (conflict 2026-10-16 16.20).ext` on both
  sides and the source's under the original name. *Newest wins* copies the newer version over the
  older one, which goes to `.charon-trash`. It only decides when the times can be trusted: both
  drives local and of a known kind, neither keeping local times (FAT, exFAT), the times more than
  2 seconds apart and not a whole number of hours apart, and neither side's time gone backwards.
  Otherwise two files are both kept, as with *Keep both*, and two links are left for the
  person. *Ask me* leaves the item for the person.
- When both sides changed an item (or there's no memory to tell), a file meeting a folder is
  always a question for the person, and so are two different links unless *Newest wins* can
  tell which is newer (then the newer link is carried and the other goes to `.charon-trash`). A
  link taking the place of a file or folder is always a question. When only one side changed an
  item into another kind, that change crosses: a folder that became a file is replaced on the
  other side only when that folder holds nothing Charon leaves alone and nothing changed in it.
  What it replaces goes to `.charon-trash`, even with safe deletes off (see *Carrying out a
  list*).
- **Units** (`plan/units.rs`): an app's library or document package (a Photos or Final Cut
  library, a Logic project, a Pages document, a sparse bundle; the list is `PACKAGES`) and an
  SQLite database with its `-wal`, `-shm` or `-journal` file only work as a whole. A package is
  only one when it's a folder on each side that has it (a document saved as a single file is
  an ordinary file), and a database only a file with a journal file next to it; a database's
  unit is all its files. When files of one unit changed on both sides (any change counts here,
  details too), the whole unit becomes one conflict, and none of its files moves. A run carries
  a unit's files together too; only a file new to a library may land on its own while the rest
  waits, and a library the receiving side doesn't have yet is copied file by file (see *Carrying
  out a list*).
- A folder is only removed when everything inside it is removed by the same plan.

Without a memory Charon can trust, **nothing is removed**: an item on one side only is copied to
the other, and two files that differ are kept both or left for the person, except with *Newest
wins*, which still replaces the older version when the times can be trusted (the older one goes
to `.charon-trash`). That's the case for the first sync, and whenever the memory is missing
after the job has synced, can't be read or is damaged, belongs to other folders, was saved by
Charon 0.2 or earlier, the last sync didn't finish (`run_in_progress` is still set), a drive now
treats case differently, or a folder's marker is missing, unreadable, carries another token, or
has a lower or higher counter than the memory. The list then says why and what happens to files
that differ under the job's rule, and an automatic run is held. When a memory was there but
can't be trusted, the list also counts the items only on one side that get copied to the other
and, where an older record names them (the memory it distrusts, or 0.2's), how many of those
were on both sides at the last sync and so were most likely removed since, naming them. That
record never decides anything.

### Settling one conflict

`plan/resolve.rs`. A conflict in a job's row offers *Keep source*, *Keep destination* and, for
two files, *Keep both*. The choice prepares a list like any other, which touches only that item
(and what's inside it) and is confirmed the same way; the version it replaces or removes goes
to `.charon-trash`, even with safe deletes off. An app library or database is settled as a
whole, and can't be kept both; items in it that the other drive can never hold (a link on a
drive without links, a name it can't take) stay out of it. When the memory is trusted, only
that item's record changes. When it isn't, the memory is kept as it was, without the settled
item and with the reason it isn't trusted (`distrust` in the memory file, `keep_untrusted`):
every look compares afresh and removes nothing until a whole list is confirmed, while the lists
still say why and name the items most likely removed since. A memory without markers (Charon
0.2's, a damaged one, one for other folders) is left as it is, since no look trusts it anyway.
The list that settles the item says so too.

### The guard

`plan/guard.rs` looks at every plan. A **refusal** stops it in every mode, and such a list can't
be confirmed: one side has no files while the plan would remove on the other side, which has
some (almost always a drive that isn't connected properly, or a wrong folder).

A **concern** is a plan that looks like something went wrong. It holds an automatic run, shows
in red in the list, and the list needs the tick *I've looked at the removals and replacements*.
Counted per side, on files and links:

- removals of at least 3 files that are at least half of the files there;
- removals and replacements of at least 5 files that are at least half of them, of more than 20
  that are more than a quarter of them, or of more than 1,000 in any case;
- a file with contents that would be replaced by an empty one;
- a folder on the other side that's there but looks empty, while the plan would remove in it on
  this side and it held at least 5 items at the last sync (two-way), or the plan removes at
  least 5 items from it. A share or cloud folder listed before it has loaded looks like that.

For jobs with full automation (in their automatic runs, and in their lists, so a list shows why a
run was held), what automatic runs did before counts too, each file once:

- **The day window:** the files automatic runs removed or replaced during the last 24 hours
  (kept in the job's record in `charon-state.json`, at most 1,001 per side, more than the largest
  threshold), added to this plan's, against the files there plus those removed since: the
  removal-and-replacement thresholds above apply.
- **The long count** (`store/unattended.rs`): every file automatic runs removed or replaced since
  the person last confirmed a list, kept in `unattended/<job>.json` for the last *Keep deleted
  files* days (with 0, until the next confirmed list), at most 100,000 per side. Together with
  this plan's, at least 5 files that are at least half of the side, or more than 20 that are
  more than a quarter of it, is a concern, and so is a count that overflowed. This catches
  damage that spreads more slowly than a day allows (a broken tool rewriting a few dozen files
  an hour, or a few files a day in a small folder). Damage spread more slowly than *Keep deleted
  files* days isn't caught, though the originals stay in the trash, which automatic runs never
  empty. A count that can't be read is never taken for zero: any plan that removes or replaces
  something is then a concern.

The survey adds its own checks besides the guard (the first two are described in *Overlapping
jobs and other Charons* below):

- **Refuse, like the guard:** a job whose two folders are one place, or one inside the other,
  found by the folders themselves (links, a drive mounted twice) and, once the job has finished
  a sync, by its markers (a share reachable under two names).
- **Hold an automatic run and need the tick:** another job that would undo this one's work (a
  Mirror job and a job writing into its destination), a folder now on another drive than at the
  last sync, and a Mirror or Backup marker that doesn't match.
- **Hold an automatic run:** a two-way memory Charon doesn't trust, files changed inside
  `.charon-trash` after Charon put them there, and not enough room on a drive.
- **Need the tick:** with safe deletes off, a list that removes files (or, in Mirror and Backup,
  replaces them). Removals that make room for an item of another kind don't count: they go to
  the trash anyway.

Lists also warn about places that couldn't be read, items that can't be synced, conflicts
settled by *Newest wins*, copies of a job's `.charon` folder (which can be deleted), symbolic
links, the trash growing large, and app libraries and databases the sync changes (close the app
first). That last one is a warning only: it doesn't hold an automatic run. A library or database
the sync changes waits for the next sync when something of it changes while its files are
copied, but one the receiving side doesn't have yet is copied file by file (a database in it
still goes with its journals), so with *Full automation* it may arrive with some files from
before its app wrote and some from after. Leave such folders out of jobs with full automation,
or quit the app before the sync.

### Lists and confirming

`engine/review.rs`. *Review and sync* on a job's row prepares a list from a fresh survey. It's
kept, one per job, until it's confirmed, closed, replaced by a newer one or the job changes. It
shows the steps (removals first, then what replaces something, conflicts, new folders and
copies), the notes (places left alone, with the reason), the warnings, and housekeeping such as
old trash it would empty.

A place that couldn't be read offers *Leave out*, which adds an exclude pattern for exactly that
place; the next sync then waits for an OK. The pattern (`ExcludeFilter::pattern_for`) writes
every character a pattern gives a meaning as itself, a backslash as `[\]`, and spaces at the
end of the name in braces (`/notes{ }`), since a pattern is read without the spaces around it.

A list with no steps can still be confirmed when that counts, and says why
(`Engine::confirm_anyway`): it ends a hold, lets a job with full automation sync by itself
again, takes a folder Charon doesn't recognise as it is now, lets a two-way job remember both
folders, or records a job's first sync. When such a list needs the tick, the tick reads *I've
read the warnings above*.

*Sync all* (and the tray's *Sync All…*) starts the jobs that run by themselves and opens *Waiting
for your OK* for the others (jobs paused on their own, jobs whose folders aren't there or don't
answer, jobs already syncing and jobs whose confirmed list waits for another sync are left out),
where each list is prepared and confirmed on its own. There is deliberately no way to confirm
them all at once.

*Confirm* carries out exactly that list and nothing else. The engine refuses it when:

- the list is more than 15 minutes old;
- something that matters changed in the folders since it was made (the folder watcher marks it
  stale, and the window hears `review-stale`);
- a folder isn't there, isn't the same folder anymore (`RootId`: its drive, its number on the
  drive, its creation time where the system keeps a believable one, on Linux 6.8 and later the
  mount it's seen through, so a drive unmounted and mounted again counts as another folder, and
  on Windows the GUID of its volume, which a cloned drive doesn't share), or carries another
  marker;
- the job's settings changed, or the job synced in the meantime;
- the guard refused the plan, or the job's folders turned out to be one place;
- the list needs the tick and it isn't ticked.

A list that's out of date offers *Look again*, which makes a fresh one. While a list is being
prepared, waits for an OK, or was confirmed and waits for another sync, automatic runs of that
job don't start: a trigger that comes meanwhile only looks, or is dropped. Another sync at work
in the job's folders doesn't refuse *Confirm*: the confirmed list waits for it (see
*Overlapping jobs and other Charons*). Making a list and *Sync now* are refused meanwhile, with
`Error::FoldersBusy` ("“X” is syncing one of these folders right now. Look again when it's
done."), or `Error::DriveBusy` when another job uses a drive of theirs that's run by an add-on driver
(see *Drives run by other makers' drivers*).

### Carrying out a list

`run/executor.rs` runs the steps in this order (`Plan::ordered_steps`): new folders (from the
top down) and copies; then each place where an item of another kind takes an item's place (a
folder that became a file, a file where a new folder goes), one place after the other, first
the item there with everything inside it (deepest first), then the new folders and copies that
take its place; and last every other removal (deepest first). Removing last means a run that
stops halfway never leaves an item that was moved or renamed with only one copy, and an item
that makes room for another goes as late as it can for the same reason. Such a removal
(`Step::makes_room`) waits like any other removal when copies to its side failed, always keeps
what it removes in `.charon-trash`, even with safe deletes off, and nothing goes into a place
whose item couldn't go first. Conflicts left for the person are never touched.

Before every step, both folders must still be the folders the plan was made for, or the run
stops. Right before a step acts it looks at the disk again, on both sides: the item it copies,
replaces or removes must be exactly what the plan saw, and so must the other side's item the
step relies on (a file restored on the other side stops its removal here). If anything changed,
the step is skipped, never forced, and the next look picks it up. The folders above an item are
checked one by one, so nothing is ever written or removed through a link, and nothing is
written into a folder Charon leaves alone (excluded, or one of its own), whatever its spelling
on disk: a folder a step finds where the scan saw nothing is checked against the scan and the
exclude patterns first (`not_left_alone`). On macOS and Linux, what a step wrote must be on the
folder's drive; if the drive went away, the run stops there.

- **Copies** (`run/copier.rs`) are written to a new file `.charon-<16 hex digits>.part` next to
  the target (the first 8 digits are when the copy started), never through a link. Where that
  path would be too long for the drive (a short name deep down has a longer temporary name), the
  copy is written in the nearest folder above where it fits, never above the job's folder, and
  renamed into place from there; moving an item aside does the same (`Copier::temporary`). The
  source is checked before and after reading, and a copy of a file that changed meanwhile is
  thrown away. When the source's drive keeps times in steps of a second or more, a file dated
  within the last 3 seconds, or up to 5 minutes ahead of the computer's clock (a share's clock
  may run ahead), waits for the next sync (`Copier::saved_a_moment_ago`); a time further ahead
  is a clock set wrong, not a save going on, and holds nothing back. The copy is flushed, takes
  the extended attributes worth carrying (Finder tags, Finder info and resource forks on macOS,
  `user.` attributes on Linux) and the original's modification time (and its creation time on
  macOS and Windows), and is flushed again; then it takes the original's permissions (a drive
  that refuses them only leaves a line in the app's log). A drive that can't keep extended
  attributes (FAT, exFAT) gets the file without them, apart from a resource fork and the Finder
  info that goes with it, and the run's summary says what stayed behind. On macOS and Windows, a
  file the cloud took back since the scan is never downloaded: copies, the copies of old
  versions kept in the trash and byte comparisons all open files through `Contents::open` (on
  macOS the reading thread can't download, on Windows the file is opened with
  `FILE_FLAG_OPEN_NO_RECALL`, and on both a file whose details say it's a placeholder is
  refused), and the step is skipped as only in the cloud since the scan.

  Tags, permissions and extended attributes travel only with a copy of the file: a change to
  them alone, with the contents and the modification time as they were, isn't synced (two-way
  finds the contents identical, and Mirror and Backup don't look at them). A copy never carries
  ACLs, owner and group, other extended attributes (such as the quarantine flag), or, on
  Windows, extended attributes and alternate data streams. Hard links arrive as separate files,
  and sparse files take their full size.
- **Links** are copied as links on macOS and Linux, under a temporary name like a file. A drive
  counts as unable to hold links only when it says so outright, or when it refuses one at the
  top of the job's folder where it takes a file (`Copier::drive_holds_links`, asked at most once
  per run with a brief `.charon-<16 hex digits>.part` probe). Charon remembers that, also in the
  job's record of its drives, and later lists leave links out for that drive with a note. A link
  refused in one folder for another reason (a folder that can't be written to, say) is a failed
  item.
- **Renames** (`run/rename.rs`) never replace anything: `renameat2` with `RENAME_NOREPLACE` on
  Linux, `renamex_np` with `RENAME_EXCL` on macOS, `MoveFileExW` without
  `MOVEFILE_REPLACE_EXISTING` on Windows. Drives without such a rename (NFS, sshfs and other
  drives run by a program, some macOS NFS mounts) get one of two fallbacks, by whose name moves.
  `rename_new` gives one of Charon's own items (a finished copy, an item it moved aside) a name
  the person may use: the item gets the new name as a hard link and then loses its temporary
  one, which only Charon uses. `move_to_new_name` moves the person's item to a fresh name Charon
  made for it (aside, into the trash, a conflict copy's name): the new name is claimed with an
  empty file (or folder) the drive creates only if the name is free, and the item is renamed
  onto the claim in one step. The person's name is never removed on its own, so a version an app
  saved a moment before moves with the item, and the check after the move sees it. Where a drive
  has no hard links either (and for a folder), `rename_new` claims its name the same way.
  Something that appeared under a name before Charon claimed it is never overwritten; only
  something written into an empty claim in the instant before the rename could be. A file's
  claim that a failed rename leaves behind is an empty file dated 2 January 1980 00:00:00 UTC
  (`scan::CLAIM_MODIFIED_MS`). The scan takes it for Charon's leftover, never for the person's
  file: it isn't synced, it keeps its folder from being removed, and a list names it in a
  housekeeping line of its own. A folder's claim is an empty folder, which nothing marks, so
  one left behind looks like the person's own and is synced like one.
- **Replacing** a file: where the drive can swap two names in one step (`RENAME_EXCHANGE` on
  Linux, `RENAME_SWAP` on macOS), the copy and the old file swap, and the file swapped out is
  checked; if an app saved a new version at that very moment, the two swap back. Elsewhere
  (Windows, FAT and exFAT, most network shares) the old file is renamed aside and checked first,
  and only then does the copy take the free name. Either way, what an app saved is put back or
  kept in `.charon-trash`, never written over; when the trash can't take it, it stays under a
  temporary name in the job's folder, and a later sync moves it into the trash and says so.
- **Keeping the old version:** with safe deletes, always in two-way jobs, and always when a copy
  settles a conflict, a file being replaced is first *copied* into the trash and then replaced
  in place (a link is moved into the trash, if it's still the one the plan saw: one removed or
  changed a moment before is left as it is, and the step is skipped). An app that has the file
  open follows the file, not its name: had the old version been moved into the trash, the app
  could go on saving the person's work there. Replaced in place, the app finds the new version
  where the file was. On APFS that copy is a clone, which takes no time and no room. On macOS
  the copy doesn't take the file's Finder lock along (`Trash::unlock`), so it can be cleared out
  later. When the replacement then fails and the old version is still in its place
  (`still_in_place`), the copy is discarded, so a sync that keeps failing on a file an app holds
  open doesn't fill the trash. An item replaced by one of another kind (a file by a folder) is
  removed right before the new item lands, as a removal (below).
- **Removals** (`Executor::remove`): with safe deletes, and always when it makes room for an
  item of another kind, a file or link is moved into `.charon-trash` in one step and checked
  there; if it isn't what the plan saw (an app saved a new version at that very moment), it goes
  back to its place (`move_to_trash`). The same goes for a replaced link, the system clutter
  left in a removed folder, and the leftovers of an interrupted sync: whatever goes into the
  trash under its own name. When the trash can't take it (a link or a file in its
  place, no permission), the item isn't touched and the step fails with an error that names
  the place in the trash. Where a rename can't reach the trash (a folder of the same drive
  mounted inside the job's folder, a bind mount on Linux; another drive mounted there is frozen
  by the scan), the item is copied into the trash and then removed, which needs room there, and
  a link is made again there with its own time (`Trash::copy_link`). On APFS a file kept that
  way is cloned into the trash and then removed. Without safe deletes, a file or link is renamed
  aside, checked, and removed for good (`remove_checked`). An item Charon renamed and had to put
  back carries a new change time; the memory takes that time for it (`PutBack`,
  `Baseline::follow`), so the next look doesn't take the rename for an edit. A folder is removed
  only when nothing but system clutter (`.DS_Store`, `Thumbs.db`, `desktop.ini`, `._*` files and
  the like) is left in it, and that clutter always goes to the trash. A sync client's journal
  isn't clutter, though `._sync_<hex>.db` matches `._*`: the folder holding it stays.
- **Keep both:** the destination's version is moved to its conflict name, the source's version is
  copied to the destination under the original name, and the conflict copy is copied to the
  source. The conflict name is numbered when taken on either side, and shortened to fit the
  drive's limit on name length (`run/names.rs`) and, where the path to it would be too long, the
  room the path leaves. When not even one letter of the original name fits, both versions stay as
  they are with a note (`Refusal::ConflictCopy`), remembered like other refusals (below) while
  both versions are there, so later runs go on with everything else.
- **App libraries and databases** (`Executor::carry`): the files of one unit (see *Two-way*) are
  carried together, in every mode. First every new version is written next to its target
  (`prepare`). That can take hours for a big library, and its app may write to it meanwhile, so
  right before the new versions take their places everything is checked again (`still_ready`,
  `as_staged`): each original and each target, each item the unit removes, and the whole library
  or database on each side the run changes it on (`unit_unchanged`: nothing in it but what the
  scan saw, the folders this run makes, the copies it is writing, system clutter and what Charon
  leaves alone). When several are carried at once, each is also checked once all of them are
  written. Then, once the trash has room for the versions they replace (`room_for_replaced`), a
  database's old journals are renamed aside (`set_aside`) and the new files take their places,
  one right after the other. What the unit no longer has goes last, and only once all its new
  versions have landed: right before the plan's first plain removal, after every library and
  database carried by then (`remove_units`), so each removal is checked against what all of
  those did. While one of the unit's removals is held back (`Holds`, below), none of them goes,
  so a file moved from one library into another keeps its old copy when its new one didn't
  land. If a new version can't be written, or anything changed meanwhile, none is carried, and
  the steps say why ("left for the next sync: something in “Photos.photoslibrary”, an app
  library or document, changed since the scan"). Only a file or link added where the scan saw
  nothing, and not part of a database, still lands on its own, checked like any copy
  (`let_additions_land`), so new photos arrive while a library its app keeps writing to waits.
  A library the receiving side doesn't have yet has no version to mix with: its files go one by
  one like any others, and only a database in it goes as a unit (`unit_of`). A unit the plan only copies into is carried
  at its first step that isn't a new folder; one it removes something of, or clears a place in,
  waits for the plan's copies, and is carried at its first removal or step in a cleared place,
  or before the plan's first plain removal if that comes sooner (`starts_carry`). When one of
  its removals is held back, none of it is carried but what only adds to it ("kept for now:
  other copies didn't land in this run…"). A failure once the files are taking their places
  (rare) stops the ones after it, but for what only adds to it, and puts the old journals of
  those databases back; the unit can be left part old and part new, but nothing of it is removed
  except the old journals of a database that landed, which go to `.charon-trash`. The next sync
  looks at the unit again. An old journal set aside that isn't removed for good goes into
  `.charon-trash` under its own name. Where a rename can't reach the trash (a folder mounted
  inside the job's), it is copied there and then removed, like a removal (`trash_aside`); one
  that changed after it was set aside then stays under its temporary name, its copy goes, and
  the step says where it is. The run's summary counts what waited ("3 items left for the next
  sync with the rest of their app library or database"). A library holding an item Charon leaves alone
  (one it can't read, one only in the cloud) is left alone whole, with a note
  (`Ground::keep_units_whole`); an item the other drive can never hold (a link on a drive
  without links, a name it can't take, a file too big for it) is left out of it on its own,
  except in a database, whose files only go together.
- **Room** (`run/room.rs`): Charon keeps 2% of every drive it writes to free, at most 2 GB,
  where the drive says how much room it has, so other apps can still save. A copy that would eat
  into that fails with a clear reason. On a Mac, room the system can free by itself counts as
  free. The copies of replaced versions kept in the trash need room too, and how much at one
  time depends on the drive (`KeptCopies`): none on APFS, where they're clones (a file on
  another drive than the trash, one mounted inside the job's folder after the scan, is copied in
  full, and on macOS and Linux its copy counts in full on every kind of drive:
  `Executor::clones_at`, `room_for_replaced`); the largest
  one's on a drive that gives a removed file's room back at once (Mac OS Extended, FAT, exFAT,
  NTFS, ext, XFS, tmpfs and `fuseblk`, not over a network), plus those of files with other names
  (hard links, `Identity::names`), which keep their room; and every one's elsewhere (Btrfs, ZFS
  and ReFS may keep a removed file in a snapshot, a share's server may too, and a drive Charon
  doesn't know counts the same). The survey counts that room in what a sync needs
  (`Engine::bytes_onto`, which can't see hard links), and before an app library or database
  lands, the run makes sure the trash has that room for the versions it replaces
  (`room_for_replaced`), so it almost never stops halfway.
- **One failed item doesn't stop the run:** it's counted, written to the job's log (the first 50
  failures, and the first 50 of the skipped, waiting and left-out items together) and shown; the
  row names the first two that failed. What a run holds back (`Holds`): on a side where a copy
  failed, a file couldn't be read or the drive refused an item, every removal the run would
  still make there waits. A copy left for the next sync because its file changed since the scan,
  because the item of another kind in its place couldn't go first, or because it waits with the
  rest of its app library or database holds back only the removals there that could be its old
  copy (an item with the same name, or the same size and time), and the job's log says for each
  which copy it waits for, and why ("kept for now: it may be the old copy of “X”, which changed
  since the scan and wasn't copied to the destination yet"). So one file an app keeps writing to
  doesn't stop every removal. Charon remembers what a drive refused until it quits (or the job's
  folders or mode change), while the item and the drive stay the same: later lists leave it out
  with the reason, and after a restart it's tried again. A name the drive found taken by another
  item (a short name like `REPORT~1.PDF`, or another rule for upper and lower case) is tried
  again as soon as that item is gone.
- **Files Charon can't read:** a file the scan listed but the copy couldn't read (no permission,
  macOS's privacy settings, a mailbox another program holds open, a locked range:
  `Error::ReadDenied`) isn't a failure, and the run isn't an error. The row lists it under
  *Couldn't read*, with *Leave out*, and a notification says so once (with *Notifications* on).
  Until Charon quits (or the job's folders or mode change), later looks keep it there for as
  long as the part where reading stopped still can't be read (`Copier::read_refusal`), even
  while the file keeps changing; then it's tried again. Meanwhile every removal of a file or
  link on the side it should go to waits, with a note naming it, and so does every other step on
  an app library or database one of those removals belongs to (`Ground::hold_removals`): a
  removal there could take the last copy of what it holds, a renamed mailbox say. Only a removal
  that makes room for an item of another kind goes ahead, into `.charon-trash` as always.
- **The trash** (`run/trash.rs`): each run puts what it removes or replaces into
  `.charon-trash/<YYYY-MM-DD_HH-MM-SS>_<6 hex digits>/` at the top of the folder, with a
  `.charon` list of every file's size and time. A file that no longer matches its list was
  changed after it went in (an app may still be saving someone's work into it): its dated folder
  is never emptied, lists say where it is, and automatic runs are held. What the system writes
  into a folder by itself (`.DS_Store`, `Thumbs.db`) and Charon's own half-written copies are
  nobody's work (`is_nobodys_work`) and don't count as such a change, and on FAT and exFAT
  neither does a time moved by whole hours. Nor does a cloud service's placeholder for a file
  the list names, once that file is gone from the dated folder and the placeholder says what the
  list says (`stands_in_for_listed`): a Nextcloud or ownCloud placeholder by its time (within 2
  seconds, or whole hours apart on FAT and exFAT), an iCloud one by the size it records
  (`NSURLFileSizeKey`), which for a package is the size of all its listed files together. One
  that says otherwise, or nothing, is judged by its time like any other file. Dated folders
  older than *Keep deleted files* (30 days by default; 0 keeps everything) are emptied only
  after a run the person confirmed, whose list named them, went all the way (`Trash::sweep`),
  and only those still older than the setting as it is then: a confirmed list can wait for
  another sync, and a setting changed meanwhile counts. On Windows, FAT and exFAT won't remove a
  read-only item, so when the drive refuses, the marks are lifted and the folder removed once
  more (`Trash::clear_out`), never through a link and never for a file with more than one name,
  whose mark is the same under its names outside the trash. Runs nobody looked at never empty
  the trash. If `.charon-trash` is a link or a file, nothing is put into it, and a step that
  would have put something there fails. An item whose path in the trash would be too long for
  the drive goes in under its own name at the top of the run's dated folder (numbered if
  needed), and the app's log says so.
- **Leftovers:** what an interrupted sync left (`.part` files, link copies under a temporary
  name, and the empty files a failed rename left as claims) is moved into the trash by a run
  that went all the way, once nothing has written to it for an hour and, for a temporary name,
  its copy started more than an hour ago. A claim's date is the marker's, so only its change
  time can tell how old it is: where the drive keeps change times Charon trusts (most local Mac
  and Linux drives), a claim goes an hour after it was made; on every other drive, the network
  shares and drives run by a program where claims are made among them, it goes at the first
  such run. The job's log says how many ("moved 2 leftover files of interrupted syncs into
  .charon-trash"). Files named like Charon 0.2's half-copied files (`*.charon-partial`) may be
  the person's own, so only a list that names them moves them.
- **Durability** (`run/durable.rs`, `Executor::flush`): at the end, every folder the run changed
  (and the trash's lists) is flushed to its drive, both folders are checked once more, and then
  each drive is flushed. Each file is flushed as it's written; flushing folders and whole drives
  is as good as the system allows. On Windows there's no drive flush, and folders are flushed
  where Windows lets Charon open them. Drives that can't flush a folder (many network shares) are
  taken at their word and write it through in their own time.
- **The last look** (`Executor::retrace`): once the drives are flushed, the executor looks again
  at what every finished step left, reading only the items' details (on a network share or a
  drive run by a program, a file a step finished less than a minute ago is opened, so the drive
  answers rather than what its client kept). A drive pulled out and plugged back in during the
  run, or a share whose server restarted, can come back under the same folder numbers without
  the run's last changes. So a copy or new folder that's gone (or an item of another kind in its
  place), a version a copy replaced that's back (told by its change time even when size and date
  look alike, where the scan trusted the drive's change times) or an older one, or a removed
  item that's back fails the whole run with `Error::Undone`: nothing of it is recorded, and its
  next look compares both folders afresh and waits for an OK. Only what a lost change leaves
  counts; an edit made after a step, a drive rounding a time or a rename moving a change time is
  a change for the next look. In a two-way job, the original each copy was made from must still
  be there and not older. One moved, removed or found older meanwhile doesn't fail the run: it's
  only left out of the new memory (`Outcome::unrecorded`), so the next look compares that item
  afresh. A copy whose original is gone is then copied back rather than removed, and an older
  original meets the copy as two versions the job's conflict rule settles. Both folders are
  checked once more after this look. Until all of it succeeds, nothing about the run is
  recorded.

### What is recorded after a run

`engine/runs.rs`. Before anything is carried, the job's record is marked `run_in_progress`; if
that can't be saved, the run changes nothing. Once the run is durable:

1. Both markers must still be what the survey found. If one changed (another drive came in under
   the same name), nothing about the run is recorded, the two-way memory is forgotten, and the
   next sync waits for an OK.
2. Two-way: the markers are written on both sides (the same tokens with the counter one higher,
   or new tokens and counter 1 when the memory wasn't trusted), then the memory. The new memory
   holds what each finished step left behind, the items both sides already agreed on as the plan
   saw them, and the old record for everything else (a skipped or failed step, an open conflict,
   an unreadable place, a run that stopped early), so the next run decides again instead of
   copying a removed file back. A copy whose original was gone or older by the end of the run
   (see *The last look*) gets no record at all, so the next look compares it afresh and never
   removes the copy because of it. If a marker or the memory can't be written, the memory is
   forgotten, and the next look compares both folders afresh and waits for an OK.
3. Mirror and Backup: the destination's marker, the copies whose dates didn't stick, the
   source's marker where Charon can write there (its counter goes up with every sync), and then
   the tokens in the job's record.
4. The job's record always gets the run's summary, and a run that ended without an error and
   wasn't stopped also sets the time of the last successful sync. A recorded run clears
   `run_in_progress` and, unless it only settled one conflict, saves the drives both folders
   were on; a run that couldn't be recorded leaves `run_in_progress` set. A run nobody looked at
   adds what it removed or replaced to the day window and the long count.
5. After a clean sync the person confirmed, the job no longer waits for an OK (so a job with full
   automation runs by itself from then on), and the day window and the long count start again.
   After *Stop*, a run that couldn't be recorded, a long count that couldn't be saved, or a sync
   that ended on an error of Charon's own, the next sync waits for an OK and the row says why. A
   run stopped because Charon quit carries on as before next time.
6. The job's activity log gets the summary, the items that failed, were skipped, wait with their
   app library or database, or were left out, the files the run couldn't read, the conflicts
   *Newest wins* settled, the copies left out of the memory because their original was gone or
   older, and the files that arrived without details their drive can't keep (up to 50 of each,
   skipped, waiting and left-out items sharing one 50).

### When jobs run by themselves

`core/src/autopilot/` starts three seconds after launch and looks at every job once a second. A
job **runs by itself** only when *Full automation* is on, its next sync doesn't wait for the
person's OK (`Job::runs_by_itself`), it isn't paused, *Pause everything* is off, and no list of
its changes is being prepared or waiting. A new job waits for a confirmed list first, and so
does a job whose folders, mode, conflict rule, excludes or safe deletes changed, that just had
full automation turned on, or that was held back or stopped. In the job editor, changing the
mode, turning safe deletes off or swapping a one-way job's folders also turns *Full automation*
off; it has to be turned on again, with its two warnings. Every other job is only *looked at*:
Charon scans and plans in the background, the row shows *Needs your OK* when changes wait, or
*Files to compare* when all a look found is files it couldn't compare yet (unless it shows
*Syncing*, *Offline*, *Paused*, *Conflict* or *Error*, which come first), and nothing changes in
its folders until the person confirms a list.

Each tick first starts, for every job, what waits for its turn: a list the person confirmed
while another sync worked in its folders (`start_confirmed`), or a run nobody looks at that a
trigger started and that waits (`start_wanted`). Only then does it go through the steps below
(on a drive run by an add-on driver, jobs take turns in the order they began to wait anyway:
see *Drives run by other makers' drivers*). For each job whose folders are reachable (a folder now on another drive than at the last sync doesn't
count, nor one that doesn't answer in time: see *Folders that don't answer* below) and that
isn't syncing, in this order:

1. **Drive connected:** a *Drive connected* job whose folders just became reachable (at launch
   too) syncs, or is looked at.
2. **Changes settled:** the folder watcher reported a change that matters (not an excluded item,
   not Charon's own files, and not the job's own sync writing: see *Changes made during a sync*
   below), and the folders have been quiet for the job's settle time (*On change*) or 5 seconds
   (any other trigger), since the last change heard and since the job's last sync ended. An *On
   change* job that runs by itself syncs; every other job is looked at.
3. **Schedule:** an *Every…* or *Daily* job is due (not while it or everything is paused).
   Schedules count from the last run, the last time they came due, or the job's creation,
   whichever is latest, so a computer that was asleep catches up when it wakes. A time later
   than now (taken while the clock was ahead) doesn't count, and with none left the job is due
   at once (`Job::schedule_reference`). Daily times are
   local. On a day when a daylight-saving change skips the chosen time (02:30 on the night the
   clocks go forward, for example), the job doesn't run that day; the next run is at the same
   time the following day.
4. **Removals left for later** (below) have come due: a job that runs by itself syncs once more,
   whatever its trigger.
5. **Regular look:** every *Look for changes* minutes (15 by default), in case the watcher missed
   something. An *On change* job that runs by itself and has changes found by a look (files the
   look couldn't compare count too, `has_pending`) syncs, unless its folders are still changing
   (step 2 then syncs once they settle) or its last sync ended less than its settle time ago,
   since what an app saved into that sync's own items meanwhile isn't heard. Folders that never
   settle (an app that saves every few seconds) still sync here, once they have kept changing for
   a whole *Look for changes* interval since they began to and since the job last synced.

*Sync now*, shown instead of *Review and sync* for a job that runs by itself, starts the same
kind of run. A run nobody looks at makes a fresh survey and goes ahead only when nothing holds
it. Otherwise it's **held back**: nothing is touched, the log (and, with *Notifications* on, a
notification) says why, and the job shows *Needs your OK* until the person confirms a list.

**Pausing** a job stops its running sync as *Stop* does, calls off its confirmed list that waits
for another sync, and drops the run it wanted (`Engine::stop_paused`). *Pause everything*
(`set_all_paused`) is saved first, then, all under one lock (`pause_everything`), it counts the
pause (`Engine::pauses`), stops every running sync nobody confirmed a list for, as *Stop* would
(the job's log says "everything paused: this sync stops, since you didn't confirm its list"),
drops every wanted run, and calls off every confirmed list that waits for another sync. A sync
the person confirmed that's at work goes on. A confirmed list whose sync is about to start
counts as running then, so it isn't called off here, but it sees the count went up and is called
off where it ends up (see *What waits* below). `begin_run` refuses a run a trigger started while
the job or everything is paused, so a run that was about to start doesn't. Runs the person asks
for (*Sync now*, *Sync all*, a confirmed list) aren't refused this way: *Sync all* leaves out
jobs paused on their own, but under *Pause everything* it still starts the jobs that run by
themselves, and such a run still waits for another sync and starts once it's done. A pause that
comes while *Sync all* is starting jobs wins: `want_for_sync_all` checks, under the locks
pausing takes, that the job isn't paused and that everything wasn't paused since *Sync all* read
the jobs (it counts the pauses first). After *Pause everything* it starts no more jobs, and the
window says "You paused everything while Sync all was starting the jobs, so it started no more
of them."

Removals that only follow from an item missing for less than ten minutes on a drive that may
show changes late (a network share, or a drive run by a program, like sshfs and cloud drives
that mount as a FUSE drive) are left for a later automatic run (`engine::Missing`); a list the
person confirms shows them all. A disk of the computer's own that a program reads (`fuseblk` on
Linux: a USB disk through ntfs-3g or exfat-fuse) doesn't count as such a drive. The ten minutes
are counted by Charon's own clock (`Moment::own`, an `Instant`), which a clock set forward or
back doesn't move. How long looks have found each item missing is saved per job in
`absences/<job>.json` (`countedSeconds`), so the wait survives a restart: the next launch counts
on from there (`Missing::restored`), and the time while Charon wasn't running doesn't count, as
its own clock can't tell how long that was. When a look first found the item missing (`since`,
by the clock on the wall) is kept only for whoever reads the file, and a record an earlier
Charon wrote without the count starts its wait over. Whenever Charon begins to look at the
folders again (at launch, when a folder comes back, and after a tick that came 30 seconds or
more after the one before by either clock, because the computer slept or the clock jumped:
`after_a_gap`), an item found missing before must also be found missing by the looks since then
for two minutes (`Missing::look_again`). A database's files only go together, so every other
change to a database one of whose removals waits is left for later with it, and the log says so
("1 more change to the same database waits with them"). Until they're carried out the row shows
*Pending* with their count (those changes included), never *In sync*, and the run's summary
counts the removals ("2 removals left for a later sync"), in the log and in the window's
last-run line. Cloud folders on the computer's own disk (where iCloud Drive, Dropbox and
OneDrive usually keep them) count as local: this rule doesn't cover them, and *Newest wins*
trusts their times.

A job whose last sync is dated later than now synced while the clock was ahead, or the clock is
behind now, and which of the two can't be told. Each tick the autopilot notes such a sync, for
every job, offline ones too (`note_syncs_dated_ahead`): the date stays as it is, and Charon
counts for how long it has known so, by its own clock, until the job syncs again (`FoundAhead`).
The row shows the sync as made at the earlier of the date and the moment Charon found it later
than now, counted back from now by that count (`Engine::last_synced`, `FoundAhead::latest`),
even once the clock is past the date, and "Not synced for 3 days" (`Engine::stale`) goes by the
same date, so it doesn't wait for the clock to reach the date kept. The count is saved with the
job (`datedAhead` in `charon-state.json`) when the date is found and then at least every 10
minutes (`DATED_AHEAD_SAVED_EVERY`), but not while the state can't be saved, and the next launch
counts on from there while it's still about the job's last sync. The time Charon wasn't running
doesn't count, so after a restart the row may show the sync as more recent than it was.

**Folders that don't answer** (`engine/reach.rs`): a network share whose server stopped
answering can hold up any question about a folder on it (is it there, which drive is it on,
where does it lead) for minutes, or for hours on a "hard" NFS mount. So the questions Charon
asks all the time (the autopilot every second, every row the window shows, *Sync all*, and
every sync that starts and works out which other jobs share its folders) are asked on threads
of their own and waited for at most 10 seconds (`reach::LIMIT`). A folder that doesn't answer
by then is `Presence::Silent`, and isn't asked again while that question is still out. Its job
counts as *Offline*, and its row says "The source folder doesn't answer. If it's on a network
share, the server may be off or out of reach." The autopilot leaves it alone, a list made before
can't be confirmed, and once it answers again it is looked at afresh. Since where it leads can't
be told, it counts as sharing a folder with every other job (`Engine::neighbours`,
`may_share`), so a sync of it that is still at work keeps the others waiting, and the check for
jobs that would undo each other compares only the folders that answer. *Quit*, *Stop* and
closing a list don't ask the disk on the window's thread. A survey or a sync that is already at
work isn't time-limited: one stuck on such a share stays stuck until the share answers, and
quitting waits for it only once, at most 30 seconds (`Quit::stop_syncs`), before Charon exits.

When changes or conflicts wait for the person, or files a look couldn't compare yet, Charon says
so in one notification ("“Videos” has 1 file to compare" when that's all a job has), reminds
them after two hours, and then once a day (`Engine::remind`). Paused and offline jobs aren't
mentioned, and right after a start Charon waits for the first looks (at most two minutes), so
the jobs found waiting come in one notification. The tray's status line (`Overview::headline`)
names each kind ("1 job waits for your OK, 1 has files to compare"), and the header, the tray's
*Review Waiting Jobs…*, the Dock badge and the quit prompt count both (`Overview.to_compare`,
`Overview::lists_waiting`). A job that was held back waits for an OK, whatever else its look
found. A job that syncs by itself and whose look found only files it couldn't compare yet
waits for nobody. It's counted apart from the jobs with changes queued
(`Overview.pending_compare`, `JobView::compares_at_next_sync`): the tray says "1 job with files
to compare at its next sync" and the header "1 job has files to compare at its next sync", and
both name both kinds when both exist (the tray: "1 job with changes waiting, 1 with files to
compare at its next sync"; the header: "1 job has changes queued · 1 job has files to compare
at its next sync"). A confirmed list that waits for another sync still counts as changes queued,
since that sync carries out exactly the list.

### Overlapping jobs and other Charons

Two syncs must never change the same files at once, and a job must never undo another job's
work unnoticed, whatever the folders' paths say. Paths can't always tell: a link, a drive or
share mounted in a second place, or a folder made after the job was saved can make two paths
one folder, and jobs saved by Charon 0.2 were never checked at all. So paths are checked when a
job is saved, and every survey checks again by the folders themselves and by Charon's markers in
them (`engine/overlaps.rs`, `Engine::overlaps`).

- **A job's two folders.** Saving refuses two folders that are the same, or one inside the
  other, by their paths (`JobSetup::validate`, with `Folder::of`: links and `..` resolved as far
  as the folders exist, Unicode NFC, and case folded where the drive ignores it). Every look then
  asks `Engine::nested`, which goes by the folders themselves: `RootId::is_within` walks up from
  one folder's real path and compares each folder's `RootId` with the other's through
  `RootId::same_place`, which leaves the mount out, so the same folder seen through a bind mount
  still counts as itself. A folder reached through a bind mount of a folder inside the other
  isn't found this way (walking up its real path never meets the other folder), and a share
  mounted twice can show other folder numbers under each name. For both, `nested_by_markers`
  looks for the job's marker of the other side in each folder (the scans note every `.charon`
  folder they meet, with the markers in it: `Snapshot::charon_folders`) and in the folders above
  its real path (`markers_above`). Only a marker the job left at its last finished sync counts
  (`left_by`: its token is the two-way memory's, or the Mirror or Backup token in the job's
  record; `Baselines::tokens` keeps each memory file's header, its folders and tokens, once read
  or saved, so a memory file is read at most once while Charon runs, not at every other job's
  look), so the first sync of such a job isn't refused. Either way it's the survey's
  **refusal**: the list can't be confirmed, and an automatic run is held. A list that settles one
  conflict (`prepare_resolution`) keeps it too, whichever way it was found.
- **Copies of a `.charon` folder.** A marker found where it doesn't belong may be a copy: the
  person copied a job's folder, hidden `.charon` folder and all (restoring a backup, say).
  `Marker::origin` reads both markers again: two different ones are two folders. Two identical
  ones are one folder or a copy made since the last sync, which nothing in them tells apart, so
  Charon creates a file `.charon/<uuid>.probe` in one of the two Charon folders, looks for it in
  the other, and removes it at once (`Marker::same_place`, which never makes a folder). A
  question stopped meanwhile (its look was stopped, or a sync began to keep the drive of either
  folder quiet) leaves it, which does no harm (each probe has a name of its own, and
  `Marker::forget` removes the probes left in a `.charon` folder with its last marker). A copy is
  harmless: the list only mentions it, in an info warning saying it can be deleted. When Charon
  can't tell (neither Charon folder takes a file, or a marker can't be read), the job's own
  marker of its other folder means a refusal; another job's marker means a clash warning that
  the folders may be one place (it needs the tick and holds automatic runs).
- **Jobs that undo each other's work.** A Mirror job removes from its destination whatever isn't
  in its source, so its destination must not share a folder with another job that writes there
  (`JobSetup::clashes_with`; `writes_to`: a two-way job writes into both folders, Mirror and
  Backup into the destination). The other job would bring files in, the Mirror would remove them,
  and a two-way job would carry the removal back. Jobs that only read a folder don't clash.
  Adding, editing and duplicating a job refuse a setup that clashes by paths
  (`JobSetup::clash_message` for adding and editing; duplicating says a copy would share its
  folders, and a copy of a Mirror job always would). The check asks the disk, so it
  runs before any lock is taken (`Overlap::of` in `engine/jobs.rs`), and the save is refused
  ("Save it again") when another job was added, or its folders or mode changed, in between
  (`Overlap::clash`). A clash saved by an older Charon doesn't block an edit that keeps the job's
  folders and mode (`JobSetup::same_folders`), such as a new name. Every look checks again: by
  paths, and by the markers other jobs left in, at the top of or above this job's folders
  (`clashes_by_markers`, which asks only about jobs that could undo each other's work
  (`undoes`), and goes by `Marker::origin` too: a copy is only mentioned, and a job whose own
  folder is away can't sync now, so nothing is said). Between jobs there's no check by `RootId`,
  so a clash through a second mount shows only once the job whose marker is found has finished a
  sync (`left_by`). A clash isn't a refusal: it's a danger
  warning that holds an automatic run and needs the tick, so the person can still check the
  removals and sync.
- **Busy notes** (`run/busy.rs`). Charon keeps its own running jobs apart by their folders
  (`busy_neighbour`, with `Engine::neighbours`, which resolves every job's folders with the time
  limit of *Folders that don't answer*: a job whose folders don't answer counts as sharing with
  every other), but paths don't show a share mounted under two names, and another Charon (a
  second computer syncing the same share, a development build next to the installed app) can't
  see this one's runs at all. So right before a sync changes anything (after its survey; for a
  confirmed list, before the person is told it started), `Engine::claim_folders` takes a
  `Claim`: it writes `.charon/<uuid>.busy` (a new name for each run, the same in both folders)
  at the top of both folders, wherever Charon can write there, never through a link (a folder it
  can't write to, such as a card or a read-only share, carries no note, so other Charons can't
  see this sync there). The note (`BusyNote`, JSON) names the job (id and name), the Charon it
  runs in (`instance`, a new id each time Charon starts), the computer (`host`), and holds a
  `heartbeat` in UTC by that computer's clock. Then the claim looks for other syncs' notes
  (`BusyWatch::around`) in the `.charon` folders of both folders, of every folder above their
  real paths, and of the folders inside them that hold a `.charon` folder of Charon's
  (`Engine::folders_inside_now`: other jobs' folders, maybe under other names). Those are the
  ones the run's survey met (for a confirmed list, the list's), and the ones found right before
  the claim at the top of every folder the plan changes something in: folders that aren't there
  yet and links are skipped with everything inside them, and a folder only named `.charon` that
  holds the person's files is left alone (`Scanner::looks_like_charons`). The claim leaves its
  note in those inner `.charon` folders too, never making one: only while the `.charon` folder
  and every folder on the way to it from the job's folder are folders of their own, not links
  (`is_real_folder_inside`; the job's folder itself may be a link). A sync of a folder inside,
  reached through a share or mount that starts lower down (a NAS "homes" share and a "home"
  share, an NFS export of a subfolder, a bind mount), can't look in the folders above it, and
  finds the note at its own top instead. A folder inside that has no `.charon` folder at that
  moment gets no note, so a sync of just that folder that starts later through such a share
  doesn't see this one. Another sync's note means the folders are busy: the claim takes its own
  notes away again, and the run ends before anything changed (`not_begun`). Two syncs that start
  at the same moment each find the other's note, so at most one goes ahead. The claim looks only
  this once: nothing checks again while the run lasts. Meanwhile a thread writes the notes again
  every 20 seconds (when it can't start, the notes go stale after 2 minutes); on a drive the sync
  keeps quiet, the sync's own thread does it as it goes, and the thread only once that's a
  minute overdue (`Claim::beat`, `QUIET_OVERDUE`, see *Drives run by other makers' drivers*). Dropping the claim removes them. A refresh never makes a folder (`atomic::write_into`): a `.charon` folder that
  went away with its drive would otherwise be made again on the computer's own disk. Its note is
  written again once the folder is back. Nor does a refresh write through a folder replaced by a
  link while the sync runs. `begin_run` and `begin_look` (a list being prepared) also read the
  notes before they start (`folders_in_use`, with the folders inside from the last look,
  `JobRuntime::folders_inside`), and refuse with `Error::FoldersBusy`.
- **Which notes count.** Two computers' clocks can be minutes or hours apart, so whether a
  note's sync is still at work is judged by this computer's clock alone: `BusyWatch` remembers
  when it first saw each note as it is now, and a note unchanged for 2 minutes belongs to a
  Charon that stopped. It counts for nothing and is removed. The heartbeat inside is only looked
  at for whether it changes. A computer that sleeps mid-sync stops refreshing its notes too: 2
  minutes later another Charon takes them away and may sync, and when the computer wakes, its
  run carries on and its heartbeat writes the notes back, so two syncs then run at once. This
  Charon's own notes count exactly while their claim lasts. Notes left by a Charon that worked
  with this data folder on this computer before (a sync cut short by a crash, or by a quit that
  didn't wait) count for nothing at once: only one Charon works with a data folder at a time
  there. `instances.json` lists the running one and up to 19 before it, newest first, each with
  the computer and the data folder it ran with (`Launch`), and only those that ran on this
  computer with this very folder count as stopped (`ran_like`). So a data folder restored onto
  another computer, cloned or copied doesn't make the notes of a Charon still at work with the
  original look like leftovers. Only a copy on a computer with the same name, at the same path,
  can't be told apart from the original. A list that can't be read (or one in the older form,
  plain ids) is started afresh, and when the list can't be saved, such a note counts for its 2
  minutes. A
  `.busy` file that can't be read counts as long as the file keeps changing, by its modification
  time, and is never removed: it may not be a note at all, and waiting a little is better than
  syncing next to another sync. The row names the other sync as its job's name when it's this
  Charon's, as "another Charon" when it's on this computer, and as "Charon on <computer>"
  otherwise (`busy_with`); a note that can't be read shows as "another Charon" too.
- **What waits.** A run the autopilot or *Sync all* started that finds its folders busy stays
  wanted (`JobRuntime::wanted`, with `waiting_for` set), and `start_wanted` tries it again at
  every autopilot tick while the job runs by itself and no list is open; the row says *Waits until
  “X” is done, then starts by itself*. *Sync now* is refused with `Error::FoldersBusy`
  (or `Error::DriveBusy`) instead, unless the note turns up only at the claim: then its run stays wanted too. A `Wanted`
  run keeps its reason and whether the person asked for it (`by_hand`: *Sync all*, or *Sync now*
  that met another sync). It begins only while the job still wants it as it was read, checked
  under the lock that marks the run as running (`still_asked_for`, `Wanted::asks_as`), so
  pausing calls it off for good, and a refusal other than busy folders drops it only while it's
  still so. Pausing the job drops it even when the person asked for it; while everything is
  paused, the autopilot drops only the runs a trigger wanted (`unwant_automatic`), so a run
  *Sync all* asked for still waits and starts once the other sync is done. A wanted run that is
  dropped while it waited says so in the job's log ("the sync that waited for “X” was called
  off: the job is paused"). A list confirmed while another sync is busy isn't
  refused: `approve` returns `Approval::Waiting` (`{ kind: "waiting", waitingFor }` for the
  window) instead of `Approval::Started`, and `queue_confirmed` keeps it as
  `JobRuntime::confirmed`, with whether the tick was given. The dialog's toast says "Confirmed.
  “job” syncs as soon as “X” is done." `start_confirmed` tries it again at once when a sync of
  this Charon ends (`start_waiting_confirmed`), and from the autopilot every 5 seconds
  (`CONFIRMED_RETRY`), since another Charon says nothing when it's done. A list that waited goes
  through every check of *Confirm* except its age and the changes made in the folders since: it
  was confirmed in time, and each step still checks its items. Since it was made before the
  wait, the job is looked at again at once after it ran (`Crossing::outdated`; the row shows
  *Looking*), and the trash it empties is only what's still older than *Keep deleted files* as
  it is then. A folder away for the moment, the job's own sync, or another sync still at work
  keeps it waiting. A folder now on another drive than at the last sync doesn't stop the
  autopilot's tries (the person saw that in the list): `carry_out` checks that the folders are
  still the ones the list was made for. One that fails any other check (a folder that isn't the
  one the list was made for, the job changed or synced, an older Charon at work) is dropped, and
  the row says why ("The list you confirmed couldn't be carried out after all." followed by the
  reason). *Stop* (`cancel`, which drops `JobRuntime::confirmed` but keeps the list), pausing
  the job (`stop_paused`), *Pause everything* (`pause_everything`: the person now wants nothing
  to start without them), discarding the list (`discard_review`), editing the job,
  *Leave this folder out*, a newer list (`JobRuntime::forget_review`) and quitting Charon call
  it off, and the job's log says so: "the confirmed sync that waited for another sync was called
  off", followed by the reason in brackets except when the person did it directly (*Stop*,
  closing the list, pausing the job). A list confirmed while *Pause everything* is on still
  syncs, like any list the person confirms. Calling off wins even when it comes just as the sync
  starts: `carry_out` starts a list that waited only if it's still `JobRuntime::confirmed`,
  checked under the same lock that `cancel` and `stop_paused` take. *Pause everything* finds a
  list whose sync is about to start marked as running, and leaves it; the list then sees that
  everything was paused since it was confirmed (`Confirmed::pauses`) and is called off where it
  ends up: it no longer waits for the other sync (`keep_waiting`), its sync ends before it
  changes anything (`claim_for_list`), or, confirmed just then, it isn't kept at all
  (`queue_confirmed`, and the window says "You paused everything just as this list was about to
  start, and “X” isn't done yet, so the list doesn't wait for it. Look again once it's
  done."). It lives only in `JobRuntime`: quitting Charon drops it, and after
  a restart the job is where it was before the list was confirmed. While it waits, runs nobody
  looks at are refused, and the row (`JobView::confirmed`) has no `waiting`: it shows *Pending*
  when its look found changes, unless a folder that's away, *Pause everything*, conflicts, or a
  problem or failed last run outrank it; it isn't counted as waiting for an OK in the header,
  the tray, the reminders or the quit prompt (`quit.rs` counts `Overview::lists_waiting`, so
  quitting doesn't ask about it); its main button is *Stop*, and it says *Confirmed: starts as
  soon as “X” is done (Stop calls it off)* (`JobRow.waitsFor`).
- **Looks that came too late.** `JobRuntime::epoch` goes up whenever what a look under way would
  find becomes outdated: a run begins, the job is edited or removed, a list is being made
  (`outdate_looks`), or a folder comes back (`forget_look`). A look notes the epoch when it
  begins (`check` reads the setup and the epoch together, as an edit changes both; `begin_look`
  for a list), and `note_look` records what it found only while the epoch is the same, no run is
  going and the setup is unchanged; `keep_review` keeps a list only then, and otherwise says the
  job or its folders changed while it was made. `end_check` ends a background look: an error
  counts only for a current look, and a look outdated meanwhile doesn't count as one (it doesn't
  set `last_checked`). An edit, *Leave this folder out* or a folder that came back clears
  `last_checked`, so the job is looked at again as soon as nothing else is looking (the
  autopilot asks every second); after a run, its end sets or clears it (see *Changes made during
  a sync*), and a list counts as the look (`note_look`). Whatever ends after its job may have
  been removed changes the live state through
  `change_known_runtime`, which only changes a job's live state while it has one, so it leaves
  nothing behind; `change_runtime` makes one when it's missing.
- **Changes made during a sync.** From the moment a run begins, `JobRuntime::own_writes`
  (`OwnWrites`) holds what it writes: its plan's items and every folder above them, once it
  knows its plan (before that, every change is someone else's), and each conflict copy right
  before it's made, since the plan doesn't know its name (`Executor::telling_new_names`). A
  change the folder watcher hears while the sync runs, or in the 3 seconds after
  (`note_change_while_syncing`), is the sync's own when it's at one of those, and doesn't count.
  Any other change counts at once, so the folders aren't quiet and an *On change* job waits for
  them to settle; during the sync it's also kept for its end, which then has the folders looked
  at again at once, and a run that ended without carrying anything does the same
  (`look_again_if_changed`).
- **Drives run by other makers' drivers** (`engine/quiet.rs`). On a Mac, a drive whose file
  system isn't one of Apple's own (Paragon's or Tuxera's NTFS, Paragon's extFS, anything through
  macFUSE: `Volume::run_by_an_add_on_driver`, which also counts a type with a bundle in
  `/Library/Filesystems`) is run by a driver inside the system's core, where a mistake brings
  the whole Mac down. Paragon NTFS for Mac 17 did, a few seconds into a sync onto a USB stick:
  "Mutex to destroy still has waiters", with only the sync's thread of Charon's in the file
  system, Spotlight (`mds`) waiting for a lock of the driver's, and `fseventsd` at work for
  Charon's folder watching. Charon can't fix the driver or keep the system's services away, so
  this only lowers the odds; the cure is the driver's newer version. Charon works on one job at
  a time on such a drive. Which drive a folder is on is known without asking, from the disk's
  last answer about it (`whereabouts_of`, which the autopilot asks every second; a folder never
  asked about counts as on no such drive). While a job's run (its look, then its sync) or a
  list being made for it is going, another job with a folder there isn't looked at, listed or
  synced (`Engine::drive_used_by_another`): `begin_run` refuses with `Error::DriveBusy` after
  the caller's checks (so a list that can't go ahead anyway is refused, not kept waiting), the
  run waits like one that met a busy note (*What waits*), `begin_look` refuses the same way,
  `carry_out` runs a confirmed list's own checks and then lets it wait without reading the
  drive, and `check` leaves the job for later (`last_checked` stays unset, so it's looked at as
  soon as the drive is free; *Look for changes* says why, `look_now`). Who asks decides what
  holds them back (`quiet::Asker`). What Charon starts by itself, a background look (`check`)
  or a run a trigger started (neither `by_hand` nor `approved`), also waits for another job's
  background look there (or a job that syncs often could keep stopping a long look of the
  other's for good), and for its turn: a job held back there notes when it began to wait
  (`JobRuntime::drive_wait`, `note_drive_wait`), and waits for every job there that began to
  wait before it and still asks, whether to look or to sync, whatever the order of the jobs
  (otherwise two jobs listed first whose folders keep changing could take turns looking and
  keep a third waiting all day). The autopilot asks again at every tick, and counts its ticks
  (`note_a_tick`): a wait it didn't ask for in this tick or the one before lapses
  (`DriveWait::asks`; in ticks, not time, since one tick can take long while a share is slow
  to answer), one whose job's folder went away is forgotten at once (`forget_drive_wait`), and
  so is a run's once it's no longer wanted (`forget_wanted`), so it holds nobody up. The wait
  ends when the job gets the drive (`begin_run`, `begin_look`, `look_in_background`). A
  refused *Look for changes* starts a wait too, which the autopilot takes over as its own
  look's; it waits for another job's look, but never for its turn. A job held back is told
  the name of a job at work there before that of a job that only waits (the row says it waits
  until that job is done). A background look that stopped moving on holds nobody back: one
  that scanned no item, compared no pair or piece of a file, went through nothing in a trash
  and tried no file it couldn't read before again for two minutes (`STALLED_FOR`,
  `JobRuntime::look_moved`, `note_look_moved`; the scanner says how far it got at least every
  5 seconds while items come) is most likely waiting for a folder that doesn't answer, not
  reading the drive. Whoever goes ahead of it stops it, or it could read the drive again once
  that folder answers: a stopped look ends before its next step on either folder, or on
  another job's (`look_at` asks before each, and so do `Contents::same`, `compare_contents`,
  `apply_refusals`, `apply_unread`, `overlaps` with `Marker::origin`, and
  `Trash::look_going_on`; only a step under way when it was stopped finishes). A stopped
  comparison keeps what it read while both folders still carry their markers, but reads no
  marker while another job's sync keeps the drive quiet, and forgets it instead
  (`stopped_comparing`, asked again before each marker; the job's own sync counts too, which a
  look it stopped may outlive); the files it still has open there are
  closed once that sync is done (`Engine::let_go`, as closing one is work for the driver too).
  Whether another job's folder holds a copy of its marker is asked with a time limit
  (`Engine::ask_about`), and that question goes on by itself when it's late, after the look
  ends or is stopped, but stops before its next step once a sync keeps the drive of either
  folder quiet. A sync doesn't wait for a stalled look to end (`let_looks_end`). A slow
  look that keeps reading holds the drive however long it takes. A
  run or a list the person asked for waits for no look and no turn: it stops the background
  looks there instead (`stop_looks_sharing_a_drive`; they count for nothing, and come again).
  Its folders there are still asked about meanwhile (where they are, and whether they overlap
  another job's) until a sync keeps the drive quiet. `remove_job` is
  refused while another job's run or list uses the drive, naming it (*Remove “X” once it's
  done*), since removing takes Charon's marker out of its folders; it doesn't wait or retry, and
  a background look there doesn't hold it.

  While a sync writes to such a drive, its thread is meant to be the only one of Charon's at
  work there. `claim_folders` first keeps the drive quiet (`Engine::keep_quiet`, by where it's
  mounted, or only the job's folder when that can't be told; another job's sync can't keep it
  quiet too, even for drives mounted one inside the other), and waits at most 5 seconds for
  looks that began before Charon knew the drive (`let_looks_end`). Meanwhile `whereabouts_of`
  and `folders_of` answer for folders there from memory: the sync's own folders are where its
  look found them, another job's as last asked (`Presence::There(None)` or `None`, which counts
  as maybe one place with anything, when never asked), what another job's look would ask
  about a folder there (whether it holds a copy of a marker, `Engine::ask_about`) isn't asked:
  the last time the disk told them apart, a copy or the folder itself, holds
  (`QuietDrives::origin`), and a pair never told apart this session can't tell until that sync
  is done (`overlaps::Waiting`): the list says so, with the careful clash warning. A run nobody
  looks at that has no other reason to hold back (`Survey::waits_on`) isn't held back for the
  person's OK but waits for that sync without looking again meanwhile (`JobRuntime::waits_on`,
  checked in `start_unreviewed`, from memory first where nothing else would refuse the run),
  and the job's log says once what was found where and which job is syncing; with another
  reason, it's held, these clashes coming after the other reasons. A scan doesn't ask the drive
  where a folder a sync client syncs there really is (`Scanner::keeping_off`): it's taken as
  the client wrote it. Saving a job checks its folders
  against each other and against other jobs' as last found there, or waits when one was never
  found (`resolving_for_saving`, `folders_for_saving`, `JobSetup::validate_as`). The busy notes
  there are refreshed by the sync's thread between steps and between the pieces of a file
  (`Claimed::beat`); the heartbeat refreshes them only when that's overdue (`QUIET_OVERDUE`, a
  minute: a big file copied into the trash, a slow flush), so they never go stale while the sync
  runs. The autopilot stops watching folders there (`Autopilot::routes`), and the sync waits at
  most 3 seconds for that before it writes (`wait_until_unwatched`; a confirmed list the
  autopilot starts itself claims its folders on its sync's own thread, so the wait works for it
  too). When either wait runs out, the sync writes all the same, and the app's log says so.
  What changes in the folders there meanwhile isn't heard, as on a network share, and isn't
  counted as a change: the next regular look finds it. After a sync that left nothing, that
  look counts from when the folders stopped being watched (`finish` sets the sync's own
  `last_checked` to the keep's start, `Claimed::unwatched_since`); after one that left
  something (a step failed, was skipped or waited, an item was left out because the drive
  can't hold it, a file couldn't be read, or the run ended on an error), it counts
  from the sync's end, however long it kept the drive quiet, so a file that fails every time is
  tried again a whole interval later at the soonest, and the drive isn't written to again and
  again. Other jobs' looks there were held, and come once it's done if they came due. Either
  way a run nobody looked at ends as any run does: its row says what's left. A list of another
  job there made before the keep can still be confirmed, as each step checks its items right
  before it acts, and it's still checked for its age and for changes heard meanwhile; once it's
  carried out, its folders are looked at again (`StoredReview::unheard`, set by
  `let_looks_end`; `Crossing::outdated`). A look
  scans a job's two folders on one such drive one after the other (`scan_both`).
- **An older Charon, and a state that can't be saved.** Two Charons can't work with one data
  folder (`DataLock`). Charon 0.2 doesn't know that lock, nor that its jobs moved: it writes
  `charon.json` again after a sync, and syncs by itself without these checks and without busy
  notes. The engine notes when `charon.json` last changed as it opens
  (`StateFile::legacy_changed_at`; usually it isn't there, since it was renamed), and
  `Engine::older_charon_running` looks again at every autopilot tick and before every run: a
  different date (or the file appearing) means an older Charon is running. Only this data
  folder's `charon.json` is watched, so a 0.2 syncing the same share from another computer is
  never seen. From then on, until Charon restarts, `begin_run` refuses every sync with
  `runs::OLDER_CHARON`, *Confirm* included, and a confirmed list that waits is dropped with it at
  its next try. Looks and lists still work (`begin_look` doesn't ask). When a save of
  `charon-state.json` fails (a full disk), `Engine::save` notes when (`cannot_save`); until a
  save works again, `can_save` refuses runs nobody looks at (`runs::CANNOT_SAVE`: their end
  couldn't be recorded), and tries a save at most once a minute, also from the autopilot, so the
  problem clears by itself. A wanted run refused that way is dropped, not kept for later. A
  confirmed list isn't asked: its run starts, and when `cross` can't mark the job's record
  (`run_in_progress`), it ends before changing anything ("Charon couldn't save its state before
  syncing…"), which uses the list up. `Engine::problem` turns either into the sentence in
  `Overview.problem`. The tray's status line and tooltip (`Overview::headline`) and the header
  (`header.js`) then say "Charon needs your attention" before anything else, and the header
  shows the whole sentence under the title bar (`#app-problem`). Editing a job, removing it and
  *Leave this folder out* save first (`with_job_at_rest`): when the save fails, nothing changes,
  in the stored state or the live one (a confirmed list that waits keeps waiting), and the
  window says "Charon couldn't save this change (…), so nothing changed."

### Safety rules

- Without *Full automation*, nothing in a job's folders changes until the person confirms the
  list. With it, a job syncs by itself only after a confirmed sync, and needs one again after
  every hold, stop, or change to what a sync does.
- A run nobody looked at goes ahead only when nothing about it needs a look; otherwise it touches
  nothing and waits for an OK.
- A plan that would remove from one side because the other side is empty is refused in every
  mode, and can't be confirmed.
- Without a two-way memory it can trust, Charon removes nothing.
- An edit beats a removal, and a file that went back in time is never "the newest".
- Every step checks both sides again right before it acts; an item that changed is skipped,
  never forced.
- Copies are written under a temporary name and take the real name only when complete, with a
  rename that never replaces what appeared there before Charon claimed the name.
- Replaced files and links always go to `.charon-trash` in two-way jobs and when a conflict is
  settled; with safe deletes, removed and replaced items in every mode do too. An item that makes
  room for one of another kind (a file where a folder goes) goes there whatever the setting.
  When the trash can't take an item, the item stays as it was and the step fails.
- Removals come last, and an item that makes room for another right before that item lands. A
  side where a copy failed, a file couldn't be read or a drive refused an item keeps the run's
  removals, and a copy that didn't land because its file changed, its place couldn't be cleared
  or its app library or database waits keeps those that could be its old copy.
- A folder is removed only when everything in it is removed; folders holding unreadable items, a
  sync client's journal, or excluded items other than system clutter, are never removed (clutter
  such as `.DS_Store` goes to the trash with its folder).
- Places that can't be read, files a run couldn't read, names written two ways, items a scan
  leaves out with a note, and items the other drive can't hold are left alone on both sides, with
  a note in the list. While a file can't be read, no file or link is removed on the side it
  should go to, except one that makes room for an item of another kind, which goes to
  `.charon-trash`.
- Symbolic links are never followed, and nothing is written into a folder Charon leaves alone.
  Never synced: Charon's own files (`.charon`, `.charon-trash`, `.part` files, claims an
  interrupted rename left) and anything named like them or like Charon 0.2's `*.charon-partial`
  files, at any depth (its folders' names in any case, where a drive ignores case); `._*` files
  on a Mac, whatever the patterns say; a Nextcloud or ownCloud client's journal, at any depth;
  names that aren't valid Unicode; and sockets, pipes and devices. An item named exactly
  `.charon` or `.charon-trash` that holds the person's files is left out with a note, never
  taken for Charon's.
- Files that are only in the cloud are never downloaded (macOS and Windows), and a file a cloud
  service freed up and left a placeholder for is left alone, with its placeholder.
- A run stops as soon as a folder isn't the one the plan was made for.
- A job whose folders are one place, or one inside the other, never syncs once Charon can tell:
  by the folders themselves (links, a drive mounted twice), or, after the job's first finished
  sync, by its markers. A Mirror job and a job writing into its destination can't be saved side
  by side, and where they're found anyway (through a link, among an older Charon's jobs, or
  through a second mount once the other job has finished a sync and left its marker), every list
  warns, needs the tick, and holds automatic runs.
- Two syncs of Charon 0.3 don't work in one folder at once, in this Charon or another, on this
  computer or another: a sync leaves its busy notes before it changes anything, and waits while
  another sync's note is in or above its folders, or in a `.charon` folder of Charon's inside
  them (where it leaves its note too). A note is taken for a stopped Charon's only after 2
  minutes without a change on this computer's clock (at once when an earlier Charon on this
  computer with the same data folder left it). A note that can't be read is judged the same way
  by its file's modification time, and is never removed. Charon 0.2 leaves no notes, nor does a
  sync in a folder Charon can't write to, and a computer that sleeps mid-sync stops refreshing
  its notes: after 2 minutes they count for nothing, and when it wakes its sync carries on next
  to any sync that started meanwhile.
- Nothing about a run is recorded until its changes are flushed to the drives (as far as each
  system and drive allows), a last look finds them still there, and only while both markers are
  unchanged.
- Runs nobody looked at never empty the trash, and a confirmed run empties only the dated
  folders its list named that are still older than *Keep deleted files* as it is then.
- Charon keeps 2% of every drive it writes to free, at most 2 GB, where the drive says how much
  room it has.
- Only one Charon works with a data folder at a time: a second one finds `charon.lock` locked
  and refuses to start. (On a disk that can't lock files at all, Charon starts anyway; the data
  folder is on the computer's own disk, where that shouldn't happen.) An older Charon (0.2) found
  writing its state file stops every sync until Charon restarts, and while Charon can't save its
  state, nothing syncs by itself.

## Project layout

```
core/                         the sync engine (crate charon-core, no interface)
  src/
    lib.rs                    the crate's modules at a glance
    error.rs                  Error: one error type, with messages written for people
    model/                    the data Charon keeps and what the window receives
      mod.rs
      job.rs                  Job, JobSetup, SyncMode, ConflictRule, Trigger, the day window
      settings.rs             Settings
      view.rs                 JobStatus, JobView, Progress, RunSummary, Overview
      review.rs               Review, ReviewStep, Warning, WaitingJob, the upgrade notice
    scan/                     reading a folder into a snapshot
      mod.rs
      entry.rs                Entry, Snapshot, and what a scan learned besides the items
      contents.rs             Contents: opening files without downloading them, and comparing
                              two files byte for byte
      cloud.rs                the placeholders cloud services leave for the files they free up,
                              and the journals of Nextcloud and ownCloud clients
      clients.rs              ClientSettings: the folders Nextcloud and ownCloud clients sync
      filter.rs               ExcludeFilter, system clutter, Charon's own names
      keys.rs                 Keys: NFC, /-separated, folded when a drive ignores case
      root.rs                 RootId: which folder a path really is
      scanner.rs              Scanner: walks a folder, noting what it couldn't read
      volume.rs               Volume: the kind of drive, and what it can hold
    plan/                     deciding what crosses, and which way
      mod.rs                  plan_for: the planner for the job's mode
      action.rs               Action, Note, Plan, PlanSummary, the order steps run in
      ground.rs               places a plan must not touch, removals that wait for unread files,
                              and the last look at folder removals
      one_way.rs              OneWayPlanner: Mirror and Backup
      two_way.rs              TwoWayPlanner
      baseline.rs             Baseline and Pair: the two-way memory
      guard.rs                Guard: refusals and concerns, the day window, the long count
      resolve.rs              Resolution: settling one conflict
      units.rs                Unit: app libraries and SQLite databases
    run/                      carrying out a plan
      mod.rs
      executor.rs             Executor: checks each step against the disk, then runs it, and
                              the last look before a run is recorded
      copier.rs               Copier: one file or link, through a temporary .part file
      rename.rs               renames that never replace (rename_new, move_to_new_name), swaps
      identity.rs             Identity: which file an item is, by its drive and file numbers,
                              and how many names it has (hard links)
      durable.rs              flushing files, folders and drives
      room.rs                 Room: free space and the room Charon leaves for other apps;
                              KeptCopies: the room copies kept in the trash take
      trash.rs                Trash: .charon-trash, its lists and its clean-up
      marker.rs               Marker: .charon/<job>.json in the job's folders
      busy.rs                 Claim and BusyWatch: busy notes, one sync at a time in a folder
      meter.rs                ProgressMeter
      names.rs                conflict and trash names that fit a drive's limits
    store/                    what Charon keeps in its data folder
      mod.rs                  DataFolder: where each file lives
      state_file.rs           StateFile: charon-state.json, and the move from charon.json
      baselines.rs            Baselines: each job's two-way memory
      landings.rs             Landings: Mirror and Backup copies whose dates didn't stick
      same_contents.rs        SameContents: two-way pairs a full read found identical
      unattended.rs           Unattended: the long count of automatic changes
      absences.rs             Absences: items missing on a share or cloud drive, for how long
      journal.rs              Journal: one activity log per job
      lock.rs                 DataLock: one Charon per data folder
      atomic.rs               saving a file so a crash never leaves it half-written
    engine/                   the orchestration
      mod.rs                  Engine and EngineEvent: opening, settings, rows, events
      jobs.rs                 adding, editing, duplicating, pausing and removing jobs
      survey.rs               the survey: both folders, the plan and every check
      overlaps.rs             folders that are one place under two names, in a job or between jobs
      review.rs               lists waiting for an OK, confirming them, settling conflicts
      runs.rs                 starting, carrying out, recording and holding back runs
      checks.rs               background looks, folder changes, reminders
      status.rs               the row the window shows for a job
      runtime.rs              each job's live state, and Missing: removals left for later
      reach.rs                Reach: questions about folders, with a time limit
      quiet.rs                QuietDrives: drives run by other makers' drivers, kept quiet
                              while a sync writes to them
    autopilot/                looking at jobs and running them by themselves
      mod.rs                  Autopilot: the once-a-second loop
      schedule.rs             Schedule: when an Every… or Daily job is due
      watcher.rs              FolderWatcher: changes inside the folders
  tests/                      end-to-end tests with real folders (see above)
src-tauri/                    the desktop app (crate charon)
  src/
    main.rs                   start-up, plugins, the app's log, wiring
    state.rs                  AppState: what every command can reach
    commands.rs               everything the window can ask for
    events.rs                 engine events to the window, the tray and notifications
    tray.rs                   Tray: the menu-bar or system-tray icon and its menu
    notifier.rs               notifications, and Repeats (the same problem told once)
    quit.rs                   Quit: asks first when lists wait, lets syncs stop cleanly
    login.rs                  LaunchAtLogin
    desktop.rs                showing folders and opening links, per system
    about.rs                  About: what the About panel shows, and the links it may open
    updates.rs                Updates: asks GitHub whether a newer version is out
    data_folder.rs            DataFolder: the data folder, a dev build's own, the move from 0.1.0
  capabilities/default.json   what the window may do
  icons/                      app and tray icons
  tauri.conf.json             app configuration (identifier dev.stean.charon)
ui/                           the interface (plain HTML, CSS and JavaScript modules)
  index.html
  css/                        base (tokens), controls, layout, overlays, review, rows
  img/                        logo-light.svg, logo-dark.svg (the logo without background)
  js/
    main.js                   App: creates the views, connects them to Rust
    api.js                    Api: every command and event
    store.js                  JobStore: jobs, settings and what the person is looking at
    format.js                 Format: words and numbers, and the status names (STATUS_NAMES)
    dom.js                    Dom: building markup safely
    icons.js                  Icons: inline SVG icons
    logo.js                   Logo: the logo for light and dark
    platform.js               Platform: names that differ per system ("menu bar")
    theme.js                  Theme: light or dark
    toast.js                  Toast: a short message at the bottom
    updates.js                Updates: the "Update" button and GitHub's answer
    views/
      header.js               Header: status line, "Pause all", overview tiles
      job-list.js             JobList: the rows
      job-row.js              JobRow: one row and its detail, with conflicts to settle
      job-editor.js           JobEditor: the job editor, and Full automation's two warnings
      review-dialog.js        ReviewDialog: the list of changes, and settling one conflict
      waiting-sheet.js        WaitingSheet: "Waiting for your OK"
      upgrade-sheet.js        UpgradeSheet: what changed on the first start of 0.3
      settings-panel.js       SettingsPanel
      about-panel.js          AboutPanel
      sheet.js                Sheet: a panel that slides in from the right
      modal.js                Modal: a question that must be answered first
      overlay.js              Overlay: the stack of everything over the window
      pop-menu.js             PopMenu: a small menu under a button
    effects/
      river.js                River: the animated backdrop and the boat
      glow.js                 PointerGlow: cards that light up around the pointer
      counter.js              Counter: numbers that roll to their new value
    demo/
      backend.js              DemoBackend: the pretend Rust side for a plain browser
.github/workflows/            build.yml and publish.yml (see Releases)
docs/                         this guide and the README screenshot
```

Code style: small objects with one job each, and a short comment on every function.

## Where data lives

In the app's data folder:

- macOS: `~/Library/Application Support/dev.stean.charon/`
- Windows: `%APPDATA%\dev.stean.charon\`
- Linux: `~/.local/share/dev.stean.charon/`

A development build uses `dev.stean.charon-dev` next to it instead.

| File | What's in it |
|---|---|
| `charon-state.json` | settings and jobs, with each job's record: its last run, whether its next sync waits for an OK and why, the drives of the last sync (with whether one refused links, and `caseGuessed` where how it treats case was only a guess), the Mirror/Backup markers of the last sync, what automatic runs removed or replaced during the last day, and, for a last sync dated later than now, for how long Charon has known so (`datedAhead`) |
| `charon-state.json.damaged-<date>-<time>` | a state file that couldn't be read, put aside instead of overwritten (Charon then starts without jobs) |
| `charon.json.moved-to-0.3` | Charon 0.2's settings and jobs, after they moved (see below) |
| `charon.lock` | locked while a Charon works with this folder; a second one refuses to start |
| `instances.json` | the running Charon and up to 19 before it that worked with this folder, newest first, each with its id (a new one each start), its computer and its data folder: busy notes left in a job's folders by an earlier one on this computer with this folder count for nothing (see *Overlapping jobs and other Charons*) |
| `baselines/<job>.json` | two-way memory: the two folders it belongs to, the markers' tokens and counter, how each side looked at the last sync, the pairs that look alike but were found to differ and left for the person (`unsettled`), and, when one item was settled while Charon couldn't trust the memory, why it isn't trusted (`distrust`) |
| `landings/<job>.json` | Mirror and Backup: copies whose dates didn't stick on the destination, with the folders and destination marker they belong to |
| `same-contents/<job>.json` | two-way: pairs of files a full read found identical, with how both sides looked then, and the folders and markers they belong to (gone with the two-way memory, when the job's folders or mode change, and when the job is removed) |
| `unattended/<job>.json` | full automation: what automatic runs removed or replaced since the last confirmed list, within *Keep deleted files* days |
| `absences/<job>.json` | items the job's looks found missing on a network share or a drive run by a program, for how long by Charon's own clock (`countedSeconds`; `since`, by the clock on the wall, is only for reading), and whether a run left the removal that follows for later, with the folders they belong to (gone when the job's folders or mode change, or the job is removed) |
| `logs/<job>.log` | each job's activity log, cut to its newest half when it grows past 512 KB |

The state file, the list of Charons, the memories, the landings, the counts, the absences and
the files found identical are saved under a temporary name next to them
(`.<name>.<process>-<n>.tmp`) and renamed into place, so a crash never leaves one half-written.
The job logs are added to as they go.

The app's own log is `charon.log` (`charon-dev.log` for a development build), started afresh at
5 MB with a few older ones kept: in `~/Library/Logs/dev.stean.charon/` on macOS,
`%LOCALAPPDATA%\dev.stean.charon\logs\` on Windows, and `~/.local/share/dev.stean.charon/logs/` on
Linux (next to the job logs).

**From 0.1.0.** Charon 0.1.0 used the identifier `ninja.digifix.charon`. The first time an
installed 0.1.1 or later starts, it moves that folder's items into the new data folder when the
new one holds no jobs yet and the old one does. Nothing already there is replaced, and the
files with the jobs move last, so a move cut off halfway is finished at the next start. The old
folder is removed once it's empty.

**From 0.2.** Charon 0.2 kept its settings and jobs in `charon.json`. When `charon-state.json`
doesn't exist yet, 0.3 takes them over: every job comes over with full automation off and its
next sync waiting for the person's OK. Jobs that used to sync by themselves are listed once in
*What changed in Charon 0.3.0* and in their logs. The old file is renamed to
`charon.json.moved-to-0.3`, so an old copy of the app started by mistake finds no jobs to run. One
still running writes `charon.json` again after a sync; Charon notices, and nothing syncs until it
restarts (see *Overlapping jobs and other Charons*). A `charon.json` that's damaged is left where
it is, and Charon starts without jobs. Two-way memories saved by 0.2.0 and earlier don't say
which folders they belong to, so they're never used: the first sync with 0.3 compares both
folders afresh and removes nothing. Their item names only tell that first list which of the
items it copies back were on both sides at the last sync (see *Two-way*).

In each synced folder, Charon keeps:

- `.charon/<job>.json`: the job's marker, a random token and a counter. Two-way jobs keep one in
  both folders; Mirror and Backup in the destination, and in the source where Charon can write
  there. Removing a job removes its marker.
- `.charon/<uuid>.busy`: a note that a sync is at work in the folder, there while it runs (see
  *Overlapping jobs and other Charons*), also in the `.charon` folders of other jobs' folders
  inside the folders it syncs. One that a sync cut short left behind is removed by the
  next sync that finds it, once it counts for nothing. For an instant, `.charon/<uuid>.probe`
  tells whether two folders holding the same marker are one folder; a question stopped at that
  instant (its look was stopped, or a sync began to keep the drive quiet) may leave it, and it
  goes with the folder's last marker.
- `.charon-trash/<date>_<time>_<id>/`: what a run removed or replaced, with its `.charon` list.
- `.charon-<16 hex digits>.part`: a copy on its way (of a file or a link), an original Charon
  renamed aside for a moment while replacing or removing it, or, for an instant, a probe that
  asks the drive about case or links. Where the path next to an item would be too long, it's in
  a folder above. After an interrupted sync, such a file may be the only copy of a version. A run
  that goes all the way moves it into `.charon-trash` once it's an hour old (see *Leftovers*
  above), so don't delete these files by hand.
- An empty file dated 2 January 1980 00:00:00 UTC: a claim an interrupted rename left under the
  name it was taking (see *Renames* above). It's never synced, and the next run that goes all
  the way moves it into `.charon-trash` (after an hour where the drive keeps change times
  Charon trusts; see *Leftovers* above).

On Windows, Charon marks `.charon` and `.charon-trash` as hidden; elsewhere the dot hides them.

## Changing the About information

The app's website, the source code link, the developer's name, website and e-mail, the licence
name and the copyright year are constants in `src-tauri/src/about.rs`; name and version come from
the app's package information. The About panel's copyright line is built from `COPYRIGHT_YEAR`
and `DEVELOPER`, so keep those in step with the copyright line in `LICENSE` and the README. The
window may only open the app's website, the source code (releases included), the developer's
website and an e-mail to the developer (`About::may_open`), so a new link needs a constant there
too. The licence text is in `LICENSE`.

## How the update check works

`src-tauri/src/updates.rs` reads `https://api.github.com/repos/steanart/charon/releases/latest`
ten seconds after start-up and then once a day while Charon runs, each time only if *Look for
new versions* is on in Settings. *Check for updates* in the About panel and *Check for Updates…*
in the tray menu ask at any time. GitHub's latest release never includes drafts or pre-releases,
so a new version reaches people only once its release is **published**. The tag (`v0.3.0`) is
compared with the app's own version; a tag that isn't a version never counts as newer.

When a newer version is out, the title bar shows an *Update* button, the tray item becomes
*Download Charon <version>…*, and a background check shows one notification per version (with
*Notifications* on). Charon installs nothing by itself: the link is the release asset with the
fixed name for this computer (`Charon-macOS-Apple-Silicon.dmg`, `Charon-macOS-Intel.dmg`,
`Charon-Windows-Setup.exe`; on Linux the release page). Keep those names in `build.yml` and
`updates.rs` the same.
