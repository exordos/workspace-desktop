//! Связывает имена иконок Workspace с SVG и рисует их средствами Iced.
//! Сначала идёт список файлов, затем выбор формы и цвета и сам виджет.

use std::sync::LazyLock;

use iced::advanced::svg::{Handle, Renderer as _, Svg};
use iced::advanced::widget::Tree;
use iced::advanced::{Layout, Widget, layout, renderer};
use iced::{ContentFit, Element, Length, Rectangle, Renderer, Size, Theme, mouse};

use super::theme::text_muted;

/// Строит список имён и методы загрузки SVG из одного реестра.
/// Имена и исходные размеры холста SVG взяты из реестра Workspace UI.
macro_rules! icon_registry {
    ($($variant:ident => ($name:literal, $file:literal $(, $paint:ident)?),)+
     @aliases { $($alias:ident => ($alias_name:literal, $target:ident)),* $(,)? }) => {
        /// Имя иконки из реестра Workspace. По нему выбирается SVG.
        #[derive(Debug, Clone, Copy)]
        pub enum IconName {
            $($variant,)+
            $($alias,)*
        }

        impl IconName {
            pub const ALL: &[Self] = &[$(Self::$variant,)+ $(Self::$alias,)*];

            /// Возвращает исходное имя иконки для отображения и поиска.
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $name,)+
                    $(Self::$alias => $alias_name,)*
                }
            }

            /// Загружает встроенный SVG при первом обращении. Псевдонимы используют тот же файл.
            fn handle(self) -> &'static Handle {
                match self {
                    $(Self::$variant => {
                        static HANDLE: LazyLock<Handle> = LazyLock::new(|| {
                            Handle::from_memory(&include_bytes!(concat!("../../assets/icons/", $file))[..])
                        });
                        &HANDLE
                    },)+
                    $(Self::$alias => Self::$target.handle(),)*
                }
            }

            /// Проверяет, нужно ли сохранить цвет, заданный в самом SVG.
            fn uses_original_color(self) -> bool {
                match self {
                    $(Self::$variant => icon_registry!(@original $($paint)?),)+
                    $(Self::$alias => Self::$target.uses_original_color(),)*
                }
            }
        }
    };
    (@original original) => { true };
    (@original) => { false };
}

icon_registry! {
    AccountCircle => ("accountCircle", "account_circle.svg"),
    Add => ("add", "add.svg"),
    AlternateEmail => ("alternate_email", "alternate_email.svg"),
    At => ("at", "at.svg"),
    Attach => ("attach", "attach.svg"),
    Bedtime => ("bedtime", "bedtime.svg", original),
    Bell => ("bell", "bell.svg"),
    BellOff => ("bell_off", "bell_off.svg"),
    Block => ("block", "block.svg"),
    BorderColor => ("border_color", "border_color.svg"),
    Build => ("build", "build.svg"),
    BusinessCenter => ("businessCenter", "business_center.svg"),
    Calendar => ("calendar", "calendar.svg"),
    CalendarNext => ("calendar-next", "calendar-next.svg"),
    CalendarMonth => ("calendar_month", "calendar_month.svg"),
    Celebration => ("celebration", "celebration.svg"),
    ChatBubbleOutline => ("chat_bubble_outline", "chat_bubble_outline.svg"),
    ChatBubble => ("chatBubble", "chat_bubble.svg"),
    ChatLock => ("chatLock", "chat_lock.svg"),
    Check => ("check", "check.svg"),
    ChevronDown => ("chevron-down", "chevron-down.svg"),
    ChevronRight => ("chevron-right", "chevron-right.svg"),
    ChevronUp => ("chevron-up", "chevron-up.svg"),
    Close => ("close", "close.svg"),
    Command => ("command", "command.svg"),
    ComposerEmoji => ("composer-emoji", "composer-emoji.svg"),
    ComposerSend => ("composer-send", "composer-send.svg"),
    Copy => ("copy", "copy.svg"),
    DashboardCustomize => ("dashboard_customize", "dashboard_customize.svg"),
    Delete => ("delete", "delete.svg"),
    DeleteHistory => ("delete_history", "delete_history.svg"),
    DesktopWindows => ("desktop_windows", "desktop_windows.svg", original),
    Download => ("download", "download.svg"),
    DoneAll => ("done_all", "done_all.svg"),
    Drafts => ("drafts", "drafts.svg"),
    DraftsCompact => ("drafts_compact", "drafts_compact.svg"),
    Draw => ("draw", "draw.svg"),
    EncryptedSpoiler => ("encrypted_spoiler", "encrypted_spoiler.svg"),
    Error => ("error", "error.svg"),
    Files => ("files", "files.svg"),
    Flag => ("flag", "flag.svg"),
    Folder => ("folder", "folder.svg"),
    FolderCopy => ("folder_copy", "folder_copy.svg"),
    FolderOpen => ("folder_open", "folder_open.svg"),
    Folders => ("folders", "folders.svg"),
    Forward => ("forward", "forward.svg"),
    Fullscreen => ("fullscreen", "fullscreen.svg"),
    FullscreenExit => ("fullscreen_exit", "fullscreen_exit.svg"),
    GlobeLocationPin => ("globe_location_pin", "globe_location_pin.svg"),
    Grid => ("grid", "grid.svg"),
    Group => ("group", "group.svg"),
    Handshake => ("handshake", "handshake.svg"),
    Heart => ("heart", "heart.svg"),
    Home => ("home", "home.svg"),
    HomeFilled => ("home_filled", "home_filled.svg"),
    Images => ("images", "images.svg"),
    Info => ("info", "info.svg"),
    LabProfile => ("lab_profile", "lab_profile.svg"),
    Language => ("language", "language.svg"),
    Links => ("links", "links.svg"),
    LinksCompact => ("links_compact", "links_compact.svg"),
    ListArrow => ("list_arrow", "list_arrow.svg", original),
    ListBulleted => ("list_bulleted", "list_bulleted.svg"),
    Lists => ("lists", "lists.svg"),
    Logout => ("logout", "logout.svg"),
    LogoutCompact => ("logout_compact", "logout_compact.svg"),
    MagicWand => ("magic_wand", "magic_wand.svg"),
    Mail => ("mail", "mail.svg"),
    MailActivity => ("mail_activity", "mail_activity.svg"),
    MailActivityCompact => ("mail_activity_compact", "mail_activity_compact.svg"),
    MailOutline => ("mail_outline", "mail_outline.svg"),
    Marker => ("marker", "marker.svg"),
    MarkerOutline => ("marker_outline", "marker_outline.svg"),
    Mood => ("mood", "mood.svg"),
    More => ("more", "more.svg"),
    MoreVert => ("moreVert", "more_vert.svg"),
    NewWindow => ("newWindow", "new_window.svg"),
    Pen => ("pen", "pen.svg"),
    PersonAdd => ("person_add", "person_add.svg"),
    Phone => ("phone", "phone.svg"),
    PhotoCamera => ("photo_camera", "photo_camera.svg"),
    Pin => ("pin", "pin.svg"),
    Plus => ("plus", "plus.svg"),
    Profile => ("profile", "profile.svg"),
    Reply => ("reply", "reply.svg"),
    Schedule => ("schedule", "schedule.svg"),
    Search => ("search", "search.svg"),
    SelectedBookmarkIcon => ("selected_bookmark_icon", "selected_bookmark_icon.svg"),
    Send => ("send", "send.svg"),
    SentimentSatisfied => ("sentiment_satisfied", "sentiment_satisfied.svg"),
    Settings => ("settings", "settings.svg"),
    SidePanel => ("sidePanel", "side_panel.svg"),
    Smile => ("smile", "smile.svg"),
    Sparkles => ("sparkles", "sparkles.svg"),
    Star => ("star", "star.svg"),
    StarOutline => ("star_outline", "star_outline.svg"),
    Stylus => ("stylus", "stylus.svg"),
    ThumbsUp => ("thumbs-up", "thumbs-up.svg"),
    TopicFollow => ("topic_follow", "topic_follow.svg"),
    TopicInherit => ("topic_inherit", "topic_inherit.svg"),
    TopicMute => ("topic_mute", "topic_mute.svg"),
    TopicUnmute => ("topic_unmute", "topic_unmute.svg"),
    Videos => ("videos", "videos.svg"),
    Visibility => ("visibility", "visibility.svg"),
    VolumeUp => ("volumeUp", "volume_up.svg"),
    Warning => ("warning", "warning.svg"),
    WbSunny => ("wb_sunny", "wb_sunny.svg", original),
    // У channels.svg и folders.svg одинаковый рисунок, поэтому достаточно одного файла.
    @aliases { Channels => ("channels", Folders) }
}

/// Форма иконки, если для неё есть пара SVG.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum IconVariant {
    #[default]
    Outline, // Контурная форма.
    Filled, // Заполненная форма.
}

impl IconName {
    /// Выбирает форму из пары SVG. Если пары нет, возвращает исходное имя.
    const fn with_variant(self, variant: IconVariant) -> Self {
        match (self, variant) {
            (Self::Home | Self::HomeFilled, IconVariant::Outline) => Self::Home,
            (Self::Home | Self::HomeFilled, IconVariant::Filled) => Self::HomeFilled,
            _ => self,
        }
    }
}

/// Способ выбрать цвет иконки из темы.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum IconTone {
    #[default]
    Inherit, // Цвет текста или исходный цвет SVG, если он фиксирован.
    Muted,  // Приглушённый цвет темы.
    Accent, // Основной акцент темы.
    Danger, // Цвет опасного действия.
}

/// Рисует декоративную иконку. С `None` используется SVG с переданным именем.
/// Запрошенная форма применяется, если для имени зарегистрирована пара.
/// Тон `Inherit` берёт цвет текста или сохраняет фиксированный цвет SVG.
pub fn icon<Message>(
    name: IconName,
    size: u32,
    tone: IconTone,
    variant: Option<IconVariant>,
) -> Element<'static, Message> {
    let name = variant.map_or(name, |variant| name.with_variant(variant));
    Element::new(Icon { name, size, tone })
}

/// Виджет Iced, который рисует одну иконку без собственного состояния.
struct Icon {
    name: IconName, // Имя SVG после выбора формы иконки.
    size: u32,      // Сторона квадрата иконки в логических пикселях.
    tone: IconTone, // Способ подобрать цвет иконки.
}

impl<Message> Widget<Message, Theme, Renderer> for Icon {
    /// Запрашивает квадрат заданного размера.
    fn size(&self) -> Size<Length> {
        Size::new(self.size.into(), self.size.into())
    }

    /// Рассчитывает место под квадрат с учётом ограничений родительского виджета.
    fn layout(
        &mut self,
        _tree: &mut Tree,
        _renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        layout::atomic(limits, self.size, self.size)
    }

    /// Вписывает SVG в доступный квадрат, сохраняет пропорции и применяет выбранный цвет.
    fn draw(
        &self,
        _tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        let side = bounds.width.min(bounds.height);
        if side <= 0.0 || !bounds.intersects(viewport) {
            return;
        }
        let color = match self.tone {
            IconTone::Inherit if self.name.uses_original_color() => None,
            IconTone::Inherit => Some(style.text_color),
            IconTone::Muted => Some(text_muted(theme)),
            IconTone::Accent => Some(theme.palette().primary),
            IconTone::Danger => Some(theme.palette().danger),
        };
        let handle = self.name.handle();
        let source = renderer.measure_svg(handle);
        if source.width == 0 || source.height == 0 {
            return;
        }
        let fitted = ContentFit::Contain.fit(
            Size::new(source.width as f32, source.height as f32),
            Size::new(side, side),
        );
        let centered = Rectangle {
            x: bounds.center_x() - fitted.width / 2.0,
            y: bounds.center_y() - fitted.height / 2.0,
            width: fitted.width,
            height: fitted.height,
        };
        let mut svg = Svg::from(handle);
        svg.color = color;
        renderer.draw_svg(svg, centered, *viewport);
    }
}
