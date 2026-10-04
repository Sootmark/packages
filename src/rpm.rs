//! The RHEL family's logs, both naming packages as rpm does
//! (`name-[epoch:]version-release.arch`):
//!
//! - `/var/log/dnf.rpm.log`: `2026-10-04T20:09:06+0530 SUBDEBUG Installed:
//!   jq-1.6-19.el9.x86_64`. dnf writes both sides of a replacement on lines
//!   of their own: `Upgrade:` names the incoming package, `Upgraded:` the
//!   outgoing one (likewise `Downgrade`, `Reinstall`, `Obsolete`).
//! - `/var/log/yum.log`: `Oct 04 14:39:17 Installed: tree-1.6.0-10.el7.x86_64`
//!   (`Updated:` names the incoming package, `Erased:` the removed one). yum
//!   writes a non-zero epoch before the name: `1:bc-1.06-1.el7.x86_64`.

use crate::time::{self, Undated};
use crate::{Action, Change, Context, Entry, Event, Parsed};

/// The architectures rpm names, so that the last `.` part of a package is
/// only taken for one when it is one.
const ARCHES: [&str; 18] = [
    "x86_64", "noarch", "i386", "i486", "i586", "i686", "aarch64", "armv7hl", "armv7hnl", "armhfp",
    "ppc64le", "ppc64", "ppc", "s390x", "s390", "riscv64", "src", "nosrc",
];
/// rpm's architecture for packages without one (`gpg-pubkey`).
const NO_ARCH: &str = "(none)";
/// `YYYY-` at the start of a dnf line.
const YEAR_PREFIX: usize = 5;

/// Which side of a change a line names.
#[derive(Clone, Copy)]
enum Side {
    /// The package going in.
    Incoming,
    /// The package going out.
    Outgoing,
}

pub(crate) fn parse_dnf(text: &str, parsed: &mut Parsed) {
    for line in crate::lines(text) {
        if line.text.trim().is_empty() {
            continue;
        }
        let (time_text, rest) = line.text.split_once(' ').unwrap_or((line.text, ""));
        let Some(time) = time::offset_date_time(time_text) else {
            if !looks_dated(line.text) && continue_message(parsed, line.text) {
                continue;
            }
            parsed
                .problems
                .push(format!("line {}: not a dnf line", line.number));
            continue;
        };
        let (level, message) = rest.split_once(' ').unwrap_or((rest, ""));
        match event(Some(level), message) {
            Ok(event) => parsed.entries.push(Entry {
                line: line.number,
                offset: line.offset,
                time: Some(time),
                time_text: time_text.to_owned(),
                year_inferred: false,
                event,
            }),
            Err(problem) => parsed
                .problems
                .push(format!("line {}: {problem}", line.number)),
        }
    }
}

/// `YYYY-…`: a damaged time, not a scriptlet's output.
fn looks_dated(line: &str) -> bool {
    let bytes = line.as_bytes();
    bytes.len() >= YEAR_PREFIX
        && bytes[..YEAR_PREFIX - 1].iter().all(u8::is_ascii_digit)
        && bytes[YEAR_PREFIX - 1] == b'-'
}

/// A line without a time continues the previous message (multi-line
/// scriptlet output), if the previous entry is one.
fn continue_message(parsed: &mut Parsed, line: &str) -> bool {
    let Some(Entry {
        event: Event::Message { text, .. },
        ..
    }) = parsed.entries.last_mut()
    else {
        return false;
    };
    text.push('\n');
    text.push_str(line);
    true
}

pub(crate) fn parse_yum(text: &str, context: Context, parsed: &mut Parsed) {
    let mut undated = Vec::new();
    for line in crate::lines(text) {
        if line.text.trim().is_empty() {
            continue;
        }
        let read = Undated::read(line.text)
            .ok_or_else(|| "not a yum line".to_owned())
            .and_then(|(clock, time_text, rest)| {
                Ok((clock, time_text, event(None, rest.trim_start())?))
            });
        match read {
            Ok((clock, time_text, event)) => {
                undated.push(clock);
                parsed.entries.push(Entry {
                    line: line.number,
                    offset: line.offset,
                    time: None,
                    time_text: time_text.to_owned(),
                    year_inferred: true,
                    event,
                });
            }
            Err(problem) => parsed
                .problems
                .push(format!("line {}: {problem}", line.number)),
        }
    }
    time::infer_years(&mut parsed.entries, &undated, context, &mut parsed.problems);
}

/// `Installed: name-version-release.arch`, or any other message. An action
/// naming a package that can't be read is a problem.
fn event(level: Option<&str>, message: &str) -> Result<Event, String> {
    let known = message
        .split_once(": ")
        .and_then(|(word, package)| Some((action(word)?, package)));
    let Some(((action, side), package)) = known else {
        return Ok(Event::Message {
            level: level.map(str::to_owned),
            text: message.to_owned(),
        });
    };
    let (package, arch, version) =
        nevra(package.trim()).ok_or_else(|| format!("unreadable package `{package}`"))?;
    let (old_version, new_version) = match side {
        Side::Incoming => (None, Some(version)),
        Side::Outgoing => (Some(version), None),
    };
    Ok(Event::Change(Change {
        action,
        package,
        arch,
        old_version,
        new_version,
        automatic: false,
    }))
}

/// The action a dnf or yum word names, and which side of it the package
/// is.
fn action(word: &str) -> Option<(Action, Side)> {
    use Side::{Incoming, Outgoing};
    Some(match word {
        "Installed" | "Install" => (Action::Install, Incoming),
        "Upgrade" | "Updated" => (Action::Upgrade, Incoming),
        "Upgraded" => (Action::Upgrade, Outgoing),
        "Downgrade" => (Action::Downgrade, Incoming),
        "Downgraded" => (Action::Downgrade, Outgoing),
        "Reinstall" => (Action::Reinstall, Incoming),
        "Reinstalled" => (Action::Reinstall, Outgoing),
        "Obsolete" => (Action::Obsolete, Incoming),
        "Obsoleted" => (Action::Obsolete, Outgoing),
        "Erase" | "Erased" => (Action::Remove, Outgoing),
        "Cleanup" => (Action::Cleanup, Outgoing),
        _ => return None,
    })
}

/// `name-[epoch:]version-release[.arch]` (dnf) or
/// `[epoch:]name-version-release[.arch]` (yum): the name, the architecture
/// and `[epoch:]version-release`.
fn nevra(text: &str) -> Option<(String, Option<String>, String)> {
    let (rest, arch) = match text.rsplit_once('.') {
        Some((rest, arch)) if ARCHES.contains(&arch) || arch == NO_ARCH => {
            (rest, (arch != NO_ARCH).then(|| arch.to_owned()))
        }
        _ => (text, None),
    };
    let (rest, release) = rest.rsplit_once('-')?;
    let (name, version) = rest.rsplit_once('-')?;
    let (epoch, name, version) = match (name.split_once(':'), version.split_once(':')) {
        (Some((epoch, name)), None) => (Some(epoch), name, version),
        (None, Some((epoch, version))) => (Some(epoch), name, version),
        (None, None) => (None, name, version),
        (Some(_), Some(_)) => return None,
    };
    let valid_epoch = epoch.map_or(true, |e| {
        !e.is_empty() && e.bytes().all(|b| b.is_ascii_digit())
    });
    if name.is_empty() || version.is_empty() || release.is_empty() || !valid_epoch {
        return None;
    }
    let evr = match epoch {
        Some(epoch) => format!("{epoch}:{version}-{release}"),
        None => format!("{version}-{release}"),
    };
    Some((name.to_owned(), arch, evr))
}

#[cfg(test)]
mod tests {
    use super::nevra;
    use crate::{parse, Action, Context, Event, Kind};
    use common::time::Ts;

    #[test]
    fn package_names_are_split_as_rpm_writes_them() {
        assert_eq!(
            nevra("jq-1.6-19.el9_8.2.x86_64"),
            Some(("jq".into(), Some("x86_64".into()), "1.6-19.el9_8.2".into()))
        );
        // dnf writes the epoch before the version, yum before the name.
        assert_eq!(
            nevra("perl-Errno-0:1.30-481.el9.x86_64").map(|p| p.2),
            Some("0:1.30-481.el9".into())
        );
        assert_eq!(
            nevra("1:bc-1.06.95-13.el7.x86_64").map(|p| (p.0, p.2)),
            Some(("bc".into(), "1:1.06.95-13.el7".into()))
        );
        assert_eq!(
            nevra("gpg-pubkey-350d275d-6279464b"),
            Some(("gpg-pubkey".into(), None, "350d275d-6279464b".into()))
        );
        assert_eq!(nevra("nodashes.x86_64"), None);
        assert_eq!(nevra("x:a-y:1-2"), None);
    }

    #[test]
    fn both_sides_of_a_dnf_upgrade_are_read() {
        let text = "\
2026-10-04T10:00:00+0000 SUBDEBUG Upgrade: openssl-1:3.2.2-6.el9.x86_64
2026-10-04T10:00:01+0000 SUBDEBUG Upgraded: openssl-1:3.0.7-27.el9.x86_64
2026-10-04T10:00:02+0000 INFO warning: /etc/ssh/sshd_config created as /etc/ssh/sshd_config.rpmnew
";
        let parsed = parse(Kind::DnfRpm, text.as_bytes(), Context::default());
        assert!(parsed.problems.is_empty(), "{:?}", parsed.problems);
        let summaries: Vec<String> = parsed.entries.iter().map(crate::Entry::summary).collect();
        assert_eq!(
            summaries,
            [
                "upgrade openssl 1:3.2.2-6.el9 (x86_64)",
                "upgrade openssl 1:3.0.7-27.el9 (replaced) (x86_64)",
                "warning: /etc/ssh/sshd_config created as /etc/ssh/sshd_config.rpmnew",
            ]
        );
    }

    #[test]
    fn scriptlet_output_continues_its_message_but_damaged_times_are_problems() {
        let text = "\
2026-10-04T10:00:00+0000 INFO scriptlet said
and more
2026-13-04T10:00:00+0000 SUBDEBUG Installed: a-1-1.noarch
2026-10-04T10:00:00+0000 SUBDEBUG Installed: garbage
";
        let parsed = parse(Kind::DnfRpm, text.as_bytes(), Context::default());
        assert_eq!(parsed.entries.len(), 1);
        assert_eq!(parsed.entries[0].summary(), "scriptlet said\nand more");
        assert_eq!(parsed.problems.len(), 2, "{:?}", parsed.problems);
    }

    /// Modified at noon UTC on 2027-01-15.
    fn january_2027() -> Context {
        Context {
            modified: Some(Ts::from_unix_seconds(1_800_014_400)),
        }
    }

    #[test]
    fn yum_years_roll_over_and_are_marked() {
        let text = "\
Dec 31 23:59:00 Updated: 1:bc-1.06.95-14.el7.x86_64
Jan 01 00:01:00 Erased: tree-1.6.0-10.el7.x86_64
";
        let parsed = parse(Kind::Yum, text.as_bytes(), january_2027());
        assert!(parsed.problems.is_empty(), "{:?}", parsed.problems);
        let times: Vec<String> = parsed
            .entries
            .iter()
            .map(|e| e.time.unwrap().to_iso8601().unwrap())
            .collect();
        assert_eq!(
            times,
            ["2026-12-31T23:59:00.0000000", "2027-01-01T00:01:00.0000000"]
        );
        assert!(parsed.entries.iter().all(|e| e.year_inferred));
        let Event::Change(change) = &parsed.entries[0].event else {
            panic!("not a change");
        };
        assert_eq!(change.action, Action::Upgrade);
        assert_eq!(change.new_version.as_deref(), Some("1:1.06.95-14.el7"));
    }

    #[test]
    fn yum_without_a_modification_time_keeps_times_unset() {
        let parsed = parse(
            Kind::Yum,
            b"Oct 04 14:39:17 Installed: tree-1.6.0-10.el7.x86_64\n",
            Context::default(),
        );
        assert_eq!(parsed.entries[0].time, None);
        assert_eq!(parsed.entries[0].time_text, "Oct 04 14:39:17");
    }

    #[test]
    fn a_leap_day_in_a_year_without_one_is_a_problem() {
        let parsed = parse(
            Kind::Yum,
            b"Feb 29 10:00:00 Installed: a-1-1.noarch\n",
            january_2027(),
        );
        assert_eq!(parsed.entries[0].time, None);
        assert_eq!(parsed.problems.len(), 1);
    }
}
