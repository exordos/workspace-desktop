//! Проверяет прямые обращения к драйверу SQLite в рабочем коде.
//! Обходит модули от точек входа Cargo, затем проверяет импорты и пути в коде.

use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

use syn::{
    Attribute, Expr, Item, ItemMod, Lit, Meta, Token, UseTree,
    ext::IdentExt,
    punctuated::Punctuated,
    visit::{self, Visit},
};

use crate::cargo_rules::SourceTarget;

/// Собирает точные места обращений к драйверу вне файлов хранилища.
pub(crate) fn check(root: &Path, targets: &[SourceTarget]) -> Result<Vec<String>, String> {
    let root = root.canonicalize().map_err(|error| error.to_string())?;
    let mut violations = BTreeSet::new();
    for target in targets {
        let mut scanner = Scanner {
            root: &root,
            target,
            active_files: BTreeSet::new(),
            violations: &mut violations,
        };
        scanner.file(&root.join(&target.path), true)?;
    }
    Ok(violations
        .into_iter()
        .map(|(path, line)| {
            format!("{path}:{line}: ARCH-08: используйте rusqlite только в workspace-core::storage")
        })
        .collect())
}

/// Хранит данные текущей цели Cargo и общий список найденных нарушений.
struct Scanner<'a> {
    root: &'a Path,                                // Канонический путь корня checkout.
    target: &'a SourceTarget, // Цель Cargo с именами драйвера, видимыми в пакете.
    active_files: BTreeSet<PathBuf>, // Файлы текущей ветви обхода для проверки циклов #[path].
    violations: &'a mut BTreeSet<(String, usize)>, // Путь относительно checkout и строка.
}

/// Хранит два базовых каталога, которые Rust использует для поиска подмодулей.
struct ModulePaths {
    implicit: PathBuf, // Каталог для `mod child;` без атрибута.
    explicit: PathBuf, // Каталог для относительного `#[path = "..."]`.
}

impl Scanner<'_> {
    /// Читает файл и обходит его модули, которые могут войти в рабочую сборку.
    fn file(&mut self, path: &Path, modules_by_file: bool) -> Result<(), String> {
        let canonical = path
            .canonicalize()
            .map_err(|error| format!("{}: не удалось открыть модуль: {error}", path.display()))?;
        let relative = canonical
            .strip_prefix(self.root)
            .map_err(|_| format!("{}: модуль находится вне checkout", canonical.display()))?;
        if !self.active_files.insert(canonical.clone()) {
            return Err(format!(
                "{}: модуль подключает сам себя",
                relative.display()
            ));
        }

        let source = fs::read_to_string(&canonical)
            .map_err(|error| format!("{}: {error}", relative.display()))?;
        let syntax = syn::parse_file(&source).map_err(|error| {
            format!(
                "{}:{}: {error}",
                relative.display(),
                error.span().start().line
            )
        })?;
        if in_production(&syntax.attrs) {
            let directory = path.parent().expect("у файла модуля есть каталог");
            let implicit = if modules_by_file || path.ends_with("mod.rs") {
                directory.to_path_buf()
            } else {
                directory.join(path.file_stem().expect("у файла модуля есть имя"))
            };
            let allowed = self.target.package == "workspace-core"
                && (relative == Path::new("crates/workspace-core/src/storage.rs")
                    || relative.starts_with("crates/workspace-core/src/storage"));
            self.items(
                &syntax.items,
                &ModulePaths {
                    implicit,
                    explicit: directory.to_path_buf(),
                },
                relative,
                allowed,
            )?;
        }
        self.active_files.remove(&canonical);
        Ok(())
    }

    /// Проверяет обращения в исходнике и обходит обычные и вложенные модули.
    fn items(
        &mut self,
        items: &[Item],
        paths: &ModulePaths,
        relative: &Path,
        allowed: bool,
    ) -> Result<(), String> {
        let mut visitor = DriverReferences {
            names: &self.target.sqlite_names,
            lines: BTreeSet::new(),
            modules: Vec::new(),
            allowed,
        };
        for item in items {
            visitor.visit_item(item);
        }
        for line in visitor.lines {
            self.violations
                .insert((relative.display().to_string(), line));
        }
        for module in visitor.modules {
            self.module(module, paths, relative, allowed)?;
        }
        Ok(())
    }

    /// Учитывает правила Rust для путей из `mod`, `mod.rs` и `#[path]`.
    fn module(
        &mut self,
        module: &ItemMod,
        paths: &ModulePaths,
        relative: &Path,
        allowed: bool,
    ) -> Result<(), String> {
        let name = module.ident.unraw().to_string();
        let explicit = module_path(&module.attrs)
            .map_err(|error| {
                format!(
                    "{}:{}: {error}",
                    relative.display(),
                    module.ident.span().start().line
                )
            })?
            .map(|path| paths.explicit.join(path));
        if let Some((_, items)) = &module.content {
            let directory = explicit.unwrap_or_else(|| paths.implicit.join(&name));
            let paths = ModulePaths {
                implicit: directory.clone(),
                explicit: directory,
            };
            return self.items(items, &paths, relative, allowed);
        }
        if let Some(path) = explicit {
            return self.file(&path, true);
        }

        let flat = paths.implicit.join(format!("{name}.rs"));
        let nested = paths.implicit.join(name).join("mod.rs");
        match (flat.is_file(), nested.is_file()) {
            (true, false) => self.file(&flat, false),
            (false, true) => self.file(&nested, true),
            (true, true) => Err(format!(
                "модуль найден и в {}, и в {}",
                flat.display(),
                nested.display()
            )),
            (false, false) => Err(format!(
                "не найден рабочий модуль: {} или {}",
                flat.display(),
                nested.display()
            )),
        }
    }
}

/// Собирает прямые обращения и объявления подмодулей текущего исходника.
struct DriverReferences<'a, 'ast> {
    names: &'a [String], // Имена пакета rusqlite из деклараций Cargo, включая псевдонимы.
    lines: BTreeSet<usize>, // Номера строк с прямыми обращениями, начиная с 1.
    modules: Vec<&'ast ItemMod>, // Подмодули, которые нужно проверить отдельно.
    allowed: bool,       // Файл находится внутри реализации хранилища.
}

impl DriverReferences<'_, '_> {
    /// Запоминает строку, когда первый сегмент пути — имя драйвера из Cargo.
    fn check_name(&mut self, identifier: &syn::Ident) {
        let identifier = identifier.unraw();
        if !self.allowed && self.names.iter().any(|name| identifier == name.as_str()) {
            self.lines.insert(identifier.span().start().line);
        }
    }

    /// Проверяет начало каждой ветви сгруппированного импорта.
    fn use_tree(&mut self, tree: &UseTree) {
        match tree {
            UseTree::Path(path) => self.check_name(&path.ident),
            UseTree::Name(name) => self.check_name(&name.ident),
            UseTree::Rename(rename) => self.check_name(&rename.ident),
            UseTree::Group(group) => {
                for tree in &group.items {
                    self.use_tree(tree);
                }
            }
            UseTree::Glob(_) => {}
        }
    }
}

// У узлов syn есть `attrs`, но у перечислений для этих узлов общего метода нет.
macro_rules! attributes_of {
    ($node:expr, $kind:ident, $($variant:ident),+ $(,)?) => {
        match $node {
            $(syn::$kind::$variant(node) => node.attrs.as_slice(),)+
            _ => &[],
        }
    };
}

impl<'ast> Visit<'ast> for DriverReferences<'_, 'ast> {
    /// Пропускает объявления, которые существуют только в тестовой сборке.
    fn visit_item(&mut self, item: &'ast Item) {
        if in_production(attributes_of!(
            item,
            Item,
            Const,
            Enum,
            ExternCrate,
            Fn,
            ForeignMod,
            Impl,
            Macro,
            Mod,
            Static,
            Struct,
            Trait,
            TraitAlias,
            Type,
            Union,
            Use,
        )) {
            visit::visit_item(self, item);
        }
    }

    /// Исключает тестовые методы и другие члены блока `impl`.
    fn visit_impl_item(&mut self, item: &'ast syn::ImplItem) {
        if in_production(attributes_of!(item, ImplItem, Const, Fn, Type, Macro)) {
            visit::visit_impl_item(self, item);
        }
    }

    /// Исключает тестовые члены трейта.
    fn visit_trait_item(&mut self, item: &'ast syn::TraitItem) {
        if in_production(attributes_of!(item, TraitItem, Const, Fn, Type, Macro)) {
            visit::visit_trait_item(self, item);
        }
    }

    /// Исключает тестовые объявления из внешнего блока `extern`.
    fn visit_foreign_item(&mut self, item: &'ast syn::ForeignItem) {
        if in_production(attributes_of!(item, ForeignItem, Fn, Static, Type, Macro)) {
            visit::visit_foreign_item(self, item);
        }
    }

    /// Исключает тестовые варианты перечислений вместе с полями.
    fn visit_variant(&mut self, variant: &'ast syn::Variant) {
        if in_production(&variant.attrs) {
            visit::visit_variant(self, variant);
        }
    }

    /// Исключает поля, которые существуют только в тестовой конфигурации.
    fn visit_field(&mut self, field: &'ast syn::Field) {
        if in_production(&field.attrs) {
            visit::visit_field(self, field);
        }
    }

    /// Исключает тестовые поля в выражении создания структуры.
    fn visit_field_value(&mut self, field: &'ast syn::FieldValue) {
        if in_production(&field.attrs) {
            visit::visit_field_value(self, field);
        }
    }

    /// Исключает тестовые ветки `match` вместе с их условиями и выражениями.
    fn visit_arm(&mut self, arm: &'ast syn::Arm) {
        if in_production(&arm.attrs) {
            visit::visit_arm(self, arm);
        }
    }

    /// Исключает `let` и его инициализатор, если они нужны только тестам.
    fn visit_local(&mut self, local: &'ast syn::Local) {
        if in_production(&local.attrs) {
            visit::visit_local(self, local);
        }
    }

    /// Исключает тестовый вызов макроса в позиции отдельного оператора.
    fn visit_stmt_macro(&mut self, stmt: &'ast syn::StmtMacro) {
        if in_production(&stmt.attrs) {
            visit::visit_stmt_macro(self, stmt);
        }
    }

    /// Исключает выражения, которые компилятор убирает вне тестового режима.
    fn visit_expr(&mut self, expression: &'ast Expr) {
        if in_production(attributes_of!(
            expression, Expr, Array, Assign, Async, Await, Binary, Block, Break, Call, Cast,
            Closure, Const, Continue, Field, ForLoop, Group, If, Index, Infer, Let, Lit, Loop,
            Macro, Match, MethodCall, Paren, Path, Range, RawAddr, Reference, Repeat, Return,
            Struct, Try, TryBlock, Tuple, Unary, Unsafe, While, Yield,
        )) {
            visit::visit_expr(self, expression);
        }
    }

    /// Откладывает обход подмодуля до выбора его каталога.
    fn visit_item_mod(&mut self, module: &'ast ItemMod) {
        self.modules.push(module);
    }

    /// Проверяет `use` и `pub use`, в том числе импорты с псевдонимами.
    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        self.use_tree(&item.tree);
    }

    /// Проверяет явное подключение crate, в том числе с псевдонимом.
    fn visit_item_extern_crate(&mut self, item: &'ast syn::ItemExternCrate) {
        self.check_name(&item.ident);
    }

    /// Проверяет пути внутри типов, вызовов и других выражений.
    fn visit_path(&mut self, path: &'ast syn::Path) {
        if path.segments.len() > 1 || path.leading_colon.is_some() {
            self.check_name(&path.segments[0].ident);
        }
        visit::visit_path(self, path);
    }
}

/// Проверяет, может ли объявление существовать хотя бы в одной рабочей сборке.
fn in_production(attributes: &[Attribute]) -> bool {
    attributes
        .iter()
        .all(|attribute| attribute_in_production(&attribute.meta))
}

/// Учитывает `cfg` и `cfg_attr`, которые полностью выключают код вне тестов.
fn attribute_in_production(meta: &Meta) -> bool {
    let Meta::List(list) = meta else { return true };
    if list.path.is_ident("cfg") {
        return list
            .parse_args::<Meta>()
            .ok()
            .and_then(|predicate| production_cfg(&predicate))
            != Some(false);
    }
    if list.path.is_ident("cfg_attr")
        && let Ok(arguments) = list.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
    {
        let mut arguments = arguments.iter();
        if arguments.next().and_then(production_cfg) == Some(true) {
            return arguments.all(attribute_in_production);
        }
    }
    true
}

/// Вычисляет условие при выключенном `test`; None означает возможные оба значения.
fn production_cfg(predicate: &Meta) -> Option<bool> {
    if predicate.path().is_ident("test") || predicate.path().is_ident("false") {
        return Some(false);
    }
    if predicate.path().is_ident("true") {
        return Some(true);
    }
    let Meta::List(list) = predicate else {
        return None;
    };
    let arguments = list
        .parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
        .ok()?;
    if list.path.is_ident("not") && arguments.len() == 1 {
        return production_cfg(arguments.first()?).map(|matches| !matches);
    }
    let all = list.path.is_ident("all");
    if !all && !list.path.is_ident("any") {
        return None;
    }
    let mut known = true;
    for argument in &arguments {
        match production_cfg(argument) {
            Some(value) if value != all => return Some(value),
            None => known = false,
            _ => {}
        }
    }
    known.then_some(all)
}

/// Читает путь из атрибута модуля. Не пропускает условный путь молча.
fn module_path(attributes: &[Attribute]) -> Result<Option<PathBuf>, String> {
    let mut path = None;
    for attribute in attributes {
        scan_path_attribute(&attribute.meta, true, &mut path)?;
    }
    Ok(path)
}

/// Разворачивает вложенные `cfg_attr` и принимает путь, если его условие известно.
fn scan_path_attribute(
    meta: &Meta,
    certain: bool,
    path: &mut Option<PathBuf>,
) -> Result<(), String> {
    if meta.path().is_ident("path") {
        if !certain {
            return Err(
                "для обхода требуется явный #[path] или отдельные объявления mod с #[cfg]"
                    .to_owned(),
            );
        }
        if let Meta::NameValue(meta) = meta
            && let Expr::Lit(expression) = &meta.value
            && let Lit::Str(value) = &expression.lit
        {
            *path = Some(PathBuf::from(value.value()));
            return Ok(());
        }
        return Err("ожидался строковый путь в #[path]".to_owned());
    }
    if let Meta::List(list) = meta
        && list.path.is_ident("cfg_attr")
    {
        let arguments = list
            .parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
            .map_err(|error| error.to_string())?;
        let mut arguments = arguments.iter();
        let predicate = arguments.next().and_then(production_cfg);
        if predicate != Some(false) {
            for attribute in arguments {
                scan_path_attribute(attribute, certain && predicate == Some(true), path)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// Хранит временное дерево исходников и удаляет его после проверки.
    struct Fixture(PathBuf); // Корень синтетического checkout.

    impl Fixture {
        /// Создаёт файлы, которые используются при обходе модулей.
        fn new(files: &[(&str, &str)]) -> Self {
            static NEXT_ID: AtomicUsize = AtomicUsize::new(0);
            let root = std::env::temp_dir().join(format!(
                "workspace-architecture-{}-{}",
                std::process::id(),
                NEXT_ID.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).unwrap();
            for (file, source) in files {
                let path = root.join(file);
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(path, source).unwrap();
            }
            Self(root)
        }

        /// Запускает правило с одним рабочим target и именем зависимости из Cargo.
        fn check(&self, package: &str, path: &str) -> Result<Vec<String>, String> {
            let target = SourceTarget {
                package: package.to_owned(),
                path: self.0.join(path),
                sqlite_names: vec!["rusqlite".to_owned(), "sqlite_driver".to_owned()],
            };
            check(&self.0, &[target])
        }
    }

    impl Drop for Fixture {
        /// Удаляет только временное дерево этого теста.
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Проверяет импорты, реэкспорт, псевдоним Cargo и полные пути через обычные и явные модули.
    #[test]
    fn reports_direct_driver_references_at_source_lines() {
        let fixture = Fixture::new(&[
            (
                "src/lib.rs",
                "mod flat;\n#[path = \"elsewhere/module.rs\"]\nmod moved;\n",
            ),
            (
                "src/flat.rs",
                "pub use {sqlite_driver::{Connection, Error}, std::fmt};\nmod nested;\n",
            ),
            (
                "src/flat/nested/mod.rs",
                "use rusqlite as sqlite;\ntype Model = core::storage::HistoryScope;\n",
            ),
            (
                "src/elsewhere/module.rs",
                "mod child;\n#[path = \"inline_files\"]\nmod inline { mod data; }\n",
            ),
            (
                "src/elsewhere/child.rs",
                "fn read() { ::r#rusqlite::Connection::open(); }\n",
            ),
            (
                "src/elsewhere/inline_files/data.rs",
                "type Direct = sqlite_driver::Error;\n",
            ),
        ]);
        let diagnostics = fixture.check("workspace-desktop", "src/lib.rs").unwrap();
        let sites = diagnostics
            .iter()
            .map(|message| message.split(": ARCH-08:").next().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            sites,
            [
                "src/elsewhere/child.rs:1",
                "src/elsewhere/inline_files/data.rs:1",
                "src/flat.rs:1",
                "src/flat/nested/mod.rs:1"
            ]
        );
    }

    /// Исключает тестовые объявления, сохраняя потенциальные ветки macOS, feature и not(test).
    #[test]
    fn excludes_test_only_code_and_keeps_production_configurations() {
        let fixture = Fixture::new(&[
            (
                "src/lib.rs",
                concat!(
                    "#[cfg(test)] mod no_test_file;\n",
                    "#[cfg(all(test, feature = \"extra\"))] mod no_test_feature_file;\n",
                    "#[cfg(test)] mod inline { use rusqlite::Connection; }\n",
                    "#[cfg(test)] fn check() { rusqlite::Connection::open(); }\n",
                    "#[cfg_attr(not(test), cfg(test))] use rusqlite::OptionalExtension;\n",
                    "#[cfg(any(test, target_os = \"macos\"))] use rusqlite::Error;\n",
                    "#[cfg(not(test))] type Direct = sqlite_driver::Normal;\n",
                ),
            ),
            ("tests/example.rs", "use rusqlite::Connection;\n"),
        ]);
        let diagnostics = fixture.check("workspace-core", "src/lib.rs").unwrap();
        assert_eq!(diagnostics.len(), 2);
        assert!(diagnostics[0].starts_with("src/lib.rs:6: ARCH-08:"));
        assert!(diagnostics[1].starts_with("src/lib.rs:7: ARCH-08:"));
    }

    /// Исключает тестовые объявления и выражения внутри рабочего элемента.
    #[test]
    fn excludes_test_only_members_and_statements() {
        let fixture = Fixture::new(&[(
            "src/lib.rs",
            concat!(
                "struct Model { #[cfg(test)] db: rusqlite::Connection }\n",
                "enum Event { #[cfg(test)] Test(rusqlite::Error), Ready }\n",
                "trait ModelExt { #[cfg(test)] fn db() -> rusqlite::Connection; }\n",
                "impl Model {\n",
                "    #[cfg(test)] fn db() { rusqlite::Connection::open(); }\n",
                "    fn read() {\n",
                "        #[cfg(test)] use rusqlite::Connection;\n",
                "        #[cfg(test)] let _value = rusqlite::Connection::open();\n",
                "        #[cfg(test)] rusqlite::Connection::open();\n",
                "        #[cfg(test)] rusqlite::params!();\n",
                "        match Event::Ready { #[cfg(test)] Event::Test(_) => rusqlite::Connection::open(), _ => () };\n",
                "        ::rusqlite::Connection::open();\n",
                "    }\n",
                "}\n",
            ),
        )]);
        let diagnostics = fixture.check("workspace-core", "src/lib.rs").unwrap();
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].starts_with("src/lib.rs:12: ARCH-08:"));
    }

    /// Принимает пути из известных условий и показывает неизвестное условие явно.
    #[test]
    fn handles_known_module_paths_and_reports_ambiguous_ones() {
        let source = concat!(
            "#[cfg_attr(test, path = \"excluded.rs\")]\n",
            "#[cfg_attr(not(test), cfg_attr(not(test), path = \"read.rs\"))]\n",
            "mod moved;\n",
        );
        let fixture = Fixture::new(&[
            ("src/lib.rs", source),
            ("src/read.rs", "use rusqlite::Connection;\n"),
        ]);
        let diagnostics = fixture.check("workspace-core", "src/lib.rs").unwrap();
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].starts_with("src/read.rs:1: ARCH-08:"));

        fs::write(
            fixture.0.join("src/lib.rs"),
            concat!(
                "#[cfg_attr(feature = \"native\", cfg_attr(not(test), path = \"read.rs\"))]\n",
                "mod moved;\n",
            ),
        )
        .unwrap();
        let error = fixture.check("workspace-core", "src/lib.rs").unwrap_err();
        assert!(error.starts_with("src/lib.rs:2:"));
        assert!(error.contains("явный #[path]"));
    }

    /// Разрешает драйвер в реализации Core storage, но проверяет подключённые файлы за её пределами.
    #[test]
    fn permits_core_storage_files_without_exempting_other_sources() {
        let fixture = Fixture::new(&[
            ("crates/workspace-core/src/lib.rs", "mod storage;\n"),
            (
                "crates/workspace-core/src/storage.rs",
                "use rusqlite::Connection;\nmod query;\n#[path = \"outside.rs\"] mod outside;\n",
            ),
            (
                "crates/workspace-core/src/storage/query.rs",
                "use ::rusqlite::Error;\n",
            ),
            (
                "crates/workspace-core/src/outside.rs",
                "use rusqlite::Error;\n",
            ),
        ]);
        let diagnostics = fixture
            .check("workspace-core", "crates/workspace-core/src/lib.rs")
            .unwrap();
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].starts_with("crates/workspace-core/src/outside.rs:1: ARCH-08:"));
    }

    /// Прерывает проверку, если объявление потенциально рабочее, а файла нет.
    #[test]
    fn reports_a_missing_production_module() {
        let fixture =
            Fixture::new(&[("src/lib.rs", "#[cfg(target_os = \"macos\")] mod missing;\n")]);
        let error = fixture.check("workspace-core", "src/lib.rs").unwrap_err();
        assert!(error.contains("не найден рабочий модуль"));
        assert!(error.contains("missing.rs"));
    }
}
