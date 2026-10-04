//! `/var/log/apt/history.log`: one block of `Key: value` lines per apt run,
//! blocks separated by blank lines:
//!
//! ```text
//! Start-Date: 2026-10-04  16:38:43
//! Commandline: apt-get install -y hello
//! Requested-By: analyst (1000)
//! Install: hello:amd64 (2.10-5), libfoo:amd64 (1.0-1, automatic)
//! End-Date: 2026-10-04  16:38:43
//! ```
//!
//! Package lists hold `package:arch (version)`, or `(old, new)` for
//! upgrades and downgrades, with `, automatic` for packages pulled in as
//! dependencies.

use crate::{split_arch, time, Action, Change, Line, Parsed, Transaction, User};

const AUTOMATIC: &str = "automatic";

pub(crate) fn parse(text: &str, parsed: &mut Parsed) {
    let mut open: Option<Transaction> = None;
    for line in crate::lines(text) {
        if line.text.trim().is_empty() {
            close(&mut open, parsed);
            continue;
        }
        let Some((key, value)) = line.text.split_once(':') else {
            parsed
                .problems
                .push(format!("line {}: not a `Key: value` line", line.number));
            continue;
        };
        let value = value.trim();
        if key == "Start-Date" {
            close(&mut open, parsed);
            open = Some(start(&line, value, &mut parsed.problems));
            continue;
        }
        let Some(transaction) = open.as_mut() else {
            parsed
                .problems
                .push(format!("line {}: outside a transaction", line.number));
            continue;
        };
        transaction.last_line = line.number;
        field(transaction, key, value, line.number, &mut parsed.problems);
    }
    close(&mut open, parsed);
}

fn close(open: &mut Option<Transaction>, parsed: &mut Parsed) {
    if let Some(transaction) = open.take() {
        parsed.transactions.push(transaction);
    }
}

fn start(line: &Line, value: &str, problems: &mut Vec<String>) -> Transaction {
    let start = time::local_date_time(value);
    if start.is_none() {
        problems.push(format!("line {}: unreadable start date", line.number));
    }
    Transaction {
        line: line.number,
        last_line: line.number,
        offset: line.offset,
        start,
        end: None,
        command: None,
        requested_by: None,
        changes: Vec::new(),
        error: None,
        other: Vec::new(),
    }
}

fn field(
    transaction: &mut Transaction,
    key: &str,
    value: &str,
    number: usize,
    problems: &mut Vec<String>,
) {
    let report = |problems: &mut Vec<String>, what: &str| {
        problems.push(format!("line {number}: unreadable {what}"));
    };
    match key {
        "End-Date" => {
            transaction.end = time::local_date_time(value);
            if transaction.end.is_none() {
                report(problems, "end date");
            }
        }
        "Commandline" => transaction.command = Some(value.to_owned()),
        "Requested-By" => transaction.requested_by = Some(user(value)),
        "Error" => transaction.error = Some(value.to_owned()),
        _ => match list_action(key) {
            Some(action) => match changes(action, value) {
                Some(changes) => transaction.changes.extend(changes),
                None => report(problems, "package list"),
            },
            None => transaction.other.push((key.to_owned(), value.to_owned())),
        },
    }
}

fn list_action(key: &str) -> Option<Action> {
    Some(match key {
        "Install" => Action::Install,
        "Upgrade" => Action::Upgrade,
        "Downgrade" => Action::Downgrade,
        "Reinstall" => Action::Reinstall,
        "Remove" => Action::Remove,
        "Purge" => Action::Purge,
        _ => return None,
    })
}

/// `analyst (1000)`.
fn user(value: &str) -> User {
    let parsed = value
        .strip_suffix(')')
        .and_then(|v| v.rsplit_once(" ("))
        .and_then(|(name, uid)| Some((name, uid.parse().ok()?)));
    match parsed {
        Some((name, uid)) => User {
            name: Some(name.to_owned()),
            uid: Some(uid),
        },
        None => User {
            name: Some(value.to_owned()),
            uid: None,
        },
    }
}

/// `a:amd64 (1.0), b:amd64 (1.0, 1.1), c:amd64 (2.0, automatic)`; `None`
/// if any item can't be read.
fn changes(action: Action, list: &str) -> Option<Vec<Change>> {
    let mut changes = Vec::new();
    let mut rest = list.trim();
    while !rest.is_empty() {
        let (package, after) = rest.split_once(" (")?;
        let (inside, after) = after.split_once(')')?;
        changes.push(change(action, package.trim(), inside)?);
        rest = after.trim_start_matches([',', ' ']);
    }
    Some(changes)
}

/// One item: the package and what's in its brackets.
fn change(action: Action, package: &str, inside: &str) -> Option<Change> {
    let mut versions: Vec<&str> = inside.split(", ").map(str::trim).collect();
    let automatic = versions.last() == Some(&AUTOMATIC);
    if automatic {
        versions.pop();
    }
    let (old_version, new_version) = match (action, versions.as_slice()) {
        (Action::Upgrade | Action::Downgrade, [old, new]) => (Some(*old), Some(*new)),
        (Action::Install, [new]) => (None, Some(*new)),
        (Action::Reinstall, [version]) => (Some(*version), Some(*version)),
        (Action::Remove | Action::Purge, [old]) => (Some(*old), None),
        _ => return None,
    };
    if package.is_empty() || versions.iter().any(|v| v.is_empty()) {
        return None;
    }
    let (package, arch) = split_arch(package);
    Some(Change {
        action,
        package,
        arch,
        old_version: old_version.map(str::to_owned),
        new_version: new_version.map(str::to_owned),
        automatic,
    })
}

#[cfg(test)]
mod tests {
    use crate::{parse, Action, Context, Kind, Parsed};

    fn read(text: &str) -> Parsed {
        parse(Kind::AptHistory, text.as_bytes(), Context::default())
    }

    const UPGRADE: &str = "\
Start-Date: 2026-09-30  09:12:01
Commandline: /usr/bin/unattended-upgrade
Upgrade: openssl:amd64 (3.0.13-1, 3.0.15-1), libssl3t64:amd64 (3.0.13-1, 3.0.15-1)
Downgrade: curl:amd64 (8.10.1-1, 7.88.1-10, automatic)
Error: Sub-process /usr/bin/dpkg returned an error code (1)
End-Date: 2026-09-30  09:12:09
";

    #[test]
    fn upgrades_downgrades_and_errors_are_read() {
        let parsed = read(UPGRADE);
        assert!(parsed.problems.is_empty(), "{:?}", parsed.problems);
        let transaction = &parsed.transactions[0];
        assert_eq!(transaction.changes.len(), 3);
        let curl = &transaction.changes[2];
        assert_eq!(curl.action, Action::Downgrade);
        assert_eq!(curl.old_version.as_deref(), Some("8.10.1-1"));
        assert_eq!(curl.new_version.as_deref(), Some("7.88.1-10"));
        assert!(curl.automatic);
        assert_eq!(
            transaction.error.as_deref(),
            Some("Sub-process /usr/bin/dpkg returned an error code (1)")
        );
        assert_eq!(
            transaction.summary(),
            "/usr/bin/unattended-upgrade: upgrade openssl 3.0.13-1 -> 3.0.15-1 (amd64), \
             upgrade libssl3t64 3.0.13-1 -> 3.0.15-1 (amd64), \
             downgrade curl 8.10.1-1 -> 7.88.1-10 (amd64), automatic \
             [error: Sub-process /usr/bin/dpkg returned an error code (1)]"
        );
    }

    #[test]
    fn a_transaction_cut_short_has_no_end() {
        let parsed = read("Start-Date: 2026-09-30  09:12:01\nInstall: a:amd64 (1)\n");
        assert_eq!(parsed.transactions[0].end, None);
        assert!(parsed.transactions[0].summary().ends_with("[no end]"));
    }

    #[test]
    fn a_start_without_a_blank_line_starts_a_new_transaction() {
        let parsed = read(
            "Start-Date: 2026-09-30  09:12:01\nStart-Date: 2026-09-30  09:13:01\n\
             End-Date: 2026-09-30  09:13:02\n",
        );
        assert_eq!(parsed.transactions.len(), 2);
        assert_eq!(parsed.transactions[1].last_line, 3);
    }

    #[test]
    fn damage_is_reported() {
        let parsed = read(
            "Install: a:amd64 (1)\n\nStart-Date: yesterday\nInstall: a:amd64 (1\n\
             garbage\nAptdaemon: x\n",
        );
        assert_eq!(parsed.problems.len(), 4, "{:?}", parsed.problems);
        assert_eq!(
            parsed.transactions[0].other,
            [("Aptdaemon".to_owned(), "x".to_owned())]
        );
    }

    #[test]
    fn requested_by_without_a_uid_keeps_the_name() {
        let parsed = read("Start-Date: 2026-09-30  09:12:01\nRequested-By: analyst\n");
        let user = parsed.transactions[0].requested_by.as_ref().unwrap();
        assert_eq!((user.name.as_deref(), user.uid), (Some("analyst"), None));
    }
}
