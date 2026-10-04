//! `/var/log/dpkg.log`: `YYYY-MM-DD HH:MM:SS <what> …`, one line per step.
//!
//! dpkg writes each package step as `<action> <package> <installed version>
//! <available version>`, `<none>` for no version. It writes `upgrade` for
//! every replacement, so the versions tell an upgrade from a reinstall
//! (same version) or a downgrade (older version), compared as dpkg does.

use std::cmp::Ordering;

use crate::{split_arch, time, Action, Change, Entry, Event, Parsed};

/// `YYYY-MM-DD HH:MM:SS`.
const TIME_LENGTH: usize = 19;
const NO_VERSION: &str = "<none>";

pub(crate) fn parse(text: &str, parsed: &mut Parsed) {
    for line in crate::lines(text) {
        if line.text.trim().is_empty() {
            continue;
        }
        match entry(line.text) {
            Some((time, time_text, event)) => parsed.entries.push(Entry {
                line: line.number,
                offset: line.offset,
                time: Some(time),
                time_text: time_text.to_owned(),
                year_inferred: false,
                event,
            }),
            None => parsed
                .problems
                .push(format!("line {}: not a dpkg line", line.number)),
        }
    }
}

fn entry(line: &str) -> Option<(common::time::Ts, &str, Event)> {
    let time_text = line.get(..TIME_LENGTH)?;
    let time = time::local_date_time(time_text)?;
    let rest = line[TIME_LENGTH..].strip_prefix(' ')?;
    Some((time, time_text, event(rest)))
}

fn event(rest: &str) -> Event {
    let words: Vec<&str> = rest.split(' ').collect();
    match words.as_slice() {
        ["startup", scope, operation] => Event::Startup {
            scope: (*scope).to_owned(),
            operation: (*operation).to_owned(),
        },
        ["status", state, package, version] => {
            let (package, arch) = split_arch(package);
            Event::Status {
                state: (*state).to_owned(),
                package,
                arch,
                version: version_of(version),
            }
        }
        ["conffile", path @ .., decision] if !path.is_empty() => Event::Conffile {
            path: path.join(" "),
            decision: (*decision).to_owned(),
        },
        [action, package, installed, available] => {
            match change(action, package, installed, available) {
                Some(change) => Event::Change(change),
                None => message(rest),
            }
        }
        _ => message(rest),
    }
}

fn message(rest: &str) -> Event {
    Event::Message {
        level: None,
        text: rest.to_owned(),
    }
}

fn change(action: &str, package: &str, installed: &str, available: &str) -> Option<Change> {
    let installed = version_of(installed);
    let available = version_of(available);
    let (action, old_version, new_version) = match action {
        "install" => (Action::Install, installed, available),
        "upgrade" => (
            replacement(installed.as_deref(), available.as_deref()),
            installed,
            available,
        ),
        "remove" => (Action::Remove, installed, None),
        "purge" => (Action::Purge, installed, None),
        "disappear" => (Action::Disappear, installed, None),
        "configure" => (Action::Configure, None, installed),
        "trigproc" => (Action::Trigger, None, installed),
        _ => return None,
    };
    let (package, arch) = split_arch(package);
    Some(Change {
        action,
        package,
        arch,
        old_version,
        new_version,
        automatic: false,
    })
}

fn version_of(text: &str) -> Option<String> {
    (text != NO_VERSION).then(|| text.to_owned())
}

/// What dpkg's `upgrade` was, from the two versions.
fn replacement(old: Option<&str>, new: Option<&str>) -> Action {
    match (old, new) {
        (Some(old), Some(new)) => match compare_versions(old, new) {
            Ordering::Less => Action::Upgrade,
            Ordering::Equal => Action::Reinstall,
            Ordering::Greater => Action::Downgrade,
        },
        _ => Action::Upgrade,
    }
}

/// Debian version order (`[epoch:]upstream[-revision]`), as
/// `dpkg --compare-versions` has it.
pub(crate) fn compare_versions(a: &str, b: &str) -> Ordering {
    let (a_epoch, a_upstream, a_revision) = split_version(a);
    let (b_epoch, b_upstream, b_revision) = split_version(b);
    a_epoch
        .cmp(&b_epoch)
        .then_with(|| compare_parts(a_upstream, b_upstream))
        .then_with(|| compare_parts(a_revision, b_revision))
}

/// The epoch (0 when absent or unreadable), upstream version and revision.
fn split_version(version: &str) -> (u64, &str, &str) {
    let (epoch, rest) = match version.split_once(':') {
        Some((epoch, rest)) => (epoch.parse().unwrap_or(0), rest),
        None => (0, version),
    };
    let (upstream, revision) = rest.rsplit_once('-').unwrap_or((rest, ""));
    (epoch, upstream, revision)
}

/// dpkg's `verrevcmp`: runs of non-digits compared character by character
/// (`~` before anything, even the end; letters before other symbols), then
/// runs of digits compared as numbers.
fn compare_parts(a: &str, b: &str) -> Ordering {
    let (mut a, mut b) = (a.as_bytes(), b.as_bytes());
    while !a.is_empty() || !b.is_empty() {
        let (a_text, a_rest) = split_run(a, |c| !c.is_ascii_digit());
        let (b_text, b_rest) = split_run(b, |c| !c.is_ascii_digit());
        let order = compare_text(a_text, b_text);
        if order != Ordering::Equal {
            return order;
        }
        let (a_digits, a_rest) = split_run(a_rest, u8::is_ascii_digit);
        let (b_digits, b_rest) = split_run(b_rest, u8::is_ascii_digit);
        let order = compare_digits(a_digits, b_digits);
        if order != Ordering::Equal {
            return order;
        }
        (a, b) = (a_rest, b_rest);
    }
    Ordering::Equal
}

fn split_run(text: &[u8], belongs: impl Fn(&u8) -> bool) -> (&[u8], &[u8]) {
    let length = text.iter().take_while(|c| belongs(c)).count();
    text.split_at(length)
}

fn compare_text(a: &[u8], b: &[u8]) -> Ordering {
    let length = a.len().max(b.len());
    (0..length)
        .map(|i| weight(a.get(i)).cmp(&weight(b.get(i))))
        .find(|order| order.is_ne())
        .unwrap_or(Ordering::Equal)
}

/// A character's place in dpkg's order: `~`, then the end, then letters,
/// then everything else.
fn weight(c: Option<&u8>) -> i32 {
    match c {
        Some(b'~') => -1,
        None => 0,
        Some(c) if c.is_ascii_alphabetic() => i32::from(*c),
        Some(c) => i32::from(*c) + 256,
    }
}

fn compare_digits(a: &[u8], b: &[u8]) -> Ordering {
    let strip = |d: &[u8]| -> usize { d.iter().take_while(|&&c| c == b'0').count() };
    let (a, b) = (&a[strip(a)..], &b[strip(b)..]);
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{parse as parse_any, Context, Kind};

    fn events(text: &str) -> Vec<Event> {
        let parsed = parse_any(Kind::Dpkg, text.as_bytes(), Context::default());
        assert!(parsed.problems.is_empty(), "{:?}", parsed.problems);
        parsed.entries.into_iter().map(|e| e.event).collect()
    }

    fn only_change(text: &str) -> Change {
        match events(text).pop() {
            Some(Event::Change(change)) => change,
            other => panic!("not a change: {other:?}"),
        }
    }

    #[test]
    fn versions_compare_as_dpkg_does() {
        for (a, b, order) in [
            ("1.0", "1.0", Ordering::Equal),
            ("1.0", "1.1", Ordering::Less),
            ("1.0~rc1", "1.0", Ordering::Less),
            ("1.0", "1.0a", Ordering::Less),
            ("1.0a", "1.0+", Ordering::Less),
            ("1:0.9", "2.0", Ordering::Greater),
            ("2.10-5", "2.10-10", Ordering::Less),
            ("1.7.1-6+deb13u4", "1.7.1-6", Ordering::Greater),
            ("1.01", "1.1", Ordering::Equal),
        ] {
            assert_eq!(compare_versions(a, b), order, "{a} vs {b}");
        }
    }

    #[test]
    fn upgrades_downgrades_and_reinstalls_are_told_apart() {
        let action = |line: &str| only_change(line).action;
        assert_eq!(
            action("2026-10-04 10:00:00 upgrade openssl:amd64 3.0.1-1 3.0.2-1"),
            Action::Upgrade
        );
        assert_eq!(
            action("2026-10-04 10:00:00 upgrade openssl:amd64 3.0.2-1 3.0.1-1"),
            Action::Downgrade
        );
        assert_eq!(
            action("2026-10-04 10:00:00 upgrade hello:amd64 2.10-5 2.10-5"),
            Action::Reinstall
        );
    }

    #[test]
    fn configure_names_the_version_configured() {
        let change = only_change("2026-10-04 10:00:00 configure tree:amd64 2.2.1-1 <none>");
        assert_eq!(change.action, Action::Configure);
        assert_eq!(change.old_version, None);
        assert_eq!(change.new_version.as_deref(), Some("2.2.1-1"));
    }

    #[test]
    fn packages_without_an_architecture_are_read() {
        // dpkg before multiarch.
        let change = only_change("2011-03-01 10:00:00 install vim <none> 2:7.2.330-1");
        assert_eq!(change.package, "vim");
        assert_eq!(change.arch, None);
        assert_eq!(change.new_version.as_deref(), Some("2:7.2.330-1"));
    }

    #[test]
    fn conffile_decisions_are_read() {
        assert_eq!(
            events("2026-10-04 10:00:00 conffile /etc/ssh/sshd_config keep"),
            [Event::Conffile {
                path: "/etc/ssh/sshd_config".to_owned(),
                decision: "keep".to_owned(),
            }]
        );
    }

    #[test]
    fn unknown_steps_are_kept_as_written() {
        assert_eq!(
            events("2026-10-04 10:00:00 frobnicate everything"),
            [Event::Message {
                level: None,
                text: "frobnicate everything".to_owned(),
            }]
        );
    }

    #[test]
    fn damaged_times_are_problems() {
        let text = "2026-02-30 10:00:00 install a <none> 1\nnot a line\n";
        let parsed = parse_any(Kind::Dpkg, text.as_bytes(), Context::default());
        assert!(parsed.entries.is_empty());
        assert_eq!(parsed.problems.len(), 2);
    }
}
