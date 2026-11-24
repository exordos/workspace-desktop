//! Разбирает параметры сцены, открывает выбранный компонент и запускает захват.

mod capture;
mod chat_row;
mod new_chat_components;

use std::{
    cell::Cell,
    path::{Path, PathBuf},
    process::{Command, exit},
    rc::Rc,
};

use gpui::{
    AnyElement, Application, Bounds, Context, Pixels, Size, Window, WindowBounds, WindowOptions,
    canvas, div, prelude::*, px, size,
};
use workspace_desktop::gpui_client::ui::{FilterGroup, Palette, ThemeMode, WorkspaceAssets};

use self::chat_row::Sample;

#[cfg(debug_assertions)]
use workspace_desktop::gpui_client::sidebar::{NewChatPreview, new_chat_preview};

/// Ошибка запуска сцены, захвата окна или записи артефакта.
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Ширина отступа слева и сверху в обычном кадре.
const INSET: f32 = 16.;
/// Насколько сдвигаем только макет сцены, чтобы проверить обнаружение разницы.
const CHECK_SHIFT: f32 = 12.;

/// Сцена одного рабочего компонента на фиксированных данных.
#[derive(Clone, Copy)]
enum Scene {
    ChatRow,           // Строка каталога из набора UI-02.
    PersonRow,         // Состояния карточки человека.
    ChannelFilter,     // Четыре фильтра с выбранной средней вкладкой.
    ChannelFilterEdge, // Выбранный сегмент примыкает к внешнему углу.
    ChannelDialog,     // Общая карточка модалки с полями и кнопками.
    #[cfg(debug_assertions)]
    NewChat(NewChatPreview), // Рабочий экран UI-19 на фиксированных данных.
}

impl Scene {
    /// Возвращает стабильное имя сцены для CLI и артефактов.
    fn id(self) -> &'static str {
        match self {
            Self::ChatRow => chat_row::ID,
            Self::PersonRow => "person-row",
            Self::ChannelFilter => "channel-filters",
            Self::ChannelFilterEdge => "channel-filters-edge",
            Self::ChannelDialog => "channel-dialog",
            #[cfg(debug_assertions)]
            Self::NewChat(screen) => screen.id(),
        }
    }

    /// Объясняет, что будет в кадре.
    fn description(self) -> &'static str {
        match self {
            Self::ChatRow => chat_row::DESCRIPTION,
            Self::PersonRow => "Строка человека с присутствием, выбором и длинным именем.",
            Self::ChannelFilter => "Фильтры каналов и архива с выделением «Без подписки».",
            Self::ChannelFilterEdge => "Скруглённая внешняя граница у выбранного крайнего фильтра.",
            Self::ChannelDialog => "Карточка диалога с заголовком, полями и рядом действий.",
            #[cfg(debug_assertions)]
            Self::NewChat(screen) => screen.description(),
        }
    }

    /// Берёт исходную ширину компонента в логических пикселях.
    fn width(self) -> f32 {
        match self {
            Self::ChatRow => chat_row::ROW_WIDTH,
            Self::PersonRow | Self::ChannelFilter | Self::ChannelFilterEdge => 299.,
            Self::ChannelDialog => 351.,
            #[cfg(debug_assertions)]
            Self::NewChat(screen) => screen.width(),
        }
    }

    /// Указывает фактическую рабочую палитру синтетического состояния.
    fn palette(self) -> &'static str {
        match self {
            #[cfg(debug_assertions)]
            Self::NewChat(_) => "blue-cold",
            _ => "blue-mist",
        }
    }

    /// Возвращает имя набора исходных данных для метаданных снимка.
    fn fixture(self) -> &'static str {
        match self {
            Self::ChatRow => "UI-02",
            Self::PersonRow
            | Self::ChannelFilter
            | Self::ChannelFilterEdge
            | Self::ChannelDialog => "UI-19",
            #[cfg(debug_assertions)]
            Self::NewChat(_) => "UI-19",
        }
    }
}

/// Список сцен, допустимых в командной строке.
const SCENES: &[Scene] = &[
    Scene::ChatRow,
    Scene::PersonRow,
    Scene::ChannelFilter,
    Scene::ChannelFilterEdge,
    Scene::ChannelDialog,
    #[cfg(debug_assertions)]
    Scene::NewChat(NewChatPreview::Catalog),
    #[cfg(debug_assertions)]
    Scene::NewChat(NewChatPreview::Unsubscribed),
    #[cfg(debug_assertions)]
    Scene::NewChat(NewChatPreview::People),
    #[cfg(debug_assertions)]
    Scene::NewChat(NewChatPreview::CreateChannel),
    #[cfg(debug_assertions)]
    Scene::NewChat(NewChatPreview::Members),
];

/// Ввод оператора для одного запуска сцены.
struct Config {
    scene: Scene,           // Компонент, для которого сохраняем кадр.
    theme: ThemeMode,       // Светлая или тёмная рабочая палитра.
    viewport: Size<Pixels>, // Размер области содержимого в логических пикселях.
    out: PathBuf,           // Новый каталог для кадров и метаданных.
    verify: bool,           // Снять повтор и контрольный сдвиг в том же окне.
    sha: String,            // Коммит исходников, который сохраняем в имени и JSON.
    dirty: bool,            // Были ли локальные изменения в отслеживаемых файлах.
}

impl Config {
    /// Проверяет имя сцены, тему, размер и каталог до запуска окна.
    fn parse(args: &[String]) -> Result<Self> {
        let [id, theme, dimensions, out, flags @ ..] = args else {
            return Err(
                "Использование: <сцена> <light|dark> <ширина>x<высота> <новый-каталог> [--verify]"
                    .into(),
            );
        };
        let scene = SCENES
            .iter()
            .copied()
            .find(|scene| scene.id() == id)
            .ok_or_else(|| format!("Неизвестная сцена {id}. Список сцен выводит --list."))?;
        let theme = match theme.as_str() {
            "light" => ThemeMode::Light,
            "dark" => ThemeMode::Dark,
            _ => return Err("Тема должна быть light или dark".into()),
        };
        let (w, h) = dimensions
            .split_once('x')
            .ok_or("Размер задаётся в формате ширинаxвысота")?;
        let (w, h) = (w.parse::<u16>()?, h.parse::<u16>()?);
        if w < 140 || h < 96 {
            return Err("Для сцены нужно окно размером не меньше 140x96".into());
        }
        let verify = match flags {
            [] => false,
            [flag] if flag == "--verify" => true,
            _ => return Err("Допустим только флаг --verify".into()),
        };
        let (sha, dirty) = revision()?;
        let out = PathBuf::from(out);
        std::fs::create_dir(&out)?;
        Ok(Self {
            scene,
            theme,
            viewport: size(px(w as f32), px(h as f32)),
            out,
            verify,
            sha,
            dirty,
        })
    }

    /// Название темы в метаданных кадра.
    fn theme_name(&self) -> &'static str {
        match self.theme {
            ThemeMode::Light => "light",
            ThemeMode::Dark => "dark",
        }
    }

    /// Составляет имя каталога одного кадра из сцены, SHA и названия прогона.
    fn artifact_dir(&self, pass: &str) -> PathBuf {
        let dirty = if self.dirty { "-dirty" } else { "" };
        self.out
            .join(format!("{}-{}{dirty}-{pass}", self.scene.id(), self.sha))
    }
}

/// Получает SHA и состояние рабочего дерева, где собран пример.
fn revision() -> Result<(String, bool)> {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
    let sha = Command::new("git")
        .current_dir(repo)
        .args(["rev-parse", "HEAD"])
        .output()?;
    if !sha.status.success() {
        return Err("Не удалось получить SHA сцены из git".into());
    }
    let status = Command::new("git")
        .current_dir(repo)
        .args(["status", "--porcelain", "--untracked-files=normal"])
        .output()?;
    if !status.status.success() {
        return Err("Не удалось проверить рабочее дерево сцены".into());
    }
    Ok((
        String::from_utf8(sha.stdout)?.trim().to_owned(),
        !status.stdout.is_empty(),
    ))
}

/// Вид сцены с датчиком фактических границ строки.
struct View {
    scene: Scene,                 // Компонент, который виден в окне.
    theme: ThemeMode,             // Светлая или тёмная палитра.
    filters: Option<FilterGroup>, // Состояние полосы в сценах фильтров.
    chat_row: Option<Sample>,     // Исходный снимок для сцены каталога.
    #[cfg(debug_assertions)]
    new_chat: Option<gpui::AnyView>, // Настоящий экран, собранный до первого кадра.
    inset: f32,                   // Левый отступ контейнера строки.
    painted: Rc<Cell<Option<Bounds<Pixels>>>>, // Границы, полученные на этапе рисования.
}

impl Render for View {
    /// Рисует рабочую строку в обычной палитре и отмечает её фактическую область.
    fn render(&mut self, window: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::for_mode(self.theme);
        let component: AnyElement = match self.scene {
            Scene::ChatRow => self
                .chat_row
                .as_ref()
                .expect("данные UI-02")
                .row(None, |_, _| {}),
            Scene::PersonRow => {
                new_chat_components::people(palette, [false, true, false], |_, _, _| {})
                    .into_any_element()
            }
            Scene::ChannelFilter | Scene::ChannelFilterEdge => {
                let selected = usize::from(matches!(self.scene, Scene::ChannelFilter));
                new_chat_components::channels(
                    palette,
                    selected,
                    self.filters.as_ref().expect("состояние фильтров"),
                    window,
                    |_, _, _| {},
                )
                .into_any_element()
            }
            Scene::ChannelDialog => new_chat_components::modal(palette, window).into_any_element(),
            #[cfg(debug_assertions)]
            Scene::NewChat(_) => self
                .new_chat
                .clone()
                .expect("экран UI-19")
                .into_any_element(),
        };
        let painted = self.painted.clone();
        let ruler = canvas(
            |bounds, _, _| bounds,
            move |_, bounds, _, _| painted.set(Some(bounds)),
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full();
        div()
            .size_full()
            .bg(palette.surface)
            .pt(px(INSET))
            .pl(px(self.inset))
            .pr(px(INSET))
            .child(
                div()
                    .relative()
                    .w(px(self.scene.width()))
                    .max_w_full()
                    .when(is_sidebar(self.scene), |div| {
                        div.h(window.viewport_size().height - px(2. * INSET))
                    })
                    .child(component)
                    .child(ruler),
            )
    }
}

/// Даёт вмещаемой в окно панели полную высоту рабочей области.
fn is_sidebar(scene: Scene) -> bool {
    #[cfg(debug_assertions)]
    if let Scene::NewChat(screen) = scene {
        return screen.is_sidebar();
    }
    let _ = scene;
    false
}

/// Запускает одно отдельное окно, при необходимости выполняет три кадра в нём.
pub fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args == ["--list"] {
        for scene in SCENES {
            println!("{} — {}", scene.id(), scene.description());
        }
        return Ok(());
    }
    let config = Config::parse(&args)?;
    capture::ensure_screen_access()?;
    Application::new()
        .with_assets(WorkspaceAssets)
        .run(move |cx| {
            let painted = Rc::new(Cell::new(None));
            let id = config.scene.id();
            let bounds = Bounds::centered(None, config.viewport, cx);
            let window = cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: None,
                    show: false,
                    is_resizable: false,
                    ..WindowOptions::default()
                },
                |_, cx| {
                    #[cfg(debug_assertions)]
                    let new_chat = match config.scene {
                        Scene::NewChat(screen) => Some(new_chat_preview(screen, config.theme, cx)),
                        _ => None,
                    };
                    cx.new(|cx| View {
                        scene: config.scene,
                        filters: matches!(
                            config.scene,
                            Scene::ChannelFilter | Scene::ChannelFilterEdge
                        )
                        .then(|| FilterGroup::new(4, cx)),
                        #[cfg(debug_assertions)]
                        new_chat,
                        theme: config.theme,
                        chat_row: matches!(config.scene, Scene::ChatRow)
                            .then(|| chat_row::fixture(config.theme)),
                        inset: INSET,
                        painted: painted.clone(),
                    })
                },
            );
            match window {
                Ok(window) => {
                    cx.activate(true);
                    let activation = window
                        .update(cx, |_, window, _| {
                            capture::disable_open_animation(window)?;
                            window.activate_window();
                            Ok::<_, Box<dyn std::error::Error>>(())
                        })
                        .map_err(|error| error.to_string())
                        .and_then(|result| result.map_err(|error| error.to_string()));
                    if let Err(error) = activation {
                        eprintln!("Не удалось активировать окно сцены {id}: {error}");
                        exit(1);
                    }
                    cx.spawn(async move |cx| {
                        if let Err(error) = capture::run(&config, window, painted, cx).await {
                            eprintln!("{error}");
                            exit(1);
                        }
                        let _ = cx.update(|cx| cx.quit());
                    })
                    .detach();
                }
                Err(error) => {
                    eprintln!("Не удалось открыть окно сцены {id}: {error}");
                    exit(1);
                }
            }
        });
    Ok(())
}
