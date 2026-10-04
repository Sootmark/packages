//! Linux package-manager logs: what was installed, upgraded and removed,
//! when, and, where the log says so, by whom.
//!
//! Four logs are read:
//!
//! - **dpkg** (`/var/log/dpkg.log`, Debian and Ubuntu): one line per step
//!   dpkg took, `2026-10-04 16:38:41 install jq:amd64 <none> 1.7.1-6`. The
//!   time is the host's wall-clock time, its zone unknown here.
//! - **apt** (`/var/log/apt/history.log`): one [`Transaction`] per apt run,
//!   with its command line, the account that ran it through sudo, and the
//!   packages it changed. Wall-clock times too.
//! - **dnf** (`/var/log/dnf.rpm.log`, RHEL 8 and later, Fedora before dnf5):
//!   `2026-10-04T20:09:06+0530 SUBDEBUG Installed: jq-1.6-19.el9.x86_64`,
//!   its time with an offset, so in UTC here.
//! - **yum** (`/var/log/yum.log`, RHEL and CentOS 7 and earlier):
//!   `Oct 04 14:39:17 Installed: tree-1.6.0-10.el7.x86_64`, no year and no
//!   zone. The year is inferred from the file's modification time, as for
//!   classic syslog, and marked on the entry.
//!
//! Rotated copies (`dpkg.log.1`, `yum.log-20261004`) are read the same way.
//! Compressed ones (`dpkg.log.2.gz`) are the caller's to decompress first.
//!
//! Lines that can't be read are reported in [`Parsed::problems`], never
//! fatal; nothing here panics on damaged input.
//!
//! ```
//! let log = b"2026-10-04 16:38:41 install jq:amd64 <none> 1.7.1-6\n";
//! let kind = packages::detect("/var/log/dpkg.log.1").unwrap();
//! let parsed = packages::parse(kind, log, packages::Context::default());
//! assert_eq!(parsed.entries[0].summary(), "install jq 1.7.1-6 (amd64)");
//! ```

use std::fmt::Write as _;

use common::time::Ts;

mod apt;
mod dnf_history;
mod dpkg;
mod rpm;
mod time;

/// This crate's version, for records of what parsed them.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Suffixes of compressed rotated copies, which [`detect`] leaves alone.
const COMPRESSED_SUFFIXES: [&str; 4] = [".gz", ".xz", ".bz2", ".zst"];
/// Digits in logrotate's `dateext` suffix (`-20261004`).
const DATEEXT_DIGITS: usize = 8;

/// Which log a file is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    /// `/var/log/dpkg.log`.
    Dpkg,
    /// `/var/log/apt/history.log`.
    AptHistory,
    /// `/var/log/dnf.rpm.log`.
    DnfRpm,
    /// `/var/log/yum.log`.
    Yum,
    /// dnf's history database, `/var/lib/dnf/history.sqlite`: transactions
    /// with their command line and the login uid that ran them.
    DnfHistory,
}

/// What the file's metadata says, for what its lines don't.
#[derive(Debug, Clone, Copy, Default)]
pub struct Context {
    /// When the file was last modified (UTC): the year of a yum log's last
    /// line.
    pub modified: Option<Ts>,
}

/// What happened to a package.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    /// Installed where it wasn't.
    Install,
    /// Replaced by a newer version.
    Upgrade,
    /// Replaced by an older version.
    Downgrade,
    /// Installed again at the same version.
    Reinstall,
    /// Removed, its configuration files kept (dpkg, apt), or erased (rpm).
    Remove,
    /// Removed with its configuration files.
    Purge,
    /// Configured after unpacking (dpkg).
    Configure,
    /// Its triggers processed (dpkg's `trigproc`).
    Trigger,
    /// Gone because another package replaced all its files (dpkg).
    Disappear,
    /// Replaced by a package that obsoletes it (rpm).
    Obsolete,
    /// The old version's files cleaned up after an upgrade (rpm).
    Cleanup,
}

impl Action {
    /// The action as one lowercase word.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Install => "install",
            Self::Upgrade => "upgrade",
            Self::Downgrade => "downgrade",
            Self::Reinstall => "reinstall",
            Self::Remove => "remove",
            Self::Purge => "purge",
            Self::Configure => "configure",
            Self::Trigger => "trigger",
            Self::Disappear => "disappear",
            Self::Obsolete => "obsolete",
            Self::Cleanup => "cleanup",
        }
    }

    /// Whether the package is on the way out: a removal names the version
    /// that was there.
    fn removes(self) -> bool {
        matches!(self, Self::Remove | Self::Purge | Self::Disappear)
    }
}

/// A change to one package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// What happened.
    pub action: Action,
    /// The package name, without its architecture.
    pub package: String,
    /// The architecture (`amd64`, `x86_64`, `noarch`), when written.
    pub arch: Option<String>,
    /// The version that was there: removals, and upgrades and downgrades
    /// when the log names it. rpm logs the outgoing side of an upgrade on a
    /// line of its own (`Upgraded:`), with only this version.
    pub old_version: Option<String>,
    /// The version installed, upgraded or downgraded to; for dpkg's
    /// configure and trigger steps, the version configured. rpm versions
    /// are `[epoch:]version-release`.
    pub new_version: Option<String>,
    /// Whether apt installed it only as a dependency (`, automatic`).
    pub automatic: bool,
}

impl Change {
    /// One line: `upgrade hello 2.10-5 -> 2.10-6 (amd64)`.
    #[must_use]
    pub fn summary(&self) -> String {
        let mut text = format!("{} {}", self.action.as_str(), self.package);
        if let Some(versions) = self.versions() {
            text.push(' ');
            text.push_str(&versions);
        }
        if let Some(arch) = &self.arch {
            let _ = write!(text, " ({arch})");
        }
        if self.automatic {
            text.push_str(", automatic");
        }
        text
    }

    fn versions(&self) -> Option<String> {
        match (&self.old_version, &self.new_version) {
            (Some(old), Some(new)) if old != new => Some(format!("{old} -> {new}")),
            (Some(old), None) if !self.action.removes() => Some(format!("{old} (replaced)")),
            (Some(version), _) | (None, Some(version)) => Some(version.clone()),
            (None, None) => None,
        }
    }
}

/// What one log line recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A package changed.
    Change(Change),
    /// dpkg's record of a package's state (`status installed tree:amd64
    /// 2.2.1-1`).
    Status {
        /// The state: `installed`, `half-configured`, `config-files`, …
        state: String,
        /// The package name.
        package: String,
        /// Its architecture, when written.
        arch: Option<String>,
        /// Its version, `None` for `<none>`.
        version: Option<String>,
    },
    /// dpkg starting a run (`startup archives unpack`, `startup packages
    /// configure`).
    Startup {
        /// `archives` or `packages`.
        scope: String,
        /// What it set out to do: `unpack`, `configure`, `remove`, …
        operation: String,
    },
    /// dpkg's decision on a configuration file the admin had changed.
    Conffile {
        /// The file.
        path: String,
        /// `keep` (the admin's copy) or `install` (the package's).
        decision: String,
    },
    /// Anything else, as written: dnf's `--- logging initialized ---`,
    /// scriptlet output (lines without a time joined with `\n`), an action
    /// word this crate doesn't know.
    Message {
        /// dnf's level word (`INFO`, `SUBDEBUG`), when written.
        level: Option<String>,
        /// The text.
        text: String,
    },
}

/// One line of a dpkg, dnf or yum log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Line number, from 1.
    pub line: usize,
    /// Byte offset of the line.
    pub offset: u64,
    /// When it was written: UTC for dnf; a wall-clock time in the host's
    /// unknown zone for dpkg and yum; `None` for a yum line whose year
    /// couldn't be inferred, or whose date doesn't exist in that year.
    pub time: Option<Ts>,
    /// The time as written.
    pub time_text: String,
    /// Whether the year was inferred (yum).
    pub year_inferred: bool,
    /// What it recorded.
    pub event: Event,
}

impl Entry {
    /// One line: `install jq 1.7.1-6 (amd64)`, `status installed tree
    /// 2.2.1-1 (amd64)`.
    #[must_use]
    pub fn summary(&self) -> String {
        match &self.event {
            Event::Change(change) => change.summary(),
            Event::Status {
                state,
                package,
                arch,
                version,
            } => {
                let mut text = format!("status {state} {package}");
                if let Some(version) = version {
                    let _ = write!(text, " {version}");
                }
                if let Some(arch) = arch {
                    let _ = write!(text, " ({arch})");
                }
                text
            }
            Event::Startup { scope, operation } => format!("startup {scope} {operation}"),
            Event::Conffile { path, decision } => format!("conffile {path}: {decision}"),
            Event::Message { text, .. } => text.clone(),
        }
    }
}

/// The account apt ran for (`Requested-By: analyst (1000)`): the user who
/// ran it through sudo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    /// The account name, when written (apt; dnf's history keeps the uid
    /// only).
    pub name: Option<String>,
    /// Its numeric id, when written.
    pub uid: Option<u32>,
}

/// One apt run, from `Start-Date` to `End-Date`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transaction {
    /// Line number of its first line, from 1.
    pub line: usize,
    /// Line number of its last line.
    pub last_line: usize,
    /// Byte offset of its first line.
    pub offset: u64,
    /// When it started: a wall-clock time in the host's unknown zone.
    pub start: Option<Ts>,
    /// When it ended; `None` when apt never wrote an end (it was
    /// interrupted, or the file was cut), or the time couldn't be read.
    pub end: Option<Ts>,
    /// The command line (`apt-get install -y tree`), when written;
    /// unattended upgrades write none.
    pub command: Option<String>,
    /// Who ran it through sudo, when written.
    pub requested_by: Option<User>,
    /// The packages it changed, in the order written.
    pub changes: Vec<Change>,
    /// The error apt reported, when it failed.
    pub error: Option<String>,
    /// Fields this crate doesn't read, as written.
    pub other: Vec<(String, String)>,
}

impl Transaction {
    /// One line: `apt-get install hello by analyst (1000): install hello
    /// 2.10-5 (amd64)`.
    #[must_use]
    pub fn summary(&self) -> String {
        let mut text = self.command.clone().unwrap_or_else(|| "apt".to_owned());
        if let Some(user) = &self.requested_by {
            match (&user.name, user.uid) {
                (Some(name), Some(uid)) => {
                    let _ = write!(text, " by {name} ({uid})");
                }
                (Some(name), None) => {
                    let _ = write!(text, " by {name}");
                }
                (None, Some(uid)) => {
                    let _ = write!(text, " by uid {uid}");
                }
                (None, None) => {}
            }
        }
        let changes: Vec<String> = self.changes.iter().map(Change::summary).collect();
        if !changes.is_empty() {
            text.push_str(": ");
            text.push_str(&changes.join(", "));
        }
        if let Some(error) = &self.error {
            let _ = write!(text, " [error: {error}]");
        }
        if self.end.is_none() {
            text.push_str(" [no end]");
        }
        text
    }
}

/// A file's contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    /// Which log it was read as.
    pub kind: Kind,
    /// Entries in file order (dpkg, dnf, yum).
    pub entries: Vec<Entry>,
    /// Transactions in file order (apt).
    pub transactions: Vec<Transaction>,
    /// Lines that couldn't be read, and dates that don't exist.
    pub problems: Vec<String>,
}

/// Which log a file is, from its name or path: `dpkg.log`, `history.log`
/// (in a directory named `apt` when a directory is given), `dnf.rpm.log`
/// or `yum.log`, rotated copies included (`dpkg.log.1`,
/// `yum.log-20261004`). `None` for anything else, including compressed
/// copies (`dpkg.log.2.gz`): decompress them, then ask about the name
/// without the compression suffix.
#[must_use]
pub fn detect(name: &str) -> Option<Kind> {
    let mut parts = name.rsplit(['/', '\\']);
    let file = parts.next()?;
    let directory = parts.next();
    if COMPRESSED_SUFFIXES.iter().any(|s| file.ends_with(s)) {
        return None;
    }
    match strip_rotation(file) {
        "dpkg.log" => Some(Kind::Dpkg),
        "history.log" if directory.map_or(true, |d| d == "apt") => Some(Kind::AptHistory),
        "dnf.rpm.log" => Some(Kind::DnfRpm),
        "yum.log" => Some(Kind::Yum),
        "history.sqlite" if directory.map_or(true, |d| d == "dnf") => Some(Kind::DnfHistory),
        _ => None,
    }
}

/// The name without logrotate's suffix: `.1`, or `-20261004` (`dateext`).
fn strip_rotation(file: &str) -> &str {
    if let Some((base, number)) = file.rsplit_once('.') {
        if !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()) {
            return base;
        }
    }
    if let Some((base, date)) = file.rsplit_once('-') {
        if date.len() == DATEEXT_DIGITS && date.bytes().all(|b| b.is_ascii_digit()) {
            return base;
        }
    }
    file
}

/// Read a log of the given kind.
#[must_use]
pub fn parse(kind: Kind, data: &[u8], context: Context) -> Parsed {
    let mut parsed = Parsed {
        kind,
        entries: Vec::new(),
        transactions: Vec::new(),
        problems: Vec::new(),
    };
    if common::gzip::is_gzip(data) {
        parsed
            .problems
            .push("gzip-compressed: decompress it first".to_owned());
        return parsed;
    }
    let text = String::from_utf8_lossy(data);
    match kind {
        Kind::Dpkg => dpkg::parse(&text, &mut parsed),
        Kind::AptHistory => apt::parse(&text, &mut parsed),
        Kind::DnfRpm => rpm::parse_dnf(&text, &mut parsed),
        Kind::Yum => rpm::parse_yum(&text, context, &mut parsed),
        Kind::DnfHistory => dnf_history::parse(data, &[], &mut parsed),
    }
    parsed
}

/// Read a file of `kind` with the SQLite write-ahead log beside it (`log`,
/// its `-wal` file): dnf's history database, whose latest transactions are
/// often only there. The log means nothing to the text logs: they read as
/// [`parse`] reads them.
#[must_use]
pub fn parse_with_log(kind: Kind, data: &[u8], log: &[u8], context: Context) -> Parsed {
    if kind != Kind::DnfHistory {
        return parse(kind, data, context);
    }
    let mut parsed = Parsed {
        kind,
        entries: Vec::new(),
        transactions: Vec::new(),
        problems: Vec::new(),
    };
    dnf_history::parse(data, log, &mut parsed);
    parsed
}

/// One line of a file, its line ending removed.
struct Line<'a> {
    number: usize,
    offset: u64,
    text: &'a str,
}

/// The file's lines, numbered from 1, with their byte offsets.
fn lines(text: &str) -> impl Iterator<Item = Line<'_>> {
    let mut offset = 0u64;
    text.split('\n').enumerate().map(move |(index, raw)| {
        let line = Line {
            number: index + 1,
            offset,
            text: raw.strip_suffix('\r').unwrap_or(raw),
        };
        offset += raw.len() as u64 + 1;
        line
    })
}

/// `name:arch` (dpkg, apt): the name and the architecture, when written.
fn split_arch(package: &str) -> (String, Option<String>) {
    match package.split_once(':') {
        Some((name, arch)) => (name.to_owned(), Some(arch.to_owned())),
        None => (package.to_owned(), None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_names_and_rotations() {
        for (name, kind) in [
            ("dpkg.log", Some(Kind::Dpkg)),
            ("/var/log/dpkg.log.1", Some(Kind::Dpkg)),
            ("/var/log/apt/history.log", Some(Kind::AptHistory)),
            ("history.log.3", Some(Kind::AptHistory)),
            ("/home/analyst/history.log", None),
            ("dnf.rpm.log", Some(Kind::DnfRpm)),
            (
                "C:\\case\\var\\log\\dnf.rpm.log-20261004",
                Some(Kind::DnfRpm),
            ),
            ("yum.log-20261004", Some(Kind::Yum)),
            ("dpkg.log.2.gz", None),
            ("dnf.log", None),
            ("", None),
        ] {
            assert_eq!(detect(name), kind, "{name}");
        }
    }

    #[test]
    fn gzip_is_reported_not_parsed() {
        let parsed = parse(Kind::Dpkg, &[0x1f, 0x8b, 8, 0, 0], Context::default());
        assert!(parsed.entries.is_empty());
        assert_eq!(parsed.problems.len(), 1);
    }

    #[test]
    fn summaries_name_both_versions_only_when_they_differ() {
        let mut change = Change {
            action: Action::Upgrade,
            package: "hello".to_owned(),
            arch: Some("amd64".to_owned()),
            old_version: Some("2.10-5".to_owned()),
            new_version: Some("2.10-6".to_owned()),
            automatic: false,
        };
        assert_eq!(change.summary(), "upgrade hello 2.10-5 -> 2.10-6 (amd64)");
        change.new_version = None;
        assert_eq!(change.summary(), "upgrade hello 2.10-5 (replaced) (amd64)");
        change.action = Action::Remove;
        assert_eq!(change.summary(), "remove hello 2.10-5 (amd64)");
    }
}
