# packages

Linux package-manager logs: what was installed, upgraded, downgraded and removed, when, and, where the log says so, by whom. dpkg and apt (Debian, Ubuntu), dnf and yum (RHEL, Rocky, Alma, CentOS, Fedora before dnf5). One dependency, its sibling `sootmark-common` (times).

```toml
[dependencies]
sootmark-packages = "0.2"
```

```rust
let path = "/var/log/apt/history.log";
let kind = packages::detect(path).expect("a package log");
let context = packages::Context { modified: Some(file_modified) };
let parsed = packages::parse(kind, &std::fs::read(path)?, context);
for transaction in &parsed.transactions {
    println!("{:?} {}", transaction.start, transaction.summary());
}
for entry in &parsed.entries {
    println!("{:?} {}", entry.time, entry.summary());
}
```

## What you get

- `detect(name)`: which log a file is, from its name or path, rotated copies included (`dpkg.log.1`, `yum.log-20261004`). `history.log` counts only in a directory named `apt` when a directory is given. Compressed copies (`dpkg.log.2.gz`) are yours to decompress first; `parse` reports gzip data as a problem rather than reading it.
- `parse(kind, bytes, context)`: `entries` (dpkg, dnf, yum) or `transactions` (apt), and `problems`. Each change to a package is a `Change { action, package, arch, old_version, new_version, automatic }`; every entry and transaction has a one-line `summary()`.
  - **dpkg** (`/var/log/dpkg.log`): `install`, `upgrade`, `remove`, `purge`, `configure`, `trigproc`, `disappear`; `status <state>`; `startup archives|packages <op>`; `conffile <path> keep|install`. dpkg writes `upgrade` for every replacement, so the two versions are compared as `dpkg --compare-versions` does to tell an upgrade from a reinstall (same version) or a downgrade. Times are the host's wall clock, its zone unknown, never passed off as UTC.
  - **apt** (`/var/log/apt/history.log`): one `Transaction` per run: start and end (wall clock; no end when apt was interrupted), command line, `Requested-By` (the account that ran it through sudo, and its uid), the Install / Upgrade / Downgrade / Reinstall / Remove / Purge lists with `automatic` marked, and the error apt reported. Fields this crate doesn't read are kept as written.
  - **dnf** (`/var/log/dnf.rpm.log`): `Installed`, `Upgrade`/`Upgraded`, `Downgrade`/`Downgraded`, `Reinstall`/`Reinstalled`, `Obsolete`/`Obsoleted`, `Erase`, `Cleanup`, with packages split as rpm names them (`name-[epoch:]version-release.arch`). dnf writes each side of a replacement on its own line: the incoming package fills `new_version`, the outgoing one `old_version`. Times carry an offset (`+0530`, `+00:00`, `Z`), so they are UTC here. Other lines (`--- logging initialized ---`, scriptlet warnings) are kept as messages, with their level.
  - **yum** (`/var/log/yum.log`, EL7 and earlier): `Installed`, `Updated`, `Erased`, the epoch written before the name. No year and no zone: the year is inferred as for classic syslog (lines are in order, so it goes up when the month goes back, and the last line is in the year the file was last modified, `Context::modified`) and marked on the entry; without a modification time, times are left unset and the text kept.
  - **dnf's history database** (`/var/lib/dnf/history.sqlite`), read with `sootmark-sqlite`: one transaction per dnf run, with its command line, the login uid that ran it (the person behind `sudo dnf`; unknown when dnf recorded `-1`), begin and end (UTC), and each package with libdnf's action and whether it came in as a dependency. `parse_with_log` reads it with its `-wal` file, where the latest transactions often are.
- Damage never panics: an unreadable line, a date that doesn't exist, or a package name that can't be split is reported in `problems`.

Not yet: dnf5's logs (Fedora 41 and later), zypper and pacman, `/var/log/apt/term.log`, and dnf's history database.

## How it's checked

- Logs and dnf's history database written by real package managers (`tests/fixtures/`), made by `tests/fixtures/gen.sh` in throwaway containers: debian:trixie (apt installs, one through sudo by a non-root account, a reinstall, a removal, a purge and an autoremove), rockylinux:9 (dnf installs, a reinstall, erasures, in a +05:30 zone) and centos:7 (yum, from vault.centos.org). The tests check the packages, versions and actions the script asked for, the account apt ran for, and that every line of every file is an entry, part of a transaction, or a problem.
- Unit tests for what the containers didn't do: upgrades and downgrades in each format, epochs, conffile decisions, apt errors, cut-short transactions, dpkg version order, yum's year rolling over, leap days.
- Property tests: arbitrary bytes read as each kind, and the real logs damaged or cut anywhere, give entries or problems, never a panic.

## Licence

MIT or Apache-2.0, at your option. The test logs were written for this crate.
