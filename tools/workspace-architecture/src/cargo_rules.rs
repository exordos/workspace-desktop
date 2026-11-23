//! Проверяет границы между пакетами и точки входа по метаданным Cargo.
//! Передаёт проверке исходников пути к рабочим целям и их имена зависимости от SQLite.

use std::{
    collections::BTreeSet,
    env,
    path::{Path, PathBuf},
    process::Command,
};

use serde::Deserialize;

const CORE: &str = "workspace-core";
const DESKTOP: &str = "workspace-desktop";
const HEADLESS: &str = "workspace-auth";
const CORE_FORBIDDEN: &[&str] = &[
    DESKTOP,
    "gpui",
    "block2",
    "objc2",
    "objc2-app-kit",
    "objc2-foundation",
    "objc2-user-notifications",
    "security-framework",
];

/// Результат проверки манифестов и список рабочих целей для разбора исходников.
pub(crate) struct CargoReport {
    pub(crate) violations: Vec<String>, // Сообщения в стабильном порядке.
    pub(crate) targets: Vec<SourceTarget>, // Рабочие цели двух пакетов.
}

/// Корень рабочего дерева модулей одной цели Cargo.
pub(crate) struct SourceTarget {
    pub(crate) package: String, // Имя пакета, которому принадлежит цель.
    pub(crate) path: PathBuf,   // Абсолютный путь к корневому Rust-файлу.
    pub(crate) sqlite_names: Vec<String>, // Имена драйвера SQLite в исходниках этой цели.
}

/// Нужная для проверки часть вывода cargo metadata.
#[derive(Deserialize)]
struct Metadata {
    packages: Vec<Package>, // Пакеты корневого workspace при запуске с --no-deps.
}

/// Метаданные одного пакета workspace.
#[derive(Deserialize)]
struct Package {
    name: String,                  // Настоящее имя пакета, заданное в Cargo.toml.
    manifest_path: PathBuf,        // Абсолютный путь к манифесту.
    dependencies: Vec<Dependency>, // Прямые зависимости из всех секций пакета.
    targets: Vec<Target>,          // Цели, объявленные в манифесте или обнаруженные Cargo.
}

/// Декларация прямой зависимости из манифеста Cargo.
#[derive(Deserialize)]
struct Dependency {
    name: String,           // Имя пакета зависимости, в том числе у переименованной записи.
    rename: Option<String>, // Ключ зависимости в манифесте, если он отличается от имени пакета.
    kind: Option<String>,   // None для обычной зависимости, dev или build для специальных секций.
    target: Option<String>, // Условие целевой платформы, если оно задано.
    optional: bool,         // Нужен ли отдельный feature для включения зависимости.
    path: Option<PathBuf>,  // Абсолютный путь к локальному пакету, если задан path.
}

impl Dependency {
    /// Возвращает название секции зависимости для правил и диагностики.
    fn kind(&self) -> &str {
        self.kind.as_deref().unwrap_or("normal")
    }

    /// Описывает запись манифеста с учётом переименования и целевой платформы.
    fn describe(&self) -> String {
        let name = match &self.rename {
            Some(alias) => format!("`{}` под именем `{alias}`", self.name),
            None => format!("`{}`", self.name),
        };
        let platform = self.target.as_deref().unwrap_or("все платформы");
        format!("{name} ({}, {platform})", self.kind())
    }
}

/// Точка входа, обнаруженная Cargo для библиотеки, бинарника или другой цели.
#[derive(Deserialize)]
struct Target {
    name: String,      // Имя цели Cargo.
    kind: Vec<String>, // Виды цели, например lib, bin, example или custom-build.
    src_path: PathBuf, // Абсолютный путь к исходнику точки входа.
    #[serde(default, rename = "required-features")]
    required_features: Vec<String>, // Features, без которых Cargo не собирает эту цель.
}

impl Target {
    /// Определяет, можно ли подключить цель как библиотеку Rust.
    fn is_library(&self) -> bool {
        self.kind
            .iter()
            .any(|kind| matches!(kind.as_str(), "lib" | "rlib" | "dylib"))
    }

    /// Выбирает рабочую цель и называет секцию зависимостей, доступную в её коде.
    fn dependency_kind(&self) -> Option<&'static str> {
        if self.kind.iter().any(|kind| kind == "custom-build") {
            return Some("build");
        }
        (self.is_library()
            || self
                .kind
                .iter()
                .any(|kind| matches!(kind.as_str(), "bin" | "cdylib" | "staticlib" | "proc-macro")))
        .then_some("normal")
    }
}

/// Читает метаданные корневого workspace и возвращает нарушения ARCH-01.
pub(crate) fn check(root: &Path) -> Result<CargoReport, String> {
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(cargo)
        .args([
            "metadata",
            "--offline",
            "--locked",
            "--no-deps",
            "--format-version",
            "1",
        ])
        .current_dir(root)
        .output()
        .map_err(|error| format!("Не удалось запустить cargo metadata: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "cargo metadata: {}: {}",
            output.status,
            stderr.trim()
        ));
    }

    let metadata = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("Не удалось прочитать вывод cargo metadata: {error}"))?;
    Ok(inspect(metadata, root))
}

/// Сверяет два рабочих пакета с правилами и собирает корни проверки исходников.
fn inspect(metadata: Metadata, root: &Path) -> CargoReport {
    let mut violations = Vec::new();
    let mut targets = Vec::new();
    let core_directory = metadata
        .packages
        .iter()
        .find(|package| package.name == CORE)
        .and_then(|package| package.manifest_path.parent());
    for name in [CORE, DESKTOP] {
        let Some(package) = metadata
            .packages
            .iter()
            .find(|package| package.name == name)
        else {
            violations.push(format!(
                "ARCH-01 Cargo.toml: добавьте пакет `{name}` в workspace"
            ));
            continue;
        };
        let manifest = package
            .manifest_path
            .strip_prefix(root)
            .unwrap_or(&package.manifest_path);
        let prefix = format!("ARCH-01 {}", manifest.display());
        if name == CORE {
            if !package.targets.iter().any(Target::is_library) {
                violations.push(format!("{prefix}: добавьте библиотечную цель `{CORE}`"));
            }
            for dependency in &package.dependencies {
                if CORE_FORBIDDEN.contains(&dependency.name.as_str()) {
                    violations.push(format!(
                        "{prefix}: удалите из ядра прямую зависимость {}",
                        dependency.describe(),
                    ));
                }
            }
        } else {
            check_desktop(package, core_directory, &prefix, &mut violations);
        }
        targets.extend(source_targets(package));
    }
    violations.sort();
    targets.sort_by(|left, right| (&left.package, &left.path).cmp(&(&right.package, &right.path)));
    CargoReport {
        violations,
        targets,
    }
}

/// Проверяет связь Desktop с ядром и декларации для запуска с окном и без окна.
fn check_desktop(
    package: &Package,
    core_directory: Option<&Path>,
    prefix: &str,
    violations: &mut Vec<String>,
) {
    if let Some(core_directory) = core_directory
        && !package.dependencies.iter().any(|dependency| {
            dependency.name == CORE
                && dependency.kind() == "normal"
                && dependency.target.is_none()
                && !dependency.optional
                && dependency.path.as_deref() == Some(core_directory)
        })
    {
        violations.push(format!(
            "{prefix}: добавьте `{CORE}` как обязательную обычную зависимость от пакета ядра в этом workspace для всех платформ",
        ));
    }
    if !package
        .dependencies
        .iter()
        .any(|dependency| dependency.name == "gpui" && dependency.kind() == "normal")
    {
        violations.push(format!(
            "{prefix}: объявите `gpui` как обычную зависимость с `optional = true`",
        ));
    }
    for gpui in package.dependencies.iter().filter(|dependency| {
        dependency.name == "gpui" && dependency.kind() != "dev" && !dependency.optional
    }) {
        violations.push(format!(
            "{prefix}: сделайте зависимость {} опциональной для сборки без окна",
            gpui.describe(),
        ));
    }
    for name in [DESKTOP, HEADLESS] {
        let Some(binary) = package
            .targets
            .iter()
            .find(|target| target.name == name && target.kind.iter().any(|kind| kind == "bin"))
        else {
            violations.push(format!("{prefix}: добавьте бинарник `{name}`"));
            continue;
        };
        if name == DESKTOP
            && !binary
                .required_features
                .iter()
                .any(|feature| feature == "gpui")
        {
            violations.push(format!(
                "{prefix}: добавьте `gpui` в `required-features` бинарника `{name}`",
            ));
        }
        if name == HEADLESS && !binary.required_features.is_empty() {
            violations.push(format!(
                "{prefix}: уберите `required-features` у бинарника `{name}` ({})",
                binary.required_features.join(", "),
            ));
        }
    }
}

/// Выбирает точки входа поставляемого кода и собирает доступные им имена SQLite.
fn source_targets(package: &Package) -> impl Iterator<Item = SourceTarget> + '_ {
    package.targets.iter().filter_map(|target| {
        let kind = target.dependency_kind()?;
        let sqlite_names = package
            .dependencies
            .iter()
            .filter(|dependency| dependency.name == "rusqlite" && dependency.kind() == kind)
            .map(|dependency| {
                dependency
                    .rename
                    .as_deref()
                    .unwrap_or(&dependency.name)
                    .replace('-', "_")
            })
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        Some(SourceTarget {
            package: package.name.clone(),
            path: target.src_path.clone(),
            sqlite_names,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Создаёт пару рабочих пакетов с корректной связью и точками входа.
    fn workspace() -> Metadata {
        serde_json::from_value(serde_json::json!({
            "packages": [
                {
                    "name": CORE,
                    "manifest_path": "/repo/crates/workspace-core/Cargo.toml",
                    "dependencies": [{"name": "rusqlite", "optional": false}],
                    "targets": [
                        {"name": "workspace_core", "kind": ["lib"], "src_path": "/repo/core/lib.rs"},
                        {"name": "support", "kind": ["test"], "src_path": "/repo/core/support.rs"}
                    ]
                },
                {
                    "name": DESKTOP,
                    "manifest_path": "/repo/crates/workspace-desktop/Cargo.toml",
                    "dependencies": [
                        {"name": CORE, "optional": false, "path": "/repo/crates/workspace-core"},
                        {"name": "gpui", "optional": true, "target": "cfg(target_os = \"macos\")"}
                    ],
                    "targets": [
                        {"name": HEADLESS, "kind": ["bin"], "src_path": "/repo/cli/run.rs"},
                        {"name": DESKTOP, "kind": ["bin"], "src_path": "/repo/desktop/main.rs", "required-features": ["gpui"]},
                        {"name": "preview", "kind": ["example"], "src_path": "/repo/desktop/preview.rs"},
                        {"name": "speed", "kind": ["bench"], "src_path": "/repo/desktop/speed.rs"}
                    ]
                }
            ]
        })).expect("метаданные двух пакетов")
    }

    /// Декларация зависимости помогает изменять по одному свойству за пример.
    fn dependency(name: &str) -> Dependency {
        Dependency {
            name: name.into(),
            rename: None,
            kind: None,
            target: None,
            optional: false,
            path: None,
        }
    }

    /// Принимает переименованную связь с ядром из этого workspace.
    #[test]
    fn accepts_renamed_local_core_dependency() {
        let mut metadata = workspace();
        metadata.packages[1].dependencies[0].rename = Some("app_core".into());
        let CargoReport { violations, .. } = inspect(metadata, Path::new("/repo"));

        assert!(violations.is_empty(), "{violations:?}");
    }

    /// Отклоняет одноимённый пакет из реестра или из другой папки.
    #[test]
    fn rejects_core_dependency_outside_workspace() {
        for path in [None, Some("/elsewhere/workspace-core".into())] {
            let mut metadata = workspace();
            metadata.packages[1].dependencies[0].path = path;
            let CargoReport { violations, .. } = inspect(metadata, Path::new("/repo"));

            assert_eq!(violations.len(), 1, "{violations:?}");
            assert!(violations[0].contains("от пакета ядра в этом workspace"));
        }
    }

    /// Сообщает об отсутствующем пакете ядра один раз.
    #[test]
    fn reports_missing_workspace_core_once() {
        let mut metadata = workspace();
        metadata.packages.remove(0);
        let CargoReport { violations, .. } = inspect(metadata, Path::new("/repo"));

        assert_eq!(
            violations,
            ["ARCH-01 Cargo.toml: добавьте пакет `workspace-core` в workspace"]
        );
    }

    /// Требует библиотечную цель ядра при наличии тестовых целей.
    #[test]
    fn rejects_core_with_only_test_targets() {
        let mut metadata = workspace();
        metadata.packages[0]
            .targets
            .retain(|target| target.name == "support");
        let CargoReport { violations, .. } = inspect(metadata, Path::new("/repo"));

        assert_eq!(
            violations,
            [
                "ARCH-01 crates/workspace-core/Cargo.toml: добавьте библиотечную цель `workspace-core`"
            ]
        );
    }

    /// Проверяет точные имена пакетов, включая алиас и секции для тестов и сборки.
    #[test]
    fn rejects_forbidden_core_dependencies_in_all_sections() {
        let mut metadata = workspace();
        metadata.packages[0].dependencies.extend([
            Dependency {
                rename: Some("window".into()),
                kind: Some("dev".into()),
                ..dependency("gpui")
            },
            Dependency {
                kind: Some("build".into()),
                target: Some("cfg(target_os = \"macos\")".into()),
                ..dependency("objc2-app-kit")
            },
            dependency(DESKTOP),
            dependency("gpui-test-utils"),
        ]);
        let CargoReport { violations, .. } = inspect(metadata, Path::new("/repo"));

        assert_eq!(violations.len(), 3, "{violations:?}");
        assert!(
            violations
                .iter()
                .all(|message| message.starts_with("ARCH-01 crates/workspace-core/Cargo.toml:"))
        );
        assert!(
            violations
                .iter()
                .any(|message| message.contains("`gpui` под именем `window` (dev,"))
        );
        assert!(violations.iter().any(|message| {
            message.contains("`objc2-app-kit` (build, cfg(target_os = \"macos\"))")
        }));
        assert!(
            violations
                .iter()
                .any(|message| message.contains("`workspace-desktop` (normal,"))
        );
    }

    /// Проверяет пропавший бинарник и обязательные зависимости, мешающие запуску без окна.
    #[test]
    fn rejects_missing_binary_and_required_gpui_dependencies() {
        let mut metadata = workspace();
        let desktop = &mut metadata.packages[1];
        desktop.targets.retain(|target| target.name != DESKTOP);
        desktop.targets[0].required_features.push("extra".into());
        desktop.dependencies[0].target = Some("cfg(unix)".into());
        desktop.dependencies[1].optional = false;
        desktop.dependencies.extend([
            Dependency {
                kind: Some("build".into()),
                ..dependency("gpui")
            },
            Dependency {
                kind: Some("dev".into()),
                ..dependency("gpui")
            },
        ]);
        let CargoReport { violations, .. } = inspect(metadata, Path::new("/repo"));

        assert_eq!(violations.len(), 5, "{violations:?}");
        for expected in [
            "добавьте бинарник `workspace-desktop`",
            "уберите `required-features` у бинарника `workspace-auth` (extra)",
            "добавьте `workspace-core` как обязательную обычную зависимость",
            "зависимость `gpui` (normal, cfg(",
            "зависимость `gpui` (build,",
        ] {
            assert!(
                violations.iter().any(|message| message.contains(expected)),
                "{violations:?}"
            );
        }
    }

    /// Проверяет, что feature gpui нужен именно бинарнику окна и указан у обычной зависимости.
    #[test]
    fn rejects_window_without_gpui() {
        let mut metadata = workspace();
        let desktop = &mut metadata.packages[1];
        desktop.targets[1].required_features = vec!["other".into()];
        desktop.dependencies[1].kind = Some("dev".into());
        let CargoReport { violations, .. } = inspect(metadata, Path::new("/repo"));

        assert_eq!(violations.len(), 2, "{violations:?}");
        assert!(
            violations
                .iter()
                .any(|message| message.contains("добавьте `gpui` в `required-features`"))
        );
        assert!(
            violations
                .iter()
                .any(|message| message.contains("объявите `gpui` как обычную зависимость"))
        );
    }

    /// Проверяет выбранные цели и правильные алиасы для приложения и скрипта сборки.
    #[test]
    fn selects_app_targets_and_sqlite_aliases_by_dependency_kind() {
        let mut metadata = workspace();
        let core = &mut metadata.packages[0];
        core.dependencies.extend([
            Dependency {
                rename: Some("sqlite-db".into()),
                target: Some("cfg(unix)".into()),
                ..dependency("rusqlite")
            },
            Dependency {
                rename: Some("build-db".into()),
                kind: Some("build".into()),
                ..dependency("rusqlite")
            },
            Dependency {
                rename: Some("test-db".into()),
                kind: Some("dev".into()),
                ..dependency("rusqlite")
            },
        ]);
        core.targets.push(Target {
            name: "build-script-build".into(),
            kind: vec!["custom-build".into()],
            src_path: "/repo/custom-builder.rs".into(),
            required_features: Vec::new(),
        });
        let CargoReport {
            violations,
            targets,
        } = inspect(metadata, Path::new("/repo"));

        assert!(violations.is_empty(), "{violations:?}");
        let paths_and_aliases = targets
            .iter()
            .map(|target| {
                (
                    target.path.to_str().unwrap(),
                    target.sqlite_names.as_slice(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            paths_and_aliases,
            [
                (
                    "/repo/core/lib.rs",
                    &["rusqlite".to_owned(), "sqlite_db".to_owned()][..]
                ),
                ("/repo/custom-builder.rs", &["build_db".to_owned()][..]),
                ("/repo/cli/run.rs", &[][..]),
                ("/repo/desktop/main.rs", &[][..]),
            ]
        );
    }
}
