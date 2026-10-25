<p align="center">
  <img src="ui/img/logo-light.svg#gh-light-mode-only" width="200" alt="Charon: a boat with a lantern, carrying a folder">
  <img src="ui/img/logo-dark.svg#gh-dark-mode-only" width="200" alt="Charon: a boat with a lantern, carrying a folder">
</p>

<h1 align="center">Charon</h1>

<p align="center">
  <strong>The ferryman for your files.</strong><br>
  Sync jobs between folders and drives: mirror, backup or two-way. Charon shows you every
  change it is about to make, and by default carries nothing across until you say OK.
</p>

<p align="center">
  <a href="../../releases/latest">Download</a> ·
  <a href="https://charonsync.app">Website</a> ·
  <a href="docs/DEVELOPMENT.md">Build from source</a>
</p>

<p align="center">
  <img alt="macOS" src="https://img.shields.io/badge/macOS-11%2B-111?logo=apple">
  <img alt="Windows" src="https://img.shields.io/badge/Windows-10%2F11-111?logo=windows">
  <img alt="Linux" src="https://img.shields.io/badge/Linux-deb%20·%20rpm%20·%20AppImage-111?logo=linux&logoColor=white">
  <img alt="Licence: MIT" src="https://img.shields.io/badge/licence-MIT-111">
</p>

<img src="docs/screenshot.png" alt="Charon showing sync jobs: one in sync, one syncing with a progress bar, one with changes waiting and one with conflicts">

---

## What it does

A **sync job** connects two folders: a source and a destination. Each job is one row in
Charon's window, with its status, a progress bar while it runs, when it last synced and what's
waiting, a *Review and sync* button (*Sync now* once a job may sync by itself, *Stop* while it
runs), and buttons to open either folder or pause the job.

**Three ways to cross**

| Mode | What happens |
|---|---|
| **Mirror** | The destination becomes a copy of the source, apart from what's excluded, unreadable or listed as not synced. Files removed from the source are removed there too. |
| **Backup** | New and changed files are copied. A file in the destination that differs from the source's, even one edited there, is replaced by the source's version. Files removed from the source stay in the destination. |
| **Two-way** | Changes flow both ways: a file you delete on one side is removed on the other too. Charon remembers the last synced state, so it can tell "deleted here" from "new there". The first two-way sync of two folders removes nothing. |

**Every sync waits for your OK**

By default Charon never changes your files by itself. When a job is due, it looks at both
folders and counts what would change. If anything would, the row says *Needs your OK* (or
*Files to compare*, when all it found is files it couldn't compare yet), a
notification tells you (if *Notifications* is on in Settings), and nothing crosses until you
press *Review and sync*, which makes the list afresh, and confirm it. A new job shows its first
list as soon as you save it.

**Four ways to start**

Each job chooses when it's due. Its row says when Charon looks next (*looks at 14:00 and asks
you*), and its *Details* tab says what happens each time (*Looks every hour and asks you*).
Charon also looks when it notices changes, and every 15 minutes by default (*Look for changes*
in Settings). Looking never changes your files.

- **Manual:** only when you press *Review and sync* (or *Sync now*, once a job may sync by itself).
- **On change:** a few seconds after edits in the folders settle down (you choose how long). If
  they never settle down (an app saves into them every few seconds), the job still syncs, or is
  looked at, about once per *Look for changes* interval.
- **Every… / Daily:** every *N* minutes, or every day at a set time.
- **Drive connected:** plug in the backup drive, and Charon looks at once and shows what waits,
  so you can confirm it before you eject the drive.

**Full automation, only if you want it**

A job can sync by itself instead of asking: turn on **Full automation** in its settings. Two
warnings come first (*Full automation can lose data*, then a *Last warning* with a tick): a
wrong folder or a damaged file is carried over before you see it, so it is only for people who
know exactly what it does.

The job then syncs by itself only after one more list you confirm. It asks again after a
*Stop* (pausing the job stops a sync of it the same way, and so does pausing everything, unless
you confirmed that sync's list), after you change what it syncs, after a sync Charon couldn't
record, and whenever the guard below holds a run back.
Changing the mode, turning safe deletes off or swapping a Mirror or Backup job's folders turns
full automation off again, so you read the warnings anew.

Such jobs carry a red *Full auto* tag, and once they may sync by themselves their row says when
(*next sync 14:00*).

## Status at a glance

Every row uses a colour *and* an icon, so it still reads for colour-blind eyes.

| Status | Colour and icon | Meaning |
|---|---|---|
| In sync | green, a tick | The last look found nothing to carry, apart from what the row's flags say couldn't be read or isn't synced. |
| Syncing | blue, turning arrows | Files are crossing right now. |
| Needs your OK | pink, a checklist | Changes wait for you to look at the list and confirm it, or a full-automation run was held back for your OK. |
| Files to compare | pink, a checklist | A look in the background found nothing else to carry, but couldn't compare some files by their contents yet (it reads at most 1 GB), so their copies may differ. *Review and sync* compares them, and its list may well have nothing to confirm. |
| Ready | cyan, a spark | A new job that hasn't synced yet. Nothing crosses until you confirm its first list. |
| Looking | no colour yet, a magnifying glass | Charon hasn't finished a new look at this job yet, so it doesn't claim the folders match. |
| Pending | amber, an up arrow | Changes are queued: a list you confirmed waits for another sync of the same folders to end, or, with full automation, changes were found and cross at the job's next run, or a look found files it couldn't compare yet, which that run reads (the tray then says *1 job with files to compare at its next sync*), or removals were left for a later sync. |
| Paused | grey, two bars | Doesn't sync by itself. Charon still looks and shows what waits, and you can still sync it from its row. |
| Conflict | violet, a fork | A file changed on both sides and waits for you to choose which version to keep. |
| Error | red, an exclamation mark | The last look or sync failed, or some files couldn't be copied. The row says why and names what failed. |
| Offline | faded grey, a crossed-out plug | A folder or its drive isn't reachable or doesn't answer (a network share whose server is off), or the folder is now on another drive than at the last sync. |

## Built to be trusted

- **A list before every sync.** What changes on each side (new, replaced, removed), whole
  folders that go, conflicts, old trash to be emptied, and *Notes* on every place Charon
  couldn't read or won't sync, with the reason. *Confirm* (never the Enter key) names what it
  removes or replaces, *Confirm: copy 23, replace 2, remove 3*, and carries out exactly that list.
- **Warnings that need a tick.** A list that removes or replaces much of one side, replaces
  files with empty ones, follows a folder that suddenly looks empty or finds a folder on another
  drive needs a tick before *Confirm*. So does one that removes files (in a Mirror or Backup
  job, also one that replaces them) with safe deletes off. If one side is empty and the sync
  would remove files on the other, Charon refuses.
- **An out-of-date list can't be confirmed.** A list is good for 15 minutes, and only until
  Charon sees the folders, the drives or the job's settings change. During the sync each step
  checks the disk again: a file edited after the list was made is skipped, never overwritten.
  In a two-way job, a file you move or delete after Charon has copied it, while the sync is
  still running, isn't counted as synced, so the next sync copies it back instead of deleting
  the copy.
- **Nothing unreadable becomes a removal.** A place Charon can't read (no permission, a folder
  macOS protects, a cloud folder that isn't downloaded) is left alone on both sides with
  everything inside it, and listed in the job's *Couldn't read* tab. So is a file the system
  won't let Charon read when it comes to copy it (no permission, or another app holds it open or
  locked): that isn't an error, and every look checks again whether it can be read, even while
  it keeps changing. Until it can, or until you leave it out from that tab, removals on the side
  it should go to wait, with a note, because one of them could take the last copy of what it
  holds. After a restart, Charon simply tries it again.
- **Excluded items are never deleted.** What matches the job's *Leave out* patterns is never
  copied or removed, and a folder that still holds such an item stays. Where either drive
  ignores upper and lower case, so do the patterns, in every alphabet (`Entwürfe` also leaves
  out `ENTWÜRFE`), so an item is never left out on one side and synced on the other. Only system
  clutter like `.DS_Store` goes along with a removed folder, into `.charon-trash`. A new job
  starts with a few patterns: system files like `.DS_Store` and `Thumbs.db`, Office lock files
  (`~$*`) and `*.tmp`. Remove any you want synced.
- **A guard for full automation.** A run nobody looks at is held for your OK when it would
  remove or replace too much of one side, replace files with empty ones or follow a folder that
  suddenly looks empty, or when a drive, a marker or the room on a drive isn't right. Too much
  means half of a side's files removed (3 or more), half of them removed or replaced (5 or
  more), more than a quarter of them (21 or more), or over 1,000.
- **Slow damage counts too.** The guard also adds up what automatic runs removed or replaced
  within the last day (held at the same limits for removing or replacing) and since you last
  confirmed a list (held at half of a side's files, 5 or more, or more than a quarter of them,
  21 or more, counting back as far as Settings keeps deleted files). Runs nobody looked at never
  empty `.charon-trash`, so with safe deletes on, the originals are still there when the guard
  steps in.
- **Safe deletes.** Removed and replaced files go to a hidden `.charon-trash` folder at the top
  of the synced folder (so on the same drive), one dated folder per sync, for 30 days by default
  (0 in Settings keeps them until you empty it). Two-way jobs, *Newest wins* and settling a
  conflict keep replaced versions there even with safe deletes off, and so does a removal that
  clears the way for an item of another kind (a folder that became a file). If `.charon-trash`
  can't take a file (it's a link, or Charon may not write there), the file stays exactly where it
  was and that step fails. Only a sync you confirmed, whose list said so, empties old trash, and
  only what is still older than *Keep deleted files* when that sync runs. It never empties a
  file that changed after it went in (an app may still be saving into it); the `.DS_Store` that
  Finder leaves when you look inside, or the hours a FAT or exFAT drive shifts at a
  daylight-saving change, don't count as such a change. Nor does the placeholder iCloud Drive,
  Nextcloud or ownCloud leaves when it frees up a file (or iCloud Drive a whole package) that
  Charon put there, as long as the placeholder says it was as Charon left it.
- **No half-written files.** Each file is copied under a temporary name next to its target (or,
  where that path would be too long for the drive, in the nearest folder above it where it fits),
  flushed to the drive, and only then takes its real name, with its original modification time.
  A replaced file is swapped out or moved aside, and checked: if an app saved a new version at
  that moment, that version stays. A source that changes while it's copied waits for the next
  sync.
- **The right folders on the right drives.** Charon leaves a small marker (a `.charon` folder)
  in the folders it syncs and remembers which drive each was on. A folder now on another drive
  (the empty folder an unplugged drive leaves behind) shows as *Offline*. A missing or older
  marker (another disk, a copy restored from a backup) makes a two-way job compare afresh and
  remove nothing. In a Mirror or Backup job, a missing or foreign marker on either side, or a
  source older than at the last sync, needs a tick before *Confirm* and holds full automation
  back. If a drive goes away mid-sync, the sync stops.
- **No job undoes its own work, or another's unnoticed.** A job whose two folders are one
  folder, or one inside the other, would copy its own copies into itself, so Charon refuses it.
  It checks when you save the job, and again at every look by the folders themselves and by its
  markers, since a link, a drive or share reachable under two names, or a folder made later can
  hide what the paths don't show; a list that finds it says why and can't be confirmed. A Mirror
  job removes whatever isn't in its source, so its destination must not overlap a folder another
  job writes into: the other job's files would be removed, and a two-way job would carry those
  removals back. Charon won't save two such jobs (so *Duplicate* never copies a Mirror job).
  Where it finds them anyway (through another name, or saved by Charon 0.2, which didn't check),
  the list warns about it, needs a tick before *Confirm* and holds full automation back; an edit
  that keeps the job's folders and mode, such as a new name, still saves. A copy of a hidden
  `.charon` folder (in files restored from a backup, say) does no harm: the list only mentions
  it, so you can delete it. Where Charon can't tell whether it is a copy (when it can't write to
  either `.charon` folder, say), the list says so. Until you delete the copy, the job then doesn't
  sync if it holds the job's own marker, and needs the tick if it holds that of a job it could
  undo.
- **One sync at a time in any folder.** Two syncs of Charon 0.3 never work in the same folders
  at once, or in one folder inside the other: not two jobs of this Charon, nor Charon on another
  computer that syncs the same share, nor a second copy of Charon 0.3 on this one. Jobs that
  share a folder without undoing each other's work (two backups of one folder, say) are fine,
  and simply take turns. Before a sync changes anything, it leaves a note in the hidden `.charon`
  folder at the top of both its folders and of the synced folders inside them, then looks for
  other syncs' notes in its folders, in the folders above them and in the synced folders inside
  them. So a sync of a folder inside, even one that reaches it through a share starting lower
  down, sees the note at its own top. Another Charon knows of a sync
  only by its note, and a folder Charon can't write to (a card, a read-only share) can't hold
  one. Charon 0.2 leaves no notes at all, so quit it first (see *Upgrading from 0.2*). If another
  sync is at work there, a sync that starts by itself, or a list you have already confirmed,
  waits for it (see *When a sync waits for another one* below); *Review and sync* and *Sync now*
  say instead which sync is at work in these folders, and to look again when it's done. A note
  that hasn't changed for two minutes, as this computer sees it, belongs to a Charon that
  stopped (it crashed, or its computer was switched off): it no longer counts and is cleared
  away. The notes Charon itself left before a crash count for nothing once it starts again.
- **A sync counts only once the drives have it.** At the end of every sync Charon makes sure its
  changes reached the drives, then looks once more at what it wrote. A drive that dropped out
  for a moment can come back without a sync's last changes: if a copy is gone, or a replaced or
  removed file is back, Charon records nothing of that sync and the job waits for your OK (a
  two-way job's next look compares both folders afresh and removes nothing). Edits you make in
  the meantime don't count as lost changes, but moving or deleting something this sync has just
  copied or created does, since Charon can't tell that from a drive that lost it.
- **Room to spare, in a safe order.** Charon keeps 2% of the receiving drive free (at most 2 GB),
  and the list warns when a sync won't fit. The old versions a sync keeps in `.charon-trash`
  count as their drive frees room: one at a time on most local drives, all together on drives
  that may keep snapshots, on network shares and on drives Charon doesn't know, and not at all
  on APFS, where they're clones. New folders and copies go first and removals last. Where an
  item of another kind takes an item's place, the item there goes as late as it can: after
  every other copy, right before what takes its place. When a copy to one side fails, that
  side's removals wait for the next sync. When a copy is skipped because its file changed
  meanwhile, or waits with the rest of its app library, only the removals that could be its old
  copy wait (the same name, or the same size and date), so one busy file doesn't stop every
  removal. The activity log says which copy each of them waits for, and why.
- **App libraries and databases stay whole.** A Photos or Final Cut library, a Logic project or
  a Pages document that changed on both sides of a two-way job becomes one conflict, kept whole
  from the side you choose. A sync carries each library, and each SQLite database with its
  journal files, as one: all of it is copied first, checked again right before it takes its
  place, and put in place only if nothing of it changed meanwhile. What it no longer has goes
  last, once its new versions have landed, so a file moved from one library into another keeps
  its old copy until the new one is there. A new file it gains (a new photo) still arrives while
  the rest of a busy library waits for the next sync, and a library the other drive doesn't
  have yet is copied file by file, each database in it with its journal files. A library
  holding a file Charon leaves alone (one it can't read, or one only in the cloud) is left alone
  whole, with a note; only an item the other drive can never hold, such as a link on a drive
  without links, is left out of it on its own.
- **When a file changed on both sides.** Two-way jobs choose *Keep both*, *Newest wins* or
  *Ask me*, and an edit always beats a deletion. *Newest wins* decides only when the times can
  be trusted (not on FAT, exFAT or network drives, not whole hours apart, not a time that went
  backwards); otherwise both versions are kept. When the dates can't tell (no record of the
  last sync, both sides changed, or a date that only moved by a rounding), two-way jobs compare
  files of the same size byte by byte: identical ones aren't copied, and look-alikes that differ
  inside become a conflict. A look in the background reads at most 1 GB this way. Past that, it
  counts two files as in sync only where a read found them identical before (Charon remembers
  that, even after a restart, while both files and the folders they're in stay as they were),
  so a large file whose details alone changed isn't left to compare at every launch. It never
  counts any other file it left unread as in sync: the row says how many files it couldn't
  compare yet, and the list and the sync read them all. Mirror and Backup go by size and
  modification time only.
- **Crashes and quitting.** Quitting lets a sync stop at a safe point (a file being copied stays
  as it was) and records what it carried. If Charon or the computer stops mid-sync, a two-way
  job's next look compares both folders afresh and removes nothing. After your *Stop*, the next
  sync waits for your OK.

## Good to know

- **Full automation trades the list for speed.** The guard catches big and slowly spreading
  damage, but a smaller mistake (a few files overwritten by a broken app) is carried over before
  you see it. With safe deletes on, the originals stay in `.charon-trash`; with safe deletes
  off, removed files (and in Mirror and Backup, replaced ones) are gone at once.
- **A two-way job compares afresh** the first time, after you change its folders or its mode,
  and whenever it can't trust its memory of the last sync. It then removes nothing, so files
  deleted on one side since may come back, and files that differ are kept twice or left for you.
  With *Newest wins*, the newer of two versions still replaces the older one, which goes to
  `.charon-trash`. When it compares afresh because it can't trust its memory, the list says why,
  counts the items only on one side that it copies to the other, and names those that Charon's
  record had on both sides at the last sync (most likely deleted since). Files that look alike
  but that this record shows changed since are read, and stay a conflict if they differ.
  Settling one conflict meanwhile doesn't end this: the job compares afresh until you confirm a
  whole list.
- **FAT and exFAT drives** keep times in local time and less precisely; Charon allows for that,
  and *Newest wins* keeps both versions there.
- **NTFS and ext4 drives on a Mac** need a driver of another maker (Paragon, Tuxera, or one
  through macFUSE), which runs inside the core of macOS: a mistake there restarts the Mac.
  Paragon NTFS for Mac 17 once did during a sync onto a USB stick, while Spotlight read the new
  files. So Charon works on one job at a time on such a drive (other jobs there wait for it),
  and while a sync writes to it, the rest of Charon keeps off it as far as it can: its folders
  there aren't watched for changes until the sync is done, so what you change there meanwhile is
  found by the next regular look. Keep the driver up to date, and consider keeping the drive out
  of Spotlight (*System Settings > Spotlight > Search Privacy*), or use exFAT, which macOS runs
  itself.
- **What a drive can't hold** is left out, with a note in the job's *Not synced* tab: on
  Windows and on FAT, exFAT and NTFS drives and SMB shares, names with characters like `:` `?`
  `*`, a trailing dot or space, or names like `CON`; on any drive, names or paths too long for
  it; on FAT32, files over 4 GB; and links on a drive that holds none. Two names that differ
  only in how Unicode writes them, or only in case where a drive ignores case, are both left
  alone. What a drive refused during a sync stays out until the item or the drive changes, or
  Charon starts again; a name the drive found taken by another item (a short name like
  `REPORT~1.PDF`) is tried again once that item is gone.
- **Long paths.** Where a conflict copy's full name would make the path too long, it gets a
  shorter one; if none fits, both versions stay as they are, with a note, until you rename or
  move one of them. An item whose path in `.charon-trash` would be too long goes in under its
  own name at the top of that sync's dated folder.
- **Never synced:** sockets, pipes and devices, names that aren't valid Unicode, the folders a
  drive keeps for itself (like `lost+found`) at the top of a synced folder, the journal a
  Nextcloud or ownCloud client keeps in the folder it syncs (`.sync_….db` and the files that go
  with it, at any depth, never removed either), and Charon's own `.charon`, `.charon-trash` and
  half-copied files (with the empty files an interrupted sync can leave to hold a name; a later
  sync moves them into `.charon-trash`, half-copied files after an hour, the empty ones after an
  hour too, or at once on a drive that keeps no change time).
  Where a drive ignores case, a folder of your own named like Charon's in other case
  (`.Charon-Trash`, `.CHARON`) is left out too, and so, on any drive, is an item of yours named
  exactly `.charon` or `.charon-trash` that holds anything but Charon's own files (system files
  like `.DS_Store` inside don't count), with a note. Whatever of these Charon finds on one side,
  it leaves an item of the same name alone on the other side as well, so a Mirror never removes
  it from the destination. The list's *Notes* name most of them.
- **Network shares and drives run by a program** (SMB, NFS, AFP, sshfs, rclone and other FUSE
  drives) may show changes late. There, full automation removes what follows from an item that
  went missing only once looks have found it missing for ten minutes. Until then the job's row
  shows *Pending* and its last run says how many removals were left for a later sync; once the
  ten minutes are up, the job syncs again by itself, whatever starts it otherwise. Charon
  remembers how long looks have found an item missing, even across a restart, and counts the
  ten minutes by its own clock only: the time Charon isn't running doesn't count, and setting
  the computer's clock forward or back, while Charon runs or between two launches, never
  shortens them. After a restart, after the computer wakes up, or once the share comes back,
  looks must also find the item missing for two more minutes. When such a removal belongs to a
  database, the database's other changes wait with it. A list you confirm shows every removal
  and holds none back. A USB disk that Linux reads through a program (ntfs-3g, exfat-fuse) and
  cloud folders on the computer's own drive (iCloud Drive, Dropbox, OneDrive) don't count as
  such drives. On Windows, a
  folder reached through a link or a mapped drive counts as being on the share it leads to.
- **Files only in the cloud** (iCloud Drive, OneDrive, Dropbox on macOS and Windows) are left
  alone when Charon looks, and it says so. So are files that iCloud Drive before macOS 14, or
  Nextcloud and ownCloud, replaced with a placeholder of their own (`.Report.pdf.icloud`,
  `Report.pdf.nextcloud`); the placeholder itself is never synced or removed. A file named like
  a placeholder counts as one only where its service keeps files: in iCloud Drive's folders on
  this Mac, and in a folder the Nextcloud or ownCloud client on this computer syncs, as its own
  settings say, or one that holds a client's journal (a client on another computer may sync
  it). Elsewhere, on a backup drive say, it's an ordinary file. A backup copied from a synced
  folder may hold a copy of that journal too: the note then names the journal, and if no client
  syncs that folder, the journal and the placeholder are old copies you can remove. Charon never
  downloads a file: one that goes back to the cloud after the list was made is skipped when its
  turn comes, and the job's activity log says why.
- **What travels with a file:** contents, modification time, and where the drive takes them,
  access time and permissions (creation time on macOS and Windows). On a Mac also Finder tags,
  Finder info and resource forks; on Linux, `user.` extended attributes. Owners, ACLs, other
  extended attributes and Windows alternate data streams are not copied, and a change to only
  tags or permissions doesn't make a file sync. In a two-way job it doesn't stand against an
  edit on the other side either: the edit crosses. Only against a deletion on the other side
  does such a file count as edited, and it's copied back. Inside an app library or database
  every change counts, so a library changed on both sides still becomes one conflict. FAT and
  exFAT keep no tags (a resource fork still travels), and the run's summary says so. On a Mac,
  `._` files are never synced on their own. Folders are created fresh: their own dates,
  permissions and tags aren't copied.
- **Close the app** that uses a library or database before you confirm a list that changes it
  (the list reminds you). A file another app keeps Charon from reading waits under *Couldn't
  read* until it can be read or you leave it out there, and a notification tells you once.
- **Links** are copied as links on macOS and Linux, never followed; on Windows, links and
  junctions are left alone, and so is an item of the same name on the other side, so a Mirror
  never removes it. When a link takes the place of a file or folder, Mirror and Backup keep the
  file or folder on the other side, with a note, and a two-way job asks you. A drive mounted
  inside a synced folder isn't crossed into.
- **A drive that stops answering** (a hard-mounted NFS share whose server is gone, a stuck sshfs
  or rclone drive) doesn't freeze Charon. After 10 seconds without an answer, its job shows
  *Offline* and says the folder doesn't answer, and the window and the other jobs carry on. A
  sync of that job that was already at work on the drive can still hang until Charon is
  restarted, and meanwhile other syncs wait for it, since Charon can't tell which folders it
  shares with them. Quitting waits for such a sync at most 30 seconds.
- **Encrypted containers** (and other files that keep their size and date when they change)
  are noticed only by a two-way job, and only where the drive keeps a change time Charon
  trusts: an APFS, Mac OS Extended, ext, Btrfs, XFS or ZFS drive that isn't a network drive or
  run by a program. That leaves out Windows, and FAT, exFAT and NTFS drives. Mirror and Backup
  go by size and date, so they don't copy such a change at all. Where there's no change time,
  the last look at the end of a sync also can't notice a drive that dropped out mid-sync and
  came back without a container's new version.
- **When a sync waits for another one.** A sync that would start by itself while another sync
  is at work in its folders waits, and its row says for which: *Waits until “Photos” is done,
  then starts by itself*, naming another job, *another Charon* or *Charon on* the other
  computer. So does a list you made before that sync began and confirm while it works: Charon
  tells you *Confirmed. “Docs” syncs as soon as “Photos” is done.*, and the row says
  *Confirmed: starts as soon as “Photos” is done*. While Charon runs, the list stays
  good however long it waits, and each step still checks the disk when its turn comes. *Stop*,
  pausing the job or everything, or editing the job calls it off, and so does quitting Charon,
  which doesn't ask first: that list is then never carried out, and the job's activity log says
  the sync was called off. If a drive is swapped meanwhile, the list is dropped and the row says why.
  The waiting sync starts the moment a sync of this Charon ends, and within seconds of another
  Charon's. Once it's done, Charon looks at the folders again at once, since the sync it waited
  for changed them.
- **If Charon can't save its jobs and settings** (a full disk, say), nothing syncs, since Charon
  couldn't record what a sync did: *Sync now* says so instead of starting, and a list you confirm
  changes nothing (the row says why). The header and the tray say *Charon needs your attention*,
  with the reason under the title bar. Charon tries a save again every minute. Once one works, jobs
  sync by themselves again from their next trigger: a scheduled sync that came due meanwhile waits
  for its next time. Editing or removing a job, or leaving a place out, changes nothing when
  Charon can't save it, and the window says so.

## More

- Lives in the **menu bar** or system tray, with the overall status, *Sync All…* (it starts the
  jobs with full automation that may sync by themselves and shows the others' lists; jobs
  paused on their own, jobs whose folders aren't there or don't answer, and jobs whose confirmed
  list already waits for another sync are left out), *Review Waiting Jobs…* and *Pause
  Everything* (it also stops at once every sync you didn't confirm a list for, and a *Sync All…*
  that is still starting jobs starts no more of them; a sync you confirmed goes on).
- **Notifications** when changes wait for your OK or files wait to be compared (a reminder after
  two hours, then once a day), when a full-automation run is held back, and for errors, files
  that couldn't be read or that a drive couldn't hold, conflicts and big syncs. A problem that
  comes back at every sync is told once, until a sync goes well again.
- **Launch at login** (off by default). With *Keep running when the window is closed* on (the
  default), Charon keeps looking for changes from the menu bar or tray after you close the window.
- **Tells you when a new version is out**: it asks GitHub at start-up and once a day, and an
  *Update* button takes you to the installer for your computer (on Linux, to the release page,
  where you choose a package). Nothing is downloaded without you.
- A **per-job activity log**: what each run did, and which files failed or were left out and
  why (up to 50 of each per run).
- **macOS, Windows and Linux.**

## Install

Download the latest version for your system from the [**Releases**](../../releases/latest) page:

| System | File |
|---|---|
| macOS (Apple Silicon) | `Charon-macOS-Apple-Silicon.dmg` |
| macOS (Intel) | `Charon-macOS-Intel.dmg` |
| Windows | `Charon-Windows-Setup.exe` or `Charon-Windows.msi` |
| Linux | `Charon-Linux.AppImage` (any distribution), `.deb` (Debian, Ubuntu) or `.rpm` (Fedora, openSUSE) |

> **First start:** the macOS version is signed and notarised by Apple. The Windows installers
> aren't signed yet: choose **More info → Run anyway** if SmartScreen asks.
> On Linux, the tray icon needs AppIndicator support (on GNOME: the "AppIndicator" extension).
> Without a tray, Charon still works, and closing its window quits it.

## Upgrading from 0.2

Charon 0.3 takes over your jobs and settings the first time it starts. What you will notice:

- **Every job now waits for your OK.** A sheet, *What changed in Charon 0.3.0*, lists the jobs
  that used to sync by themselves: keep each one asking, or turn on full automation. A job with
  full automation on syncs by itself only once you confirm its next list, which the sheet offers
  with *Review and sync*.
- **A two-way job's first sync compares both folders afresh**, because 0.2's memory of the last
  sync can't be fully trusted. It removes nothing, so files deleted since then may come back;
  with *Newest wins*, the newer of two versions still replaces the older one. The list counts
  the items it copies to the other side and names those of them 0.2 had synced, which were most
  likely deleted since. 0.2's record is used for that and nothing else.
- **Symbolic links are now copied as links** on macOS and Linux, so a first list may include
  links 0.2 left out.
- **Files ending in `.charon-partial`** (0.2's half-copied files) go to `.charon-trash` only from
  a list that names them. If one of them is yours, rename it before you confirm.
- **The old settings file becomes `charon.json.moved-to-0.3`**, so a copy of 0.2 started by
  mistake finds no jobs to run.
- **Quit Charon 0.2 before you start 0.3.** A 0.2 that is still running keeps syncing its jobs
  by itself, without 0.3's checks, until you quit it. Charon 0.3 notices it only once the older
  one saves its jobs again, after one of its syncs. From then on 0.3 syncs nothing, not even a
  list you confirm, until you reopen Charon: the header and the tray say *Charon needs your
  attention*, and the line under the title bar says what to do. Quit the older Charon from its
  icon in the menu bar or system tray, then quit and reopen Charon.

## Privacy

Charon works on your computer. No accounts, no tracking, no analytics. The only time it goes
online is to ask GitHub whether a new version is out, which you can turn off (*Look for new
versions* in Settings). Your jobs and settings, what each job remembers of its last sync and
the activity logs stay in the app's data folder on your computer. In your synced folders,
Charon adds its `.charon` marker and its `.charon-trash`, and while it works, short-lived
temporary files named `.charon-….part` and a note in `.charon` with the job's name, the
computer's name, the time and random ids, so that other syncs of those folders wait. A note
left by a sync that was cut short stays until a later sync there clears it.

## Build from source

Charon is written in Rust with [Tauri](https://tauri.app); the interface is plain HTML, CSS and
JavaScript, with no Node.js needed.

```sh
cargo install tauri-cli --version "^2" --locked
cargo tauri dev       # run it
cargo tauri build     # make installers for your system
```

See [**docs/DEVELOPMENT.md**](docs/DEVELOPMENT.md) for requirements per system, the project
layout and how the sync engine decides what to do.

## Contributing

Bug reports, ideas and pull requests are welcome. See [CONTRIBUTING.md](CONTRIBUTING.md), and
please open an issue first for bigger changes.

## Licence

[MIT](LICENSE) © 2026 [stean.ART](https://stean.art)
