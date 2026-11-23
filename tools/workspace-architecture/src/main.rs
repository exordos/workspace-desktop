//! Запускает проверки манифестов и обращений к SQLite из команды `cargo check-architecture`.

mod cargo_rules;
mod sqlite;

use std::{path::Path, process::ExitCode};

/// Проверяет исходники и манифесты checkout, из которого Cargo собрал инструмент.
fn main() -> ExitCode {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("инструмент хранится в tools/workspace-architecture");
    let cargo = match cargo_rules::check(root) {
        Ok(report) => report,
        Err(error) => {
            eprintln!("ARCH-01: не удалось проверить манифесты: {error}");
            return ExitCode::FAILURE;
        }
    };
    for violation in &cargo.violations {
        eprintln!("{violation}");
    }

    let sqlite_violations = match sqlite::check(root, &cargo.targets) {
        Ok(violations) => violations,
        Err(error) => {
            eprintln!("ARCH-08: не удалось проверить исходники: {error}");
            return ExitCode::FAILURE;
        }
    };
    for violation in &sqlite_violations {
        eprintln!("{violation}");
    }

    if cargo.violations.is_empty() && sqlite_violations.is_empty() {
        println!("Архитектурные проверки пройдены.");
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
