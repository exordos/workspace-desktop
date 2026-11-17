//! Разбирает параметры сцены, открывает рабочий ChatRow и запускает захват.

mod capture;
mod chat_row;

use std::{
    cell::Cell,
    path::{Path, PathBuf},
    process::{Command, exit},
    rc::Rc,
};

use gpui::{
    Application, Bounds, Context, Pixels, Size, Window, WindowBounds, WindowOptions, canvas, div,
    prelude::*, px, size,
};
use workspace_desktop::gpui_client::ui::{Palette, ThemeMode, WorkspaceAssets};

use self::chat_row::{DESCRIPTION, ID, ROW_WIDTH, Sample};

/// Ошибка запуска сцены, захвата окна или записи артефакта.
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Ширина отступа слева и сверху в обычном кадре.
const INSET: f32 = 16.;
/// Насколько сдвигаем только макет сцены, чтобы проверить обнаружение разницы.
const CHECK_SHIFT: f32 = 12.;

/// Ввод оператора для одного запуска сцены.
struct Config {
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
            return Err(format!(
                "Использование: {ID} <light|dark> <ширина>x<высота> <новый-каталог> [--verify]"
            )
            .into());
        };
        if id != ID {
            return Err(format!("Неизвестная сцена {id}. Доступна {ID}.").into());
        }
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
            return Err("Для ChatRow нужно окно размером не меньше 140x96".into());
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
        self.out.join(format!("{ID}-{}{dirty}-{pass}", self.sha))
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
    sample: Sample,                            // Общие данные истории и кадра.
    inset: f32,                                // Левый отступ контейнера строки.
    painted: Rc<Cell<Option<Bounds<Pixels>>>>, // Границы, полученные на этапе рисования.
}

impl Render for View {
    /// Рисует рабочую строку в обычной палитре и отмечает её фактическую область.
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
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
            .bg(Palette::for_mode(self.sample.theme).surface)
            .pt(px(INSET))
            .pl(px(self.inset))
            .pr(px(INSET))
            .child(
                div()
                    .relative()
                    .w(px(ROW_WIDTH))
                    .max_w_full()
                    .child(self.sample.row(None, |_, _| {}))
                    .child(ruler),
            )
    }
}

/// Запускает одно отдельное окно, при необходимости выполняет три кадра в нём.
pub fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args == ["--list"] {
        println!("{ID} — {DESCRIPTION}");
        return Ok(());
    }
    let config = Config::parse(&args)?;
    capture::ensure_screen_access()?;
    Application::new()
        .with_assets(WorkspaceAssets)
        .run(move |cx| {
            let painted = Rc::new(Cell::new(None));
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
                    cx.new(|_| View {
                        sample: chat_row::fixture(config.theme),
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
                        eprintln!("Не удалось активировать окно сцены {ID}: {error}");
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
                    eprintln!("Не удалось открыть окно сцены {ID}: {error}");
                    exit(1);
                }
            }
        });
    Ok(())
}
