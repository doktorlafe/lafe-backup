# lafe-backup

A small backup tool written in Rust. It packs the selected folders and files
into a single timestamped `.tar.gz` archive in a destination folder (typically
on a mounted network drive) and prunes old archives as it goes.

- configured through an interactive terminal UI or through commands
- the archive preserves permissions, owners and symlinks (symlinks are not followed)
- if the destination lives on a mounted drive that is not currently mounted, the
  backup fails and writes nothing to the local disk
- the archive is written as `.partial` and renamed only after it completes successfully

The interface and its messages are in Czech.

## Installation

Requires [Rust](https://rustup.rs) and a C linker (`gcc`).

```sh
sh install.sh
```

The script builds a release binary (in `~/.cache/lafe-backup-target`, so it also
works from a network drive without execute permission) and installs it to
`~/.local/bin/lafe-backup`. Updates are installed the same way.

## Usage

Running `lafe-backup` on its own opens the interactive settings:

| Key | Action |
| --- | --- |
| `↑` `↓` / `j` `k` | move |
| `Enter` | edit the row, open the folder browser |
| `Space` | in the browser, add/remove a source or pick the destination |
| `←` `→` | in the browser, go up a level / into a folder |
| `.` | show hidden files |
| `e` | type a path by hand |
| `d` | remove a source or an excluded name |
| `r` | run a backup |
| `q` | quit |

The same can be done with commands:

```
lafe-backup add <path>...        add folders/files to back up
lafe-backup remove <path>...     stop backing them up
lafe-backup dest <folder>        set where archives are stored
lafe-backup exclude <name>...    skip files/folders with this name
lafe-backup unexclude <name>...  stop skipping them
lafe-backup keep <n>             how many recent archives to keep (0 = never delete)
lafe-backup name <name>          archive prefix -> <name>-YYYYMMDD-HHMMSS.tar.gz
lafe-backup schedule <when>      how often to back up (see below)
lafe-backup show | list          print the current settings
lafe-backup run [--dry-run]      run a backup (--dry-run only lists what would be backed up)
```

Settings are stored in `~/.config/lafe-backup/config` (set the
`LAFE_BACKUP_CONFIG` variable to use a different path).

## Automatic backups

The frequency is set on the “Četnost” (frequency) row of the interactive UI –
`Enter` on it opens a picker (hourly, every 2–12 hours, daily, weekly, off),
where `←` `→` change the hour and `Tab` the day of the week – or with the
`schedule` command, which generates and enables a systemd user timer
(`~/.config/systemd/user/lafe-backup.{service,timer}`):

```sh
lafe-backup schedule daily 03:00   # every day at 03:00
lafe-backup schedule 6h            # every 6 hours
lafe-backup schedule hourly        # every hour
lafe-backup schedule Mon 02:30     # every Monday at 02:30
lafe-backup schedule off           # turn automatic backups off
sudo loginctl enable-linger "$USER"   # once: so the timer runs even when you are not logged in
```

Any other systemd `OnCalendar` expression works too (`man systemd.time`). With
more frequent backups, remember to raise `keep` – it counts archives, not days.

## Restoring

The archive holds absolute paths without the leading slash, so it unpacks with
plain `tar`:

```sh
tar -xzf lafe-arm-20261010-225854.tar.gz -C /target/folder
```
