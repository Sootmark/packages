//! Logs written by real dpkg, apt, dnf and yum runs in throwaway containers
//! (`tests/fixtures/gen.sh`), read back: the packages, versions and actions
//! the script asked for, the account it ran apt as, and every line of every
//! file accounted for as an entry, a transaction's field or a problem.

use std::fs;
use std::path::Path;

use common::time::{Semantic, Ts};
use packages::{detect, parse, Action, Change, Context, Event, Kind, Parsed};

fn read(name: &str) -> Vec<u8> {
    fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name),
    )
    .unwrap()
}

/// Modified at noon UTC on 2026-10-04, the day the fixtures were made.
fn october_2026() -> Context {
    Context {
        modified: Some(Ts::from_unix_seconds(1_791_115_200)),
    }
}

fn parse_fixture(name: &str) -> Parsed {
    let kind = detect(name).unwrap_or_else(|| panic!("{name} not detected"));
    parse(kind, &read(name), october_2026())
}

fn non_blank_lines(name: &str) -> usize {
    String::from_utf8_lossy(&read(name))
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count()
}

fn changes(parsed: &Parsed) -> Vec<&Change> {
    parsed
        .entries
        .iter()
        .filter_map(|e| match &e.event {
            Event::Change(change) => Some(change),
            _ => None,
        })
        .collect()
}

/// `action package old new` for each change, for compact expectations.
fn brief(changes: &[&Change]) -> Vec<String> {
    changes
        .iter()
        .map(|c| {
            format!(
                "{} {} {} {}",
                c.action.as_str(),
                c.package,
                c.old_version.as_deref().unwrap_or("-"),
                c.new_version.as_deref().unwrap_or("-"),
            )
        })
        .collect()
}

fn iso(time: Option<Ts>) -> String {
    time.and_then(|t| t.to_iso8601()).unwrap_or_default()
}

#[test]
fn every_line_is_an_entry_or_a_problem() {
    for name in ["debian/dpkg.log", "rocky9/dnf.rpm.log", "centos7/yum.log"] {
        let parsed = parse_fixture(name);
        assert!(parsed.problems.is_empty(), "{name}: {:?}", parsed.problems);
        assert_eq!(parsed.entries.len(), non_blank_lines(name), "{name}");
    }
    let parsed = parse_fixture("debian/apt/history.log");
    assert!(parsed.problems.is_empty(), "{:?}", parsed.problems);
    let covered: usize = parsed
        .transactions
        .iter()
        .map(|t| t.last_line - t.line + 1)
        .sum();
    assert_eq!(covered, non_blank_lines("debian/apt/history.log"));
}

#[test]
fn dpkg_records_what_was_installed_and_removed() {
    let parsed = parse_fixture("debian/dpkg.log");
    let steps: Vec<&Change> = changes(&parsed)
        .into_iter()
        .filter(|c| !matches!(c.action, Action::Configure | Action::Trigger))
        .collect();
    assert_eq!(
        brief(&steps),
        [
            "install libonig5 - 6.9.9-1+b1",
            "install libjq1 - 1.7.1-6+deb13u4",
            "install jq - 1.7.1-6+deb13u4",
            "install tree - 2.2.1-1",
            "install hello - 2.10-5",
            // `apt-get install --reinstall`: dpkg says `upgrade`.
            "reinstall hello 2.10-5 2.10-5",
            "remove tree 2.2.1-1 -",
            "remove jq 1.7.1-6+deb13u4 -",
            "remove libjq1 1.7.1-6+deb13u4 -",
            "remove libonig5 6.9.9-1+b1 -",
        ]
    );
    assert!(steps.iter().all(|c| c.arch.as_deref() == Some("amd64")));
    let first = &parsed.entries[0];
    assert_eq!(first.summary(), "startup archives unpack");
    // dpkg's own wall clock (Europe/Paris in the container), no zone.
    assert_eq!(first.time.unwrap().semantic(), Semantic::LocalUnknownZone);
    assert_eq!(iso(first.time), "2026-10-04T16:38:41.0000000");
    let states = parsed
        .entries
        .iter()
        .filter(|e| matches!(&e.event, Event::Status { state, package, .. } if state == "not-installed" && package == "tree"))
        .count();
    assert_eq!(states, 1);
}

#[test]
fn apt_records_each_run_and_who_asked_for_it() {
    let parsed = parse_fixture("debian/apt/history.log");
    let commands: Vec<&str> = parsed
        .transactions
        .iter()
        .map(|t| t.command.as_deref().unwrap_or(""))
        .collect();
    assert_eq!(
        commands,
        [
            "apt-get install -y -qq tree jq",
            "apt-get install -y -qq hello",
            "apt-get install -y -qq --reinstall hello",
            "apt-get remove -y -qq tree",
            "apt-get purge -y -qq jq",
            "apt-get autoremove -y -qq --purge",
        ]
    );
    let first = &parsed.transactions[0];
    let automatic: Vec<&str> = first
        .changes
        .iter()
        .filter(|c| c.automatic)
        .map(|c| c.package.as_str())
        .collect();
    assert_eq!(automatic, ["libonig5", "libjq1"]);
    assert_eq!(iso(first.start), "2026-10-04T16:38:41.0000000");
    assert_eq!(iso(first.end), "2026-10-04T16:38:42.0000000");

    let by_analyst = &parsed.transactions[1];
    let user = by_analyst.requested_by.as_ref().unwrap();
    assert_eq!(
        (user.name.as_deref(), user.uid),
        (Some("analyst"), Some(1000))
    );
    assert_eq!(
        by_analyst.summary(),
        "apt-get install -y -qq hello by analyst (1000): install hello 2.10-5 (amd64)"
    );
    assert!(parsed
        .transactions
        .iter()
        .enumerate()
        .all(|(i, t)| (i == 1) == t.requested_by.is_some()));

    let all: Vec<&Change> = parsed.transactions[2..]
        .iter()
        .flat_map(|t| &t.changes)
        .collect();
    assert_eq!(
        brief(&all),
        [
            "reinstall hello 2.10-5 2.10-5",
            "remove tree 2.2.1-1 -",
            "purge jq 1.7.1-6+deb13u4 -",
            "purge libonig5 6.9.9-1+b1 -",
            "purge libjq1 1.7.1-6+deb13u4 -",
        ]
    );
}

#[test]
fn dnf_records_installs_reinstalls_and_erasures_in_utc() {
    let parsed = parse_fixture("rocky9/dnf.rpm.log");
    assert_eq!(
        brief(&changes(&parsed)),
        [
            "install oniguruma - 6.9.6-1.el9.6",
            "install jq - 1.6-19.el9_8.2",
            "install tree - 1.8.0-10.el9",
            "reinstall tree - 1.8.0-10.el9",
            "reinstall tree 1.8.0-10.el9 -",
            "remove tree 1.8.0-10.el9 -",
            "remove jq 1.6-19.el9_8.2 -",
            "remove oniguruma 6.9.6-1.el9.6 -",
        ]
    );
    let first = &parsed.entries[0];
    assert_eq!(
        first.event,
        Event::Message {
            level: Some("INFO".to_owned()),
            text: "--- logging initialized ---".to_owned(),
        }
    );
    // Written at +0530 (Asia/Kolkata in the container).
    assert_eq!(first.time_text, "2026-10-04T20:08:51+0530");
    assert_eq!(iso(first.time), "2026-10-04T14:38:51.0000000Z");
}

#[test]
fn yum_records_installs_and_erasures_with_inferred_years() {
    let parsed = parse_fixture("centos7/yum.log");
    assert_eq!(
        brief(&changes(&parsed)),
        [
            "install tree - 1.6.0-10.el7",
            // yum logs a reinstall as another install.
            "install tree - 1.6.0-10.el7",
            "remove tree 1.6.0-10.el7 -",
            "remove bc 1.06.95-13.el7 -",
        ]
    );
    let first = &parsed.entries[0];
    assert!(first.year_inferred);
    assert_eq!(iso(first.time), "2026-10-04T14:39:17.0000000");
    assert_eq!(first.time.unwrap().semantic(), Semantic::LocalUnknownZone);
}

#[test]
fn a_log_read_as_the_wrong_kind_is_all_problems() {
    let parsed = parse(Kind::Yum, &read("debian/dpkg.log"), october_2026());
    assert!(parsed.entries.is_empty());
    assert_eq!(parsed.problems.len(), non_blank_lines("debian/dpkg.log"));
}

mod properties {
    use super::{parse, read, Kind};
    use proptest::prelude::*;

    const KINDS: [Kind; 5] = [
        Kind::Dpkg,
        Kind::AptHistory,
        Kind::DnfRpm,
        Kind::Yum,
        Kind::DnfHistory,
    ];
    const FIXTURES: [(&str, Kind); 5] = [
        ("debian/dpkg.log", Kind::Dpkg),
        ("debian/apt/history.log", Kind::AptHistory),
        ("rocky9/dnf.rpm.log", Kind::DnfRpm),
        ("centos7/yum.log", Kind::Yum),
        ("rocky9/dnf/history.sqlite", Kind::DnfHistory),
    ];

    fn summarise_all(parsed: &packages::Parsed) {
        for entry in &parsed.entries {
            let _ = entry.summary();
        }
        for transaction in &parsed.transactions {
            let _ = transaction.summary();
        }
    }

    proptest! {
        /// Any bytes, read as any kind: entries or problems, never a panic.
        #[test]
        fn arbitrary_bytes(
            data in proptest::collection::vec(any::<u8>(), 0..2_000),
            kind in 0usize..KINDS.len(),
        ) {
            summarise_all(&parse(KINDS[kind], &data, super::october_2026()));
        }

        /// Real logs, damaged anywhere.
        #[test]
        fn damaged_fixtures(
            fixture in 0usize..FIXTURES.len(),
            flips in proptest::collection::vec((any::<usize>(), any::<u8>()), 1..20),
        ) {
            let (name, kind) = FIXTURES[fixture];
            let mut data = read(name);
            for (at, byte) in flips {
                let len = data.len();
                data[at % len] = byte;
            }
            summarise_all(&parse(kind, &data, super::october_2026()));
        }

        /// Real logs, cut anywhere.
        #[test]
        fn truncated_fixtures(fixture in 0usize..FIXTURES.len(), cut in any::<usize>()) {
            let (name, kind) = FIXTURES[fixture];
            let data = read(name);
            summarise_all(&parse(kind, &data[..cut % (data.len() + 1)], super::october_2026()));
        }
    }
}

/// dnf's history database from the Rocky Linux 9 container: four
/// transactions, their command lines, packages and dependency markers. In a
/// container no login uid is recorded (`-1`): read as unknown.
#[test]
fn dnf_history_database() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/rocky9/dnf/history.sqlite"
    );
    let data = std::fs::read(path).unwrap();
    assert_eq!(
        packages::detect("var/lib/dnf/history.sqlite"),
        Some(Kind::DnfHistory)
    );
    assert_eq!(packages::detect("home/a/history.sqlite"), None);
    let parsed = packages::parse(Kind::DnfHistory, &data, Context::default());
    assert!(parsed.problems.is_empty(), "{:?}", parsed.problems);
    let summaries: Vec<String> = parsed
        .transactions
        .iter()
        .map(packages::Transaction::summary)
        .collect();
    assert_eq!(summaries.len(), 4);
    assert!(
        summaries[1].starts_with("dnf -y -q install tree: install tree 1.8.0-"),
        "{summaries:?}"
    );
    let jq = &parsed.transactions[2];
    let names: Vec<(&str, bool)> = jq
        .changes
        .iter()
        .map(|c| (c.package.as_str(), c.automatic))
        .collect();
    assert_eq!(names, [("jq", false), ("oniguruma", true)]);
    let removal = &parsed.transactions[3].changes[0];
    assert_eq!(
        (removal.action, removal.package.as_str()),
        (Action::Remove, "tree")
    );
    assert!(removal.old_version.is_some() && removal.new_version.is_none());
    assert!(parsed.transactions.iter().all(|t| t.requested_by.is_none()));
    assert!(parsed.transactions[0].start.is_some());
}
