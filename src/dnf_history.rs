//! dnf's history database (`/var/lib/dnf/history.sqlite`), read with
//! `sootmark-sqlite` and its write-ahead log: one transaction per row of
//! `trans` (the command line, the login uid that ran it, begin and end as
//! Unix seconds), its packages from `trans_item` joined to `rpm`, with
//! libdnf's action and reason codes (`TransactionItemAction`,
//! `TransactionItemReason` in libdnf's `transaction/Types.hpp`).
//!
//! The uid is the login uid: the person behind `sudo dnf`, not root; `-1`
//! when there was none (a container, a service), read as unknown.

use std::collections::HashMap;

use common::time::Ts;
use sqlite::{Database, Value};

use crate::{Action, Change, Parsed, Transaction, User};

/// Read a history database and its `-wal` file (`log`, may be empty).
pub(crate) fn parse(data: &[u8], log: &[u8], parsed: &mut Parsed) {
    let db = match Database::open_with_wal(data, log) {
        Ok(db) => db,
        Err(error) => {
            parsed
                .problems
                .push(format!("not a dnf history database: {error}"));
            return;
        }
    };
    parsed.problems.extend(db.problems.iter().cloned());
    let packages = rows(&db, "rpm", parsed)
        .into_iter()
        .filter_map(|row| Some((row.integer("item_id")?, row)))
        .collect::<HashMap<_, _>>();
    let mut changes: HashMap<i64, Vec<Change>> = HashMap::new();
    for item in rows(&db, "trans_item", parsed) {
        let package = item.integer("item_id").and_then(|id| packages.get(&id));
        let (Some(transaction), Some(package)) = (item.integer("trans_id"), package) else {
            continue;
        };
        if let Some(change) = change(&item, package) {
            changes.entry(transaction).or_default().push(change);
        }
    }
    for row in rows(&db, "trans", parsed) {
        let id = row.integer("id").unwrap_or_default();
        parsed.transactions.push(transaction(
            &row,
            id,
            changes.remove(&id).unwrap_or_default(),
        ));
    }
}

fn transaction(row: &Named, id: i64, changes: Vec<Change>) -> Transaction {
    let time = |name: &str| {
        row.integer(name)
            .filter(|&s| s > 0)
            .map(Ts::from_unix_seconds)
    };
    let uid = row
        .integer("user_id")
        .and_then(|uid| u32::try_from(uid).ok())
        .filter(|&uid| uid != u32::MAX);
    Transaction {
        line: usize::try_from(id).unwrap_or_default(),
        last_line: usize::try_from(id).unwrap_or_default(),
        offset: 0,
        start: time("dt_begin"),
        end: time("dt_end"),
        command: row.text("cmdline").map(|args| format!("dnf {args}")),
        requested_by: uid.map(|uid| User {
            name: None,
            uid: Some(uid),
        }),
        changes,
        error: None,
        other: Vec::new(),
    }
}

/// A transaction item as a change: libdnf's action codes, the outgoing
/// sides of replacements naming the version that was there.
fn change(item: &Named, package: &Named) -> Option<Change> {
    let (action, outgoing) = match item.integer("action")? {
        1 => (Action::Install, false),
        2 => (Action::Downgrade, false),
        3 => (Action::Downgrade, true),
        4 => (Action::Obsolete, false),
        5 => (Action::Obsolete, true),
        6 => (Action::Upgrade, false),
        7 => (Action::Upgrade, true),
        8 => (Action::Remove, true),
        9 => (Action::Reinstall, false),
        10 => (Action::Reinstall, true),
        // 11, a reason change: nothing installed or removed.
        _ => return None,
    };
    let version = version(package);
    Some(Change {
        action,
        package: package.text("name")?,
        arch: package.text("arch"),
        old_version: version.clone().filter(|_| outgoing),
        new_version: version.filter(|_| !outgoing),
        // 1: a dependency; 4: a weak dependency.
        automatic: matches!(item.integer("reason"), Some(1 | 4)),
    })
}

/// `[epoch:]version-release`, as rpm writes it.
fn version(package: &Named) -> Option<String> {
    let version = package.text("version")?;
    let release = package
        .text("release")
        .map_or(String::new(), |r| format!("-{r}"));
    Some(match package.integer("epoch").filter(|&e| e > 0) {
        Some(epoch) => format!("{epoch}:{version}{release}"),
        None => format!("{version}{release}"),
    })
}

/// A row with its table's column names.
struct Named {
    columns: std::rc::Rc<[String]>,
    values: Vec<Value>,
}

impl Named {
    fn value(&self, name: &str) -> Option<&Value> {
        let at = self.columns.iter().position(|c| c == name)?;
        self.values.get(at)
    }

    fn integer(&self, name: &str) -> Option<i64> {
        self.value(name).and_then(Value::as_integer)
    }

    fn text(&self, name: &str) -> Option<String> {
        self.value(name)
            .and_then(Value::as_text)
            .filter(|t| !t.is_empty())
            .map(str::to_owned)
    }
}

/// Every row of `table`; a missing table or damage goes to the problems.
fn rows(db: &Database<'_>, table: &str, parsed: &mut Parsed) -> Vec<Named> {
    let Some(schema) = db.table(table) else {
        parsed.problems.push(format!("no {table} table"));
        return Vec::new();
    };
    let columns: std::rc::Rc<[String]> = schema
        .column_names()
        .into_iter()
        .map(str::to_owned)
        .collect();
    let Ok(mut rows) = db.rows(table) else {
        return Vec::new();
    };
    let named = rows
        .by_ref()
        .map(|row| Named {
            columns: columns.clone(),
            values: row.values,
        })
        .collect();
    parsed
        .problems
        .extend(rows.problems().iter().map(|p| format!("{table}: {p}")));
    named
}
