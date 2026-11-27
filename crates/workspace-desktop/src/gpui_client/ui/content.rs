//! Компоненты чатов: аватары, счётчики, строки каталога и пузыри сообщений.

use gpui::{
    AnyElement, App, Corners, Div, ElementId, FocusHandle, Hsla, ImageSource, ObjectFit, Pixels,
    SharedString, Stateful, Window, div, img, prelude::*, px, relative, rgba,
};
use unicode_segmentation::UnicodeSegmentation;

use crate::{
    gpui_client::preferences::Language,
    platform::message_time::{CalendarDate, LocalTime},
};

use super::{
    Icon, Palette, TextRole,
    controls::activate,
    label,
    theme::{
        CONTROL_BORDER, MESSAGE_BUBBLE_RADIUS, PANEL_RADIUS, SURFACE_RADIUS, ellipsized_label,
        label_width,
    },
};

/// Невидимая точка переноса перед резервом времени.
pub(in crate::gpui_client) const BUBBLE_TAIL_BREAK: &str = "\u{200b}";

/// Показывает календарную дату из ISO-строки Workspace на языке профиля.
pub(in crate::gpui_client) fn profile_joined_date(
    value: &str,
    language: Language,
) -> Option<String> {
    LocalTime::system()
        .profile_date(value)
        .map(|date| calendar_date_label(date, language))
}

/// Подставляет местные календарные поля в шаблон языка аккаунта.
pub(in crate::gpui_client) fn calendar_date_label(
    date: CalendarDate,
    language: Language,
) -> String {
    let month_key = [
        "desktop.profile.joinedMonths.jan",
        "desktop.profile.joinedMonths.feb",
        "desktop.profile.joinedMonths.mar",
        "desktop.profile.joinedMonths.apr",
        "desktop.profile.joinedMonths.may",
        "desktop.profile.joinedMonths.jun",
        "desktop.profile.joinedMonths.jul",
        "desktop.profile.joinedMonths.aug",
        "desktop.profile.joinedMonths.sep",
        "desktop.profile.joinedMonths.oct",
        "desktop.profile.joinedMonths.nov",
        "desktop.profile.joinedMonths.dec",
    ][usize::from(date.month - 1)];
    language.tr_with(
        "desktop.profile.joinedDate",
        &[
            ("year", &date.year.to_string()),
            ("month", language.tr(month_key)),
            ("day", &date.day.to_string()),
        ],
    )
}

#[cfg(test)]
mod date_tests {
    use super::*;

    /// Использует подпись месяца и порядок частей даты из выбранного каталога.
    #[test]
    fn profile_joined_date_uses_locale_template() {
        let date = "2025-05-01";
        assert_eq!(
            profile_joined_date(date, Language::Ru).as_deref(),
            Some("1 мая 2025 г.")
        );
        assert_eq!(
            profile_joined_date(date, Language::En).as_deref(),
            Some("May 1, 2025")
        );
        assert_eq!(profile_joined_date("2025-13-01", Language::En), None);
    }
}

/// Размер аватара в пределах рабочего среза.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AvatarSize {
    Small, // 32 логических пикселя.
    #[default]
    Medium, // 40 логических пикселей.
}

/// Состояние присутствия, заданное экраном.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AvatarPresence {
    #[default]
    Hidden, // Индикатор не нужен.
    Online, // Пользователь в сети.
    Away,   // Пользователь отошёл.
    Offline, // Пользователь вне сети.
}

/// Аватар из готового изображения, инициалов или заглушки.
pub struct Avatar {
    pub initials: SharedString, // Инициалы, рассчитанные экраном. Пустые показывают заглушку.
    pub image: Option<ImageSource>, // Квадратное изображение, подготовленное для круга аватара.
    pub size: AvatarSize,       // Размер слота в окне.
    pub presence: AvatarPresence, // Видимость и цвет индикатора.
}

impl Avatar {
    /// Создаёт аватар среднего размера по инициалам.
    pub fn from_initials(initials: impl Into<SharedString>) -> Self {
        Self {
            initials: initials.into(),
            image: None,
            size: AvatarSize::Medium,
            presence: AvatarPresence::Hidden,
        }
    }

    /// Рисует фиксированный круг. При загрузке изображения остаются инициалы.
    pub fn render(self, palette: Palette) -> Div {
        self.render_with_background(palette, None)
    }

    /// Рисует тот же аватар с цветом разговора под инициалами.
    pub(in crate::gpui_client) fn render_with_background(
        self,
        palette: Palette,
        background: Option<Hsla>,
    ) -> Div {
        let diameter = match self.size {
            AvatarSize::Small => 32.,
            AvatarSize::Medium => 40.,
        };
        self.render_at_with_background(palette, px(diameter), background)
    }

    /// Рисует аватар указанного размера и цвета для карточки профиля или канала.
    pub(in crate::gpui_client) fn render_at(
        self,
        palette: Palette,
        size: gpui::Pixels,
        background: gpui::Hsla,
    ) -> Div {
        self.render_at_with_background(palette, size, Some(background))
    }

    /// Сохраняет стандартную палитру или подбирает контраст для заданного цвета.
    pub(super) fn render_at_with_background(
        self,
        palette: Palette,
        size: gpui::Pixels,
        background: Option<Hsla>,
    ) -> Div {
        let (foreground, placeholder) = background.map_or_else(
            || (palette.text, palette.text_secondary),
            |color| {
                let foreground = avatar_foreground(color);
                (foreground, foreground)
            },
        );
        let presence = match self.presence {
            AvatarPresence::Hidden => None,
            AvatarPresence::Online => Some(palette.presence_online),
            AvatarPresence::Away => Some(palette.presence_away),
            AvatarPresence::Offline => Some(palette.text_muted),
        };
        let initials: String = self.initials.graphemes(true).take(2).collect();
        let avatar = div()
            .relative()
            .size(size)
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .rounded_full()
            .overflow_hidden()
            .bg(background.unwrap_or(palette.background))
            .when(initials.trim().is_empty(), |avatar| {
                avatar.child(Icon::Account.element(px(20.), placeholder))
            })
            .when(!initials.trim().is_empty(), |avatar| {
                avatar.child(
                    label(
                        palette,
                        if size >= px(48.) {
                            TextRole::ProfileHeading
                        } else {
                            TextRole::Initials
                        },
                        initials,
                    )
                    .text_color(foreground),
                )
            })
            .children(self.image.map(|image| {
                // В GPUI границу картинки скругляет сама картинка.
                img(image)
                    .absolute()
                    .inset_0()
                    .size_full()
                    .rounded_full()
                    .object_fit(ObjectFit::Cover)
            }))
            // Обводка поверх фотографии совпадает с контуром аватара без фото.
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .rounded_full()
                    .border(CONTROL_BORDER)
                    .border_color(palette.border),
            );
        div()
            .relative()
            .size(size)
            .flex_none()
            .debug_selector(|| "avatar".into())
            .child(avatar)
            .children(presence.map(|color| {
                // Центр метки лежит на окружности под углом 45°, как в Electron.
                let inset = size * 0.1465 - px(6.);
                div()
                    .absolute()
                    .right(inset)
                    .bottom(inset)
                    .size(px(12.))
                    .rounded_full()
                    .border(px(2.))
                    .border_color(palette.background)
                    .bg(color)
            }))
    }
}

/// Выбирает чёрный или белый цвет инициалов по контрасту с цветным фоном.
fn avatar_foreground(background: Hsla) -> Hsla {
    let luminance = super::theme::relative_luminance(background);
    let contrast_with_black = (luminance + 0.05) / 0.05;
    let contrast_with_white = 1.05 / (luminance + 0.05);
    if contrast_with_black >= contrast_with_white {
        Hsla::black()
    } else {
        Hsla::white()
    }
}

/// Оформление счётчика непрочитанного.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BadgeTone {
    Accent, // Обычный счётчик непрочитанного.
    Muted,  // Приглушённый разговор.
}

/// Рисует число непрочитанных или возвращает None для нуля.
pub fn unread_badge(palette: Palette, count: u64, tone: BadgeTone) -> Option<Div> {
    if count == 0 {
        return None;
    }
    let text = if count > 1_000 {
        format!("{}k+", count / 1_000)
    } else {
        count.to_string()
    };
    Some(badge(palette, text.into(), tone).debug_selector(|| "unread-badge".into()))
}

/// Рисует бейдж упоминания.
pub fn mention_badge(palette: Palette) -> Div {
    badge(palette, "@".into(), BadgeTone::Accent).debug_selector(|| "mention-badge".into())
}

/// Размещает текст внутри бейджа фиксированной высоты.
fn badge(palette: Palette, text: SharedString, tone: BadgeTone) -> Div {
    let (background, foreground) = match tone {
        BadgeTone::Accent => (palette.unread, palette.badge_text),
        BadgeTone::Muted => (palette.unread_muted, palette.muted_badge_text),
    };
    div()
        .h(px(20.))
        .min_w(px(20.))
        .px(px(6.))
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .rounded_full()
        .bg(background)
        .child(label(palette, TextRole::Badge, text).text_color(foreground))
}

/// Готовые сведения для превью последнего сообщения.
#[derive(Clone)]
pub struct ChatRowPreview {
    pub sender: Option<SharedString>, // Автор превью, если он нужен рядом с текстом.
    pub text: SharedString,           // Уже подготовленный короткий текст сообщения.
}

/// Строка чата с отдельной колонкой для времени и счётчиков.
pub struct ChatRow {
    pub id: ElementId,                    // Стабильный ключ строки, заданный экраном.
    pub avatar: Avatar,                   // Аватар или заглушка для этой строки.
    pub title: SharedString,              // Имя собеседника или название канала.
    pub preview: Option<ChatRowPreview>,  // Готовое превью или пустая вторая строка.
    pub time: Option<SharedString>,       // Время в формате, выбранном экраном.
    pub unread: Option<(u64, BadgeTone)>, // Число и стиль счётчика.
    pub mention: bool,                    // Показывать ли бейдж упоминания.
    pub selected: bool,                   // Открыт ли разговор в данный момент.
    pub disabled: bool,                   // Разрешил ли экран открывать этот разговор.
}

impl ChatRow {
    /// Рисует строку с сокращаемым текстом и сохраняет место для метаданных справа.
    pub fn render(
        self,
        palette: Palette,
        focus: Option<&FocusHandle>,
        on_activate: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Stateful<Div> {
        self.render_with_action(palette, focus, None, on_activate)
    }

    /// Рисует строку канала с дополнительным индикатором раскрытия справа.
    pub fn render_with_action(
        self,
        palette: Palette,
        focus: Option<&FocusHandle>,
        action: Option<Div>,
        on_activate: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Stateful<Div> {
        self.render_with_caption(palette, focus, action, None, on_activate)
    }

    /// Использует готовый элемент подписи в той же раскладке строки и её метаданных.
    pub(in crate::gpui_client) fn render_with_caption(
        self,
        palette: Palette,
        focus: Option<&FocusHandle>,
        action: Option<Div>,
        caption: Option<AnyElement>,
        on_activate: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Stateful<Div> {
        let row = div()
            .id(self.id)
            .w_full()
            .min_w_0()
            .min_h(px(60.))
            .px(px(9.))
            .py(px(9.))
            .flex()
            .gap(px(12.))
            .rounded(PANEL_RADIUS)
            .border(CONTROL_BORDER)
            .border_color(gpui::transparent_black())
            .bg(if self.selected {
                palette.card_selected
            } else {
                palette.card
            })
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_start()
                    .child(self.avatar.render(palette)),
            )
            .child(Self::text_with_caption(palette, self.title, self.preview, caption))
            .children(Self::metadata(
                palette,
                self.time,
                self.unread,
                self.mention,
                action,
            ));
        if self.disabled {
            row
        } else {
            activate(
                row.hover(|style| style.bg(palette.card_hover))
                    .active(|style| style.bg(palette.card_selected)),
                focus,
                palette,
                on_activate,
            )
        }
    }

    /// Собирает сокращаемое название и превью для чата или вложенной темы.
    pub fn text(palette: Palette, title: SharedString, preview: Option<ChatRowPreview>) -> Div {
        Self::text_with_caption(palette, title, preview, None)
    }

    /// Объединяет название с готовой подписью или стандартной строкой.
    fn text_with_caption(
        palette: Palette, title: SharedString, preview: Option<ChatRowPreview>,
        caption: Option<AnyElement>,
    ) -> Div {
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .overflow_hidden()
            .child(
                ellipsized_label(palette, TextRole::Label, one_line(&title))
                    .debug_selector(|| "chat-row-title".into()),
            )
            .children(caption.or_else(|| preview.map(|preview| {
                let text = match preview.sender {
                    Some(sender) => format!("{sender}: {}", preview.text),
                    None => preview.text.to_string(),
                };
                ellipsized_label(palette, TextRole::Caption, one_line(&text))
                    .debug_selector(|| "chat-row-preview".into())
                    .into_any_element()
            })))
    }

    /// Размещает индикаторы над временем; ширина колонки защищена от длинных имён.
    pub fn metadata(
        palette: Palette,
        time: Option<SharedString>,
        unread: Option<(u64, BadgeTone)>,
        mention: bool,
        action: Option<Div>,
    ) -> Option<Div> {
        let badge = unread.and_then(|(count, tone)| unread_badge(palette, count, tone));
        if time.is_none() && badge.is_none() && !mention && action.is_none() {
            return None;
        }
        Some(
            div()
                .flex()
                .flex_col()
                .flex_none()
                .min_w(px(40.))
                .items_end()
                .justify_between()
                .debug_selector(|| "chat-row-metadata".into())
                .child(
                    div()
                        .flex()
                        .min_h(px(20.))
                        .items_center()
                        .gap(px(4.))
                        .children(mention.then(|| mention_badge(palette)))
                        .children(badge)
                        .children(action),
                )
                .children(time.map(|time| {
                    label(palette, TextRole::Caption, time)
                        .whitespace_nowrap()
                        .debug_selector(|| "chat-row-time".into())
                })),
        )
    }
}

/// Заменяет переводы строк пробелами, чтобы превью не увеличивало карточку.
fn one_line(text: &str) -> String {
    text.replace(['\n', '\r'], " ")
}

/// Значок статуса исходящего сообщения, заданный экраном разговора.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeliveryMark {
    Pending,   // Запись отправляется или ещё не дошла до сети.
    Sent,      // Сервер принял сообщение.
    Delivered, // Сообщение дошло до получателя.
    Read,      // Получатель прочитал сообщение.
    Failed,    // Ошибка доставки.
    Unknown,   // Результат отправки пока не удалось подтвердить.
}

/// Пузырь одного сообщения. Экран передаёт роли, время и статус отправки.
pub struct MessageBubble {
    pub author: Option<SharedString>,   // Имя автора в начале группы.
    pub body: SharedString,             // Текст, подготовленный экраном.
    pub time: Option<SharedString>,     // Готовая подпись времени.
    pub own: bool,                      // Исходящее сообщение выровнено вправо.
    pub delivery: Option<DeliveryMark>, // Статус исходящего сообщения.
}

/// Определяет раскладку тела бабла или подписи под медиа.
#[allow(dead_code)]
pub(in crate::gpui_client) enum BubbleContent {
    Inline(AnyElement),     // Текст может перейти на безопасную строку времени.
    InlineTail(AnyElement), // Текст содержит невидимый резерв после последнего символа.
    Blocks {
        content: AnyElement,        // Самостоятельные карточки занимают всю ширину тела.
        reactions: Vec<AnyElement>, // Кнопки и время укладываются одним потоком.
    },
}

impl BubbleContent {
    /// Расставляет содержимое и время в выбранной раскладке.
    fn render(self, metadata: Option<Div>) -> Div {
        match self {
            Self::Inline(content) => message_body(content, metadata, false),
            Self::InlineTail(content) => message_body(content, metadata, true),
            Self::Blocks { content, reactions } => message_blocks(content, reactions, metadata),
        }
    }
}

/// Невидимый хвост текста, который оставляет место для времени только в последней строке.
pub(in crate::gpui_client) struct BubbleTailReserve {
    suffix: SharedString, // Невидимые символы с шириной не меньше метаданных.
}

impl BubbleTailReserve {
    /// Возвращает строку, добавляемую после последнего символа содержимого.
    pub(in crate::gpui_client) fn suffix(&self) -> &str {
        &self.suffix
    }

    /// Добавляет резерв к отображаемому тексту, не меняя исходное тело.
    pub(in crate::gpui_client) fn append(&self, text: &str) -> SharedString {
        let mut visible =
            String::with_capacity(text.len() + self.suffix.len() + BUBBLE_TAIL_BREAK.len());
        visible.push_str(text);
        visible.push_str(BUBBLE_TAIL_BREAK);
        visible.push_str(&self.suffix);
        visible.into()
    }
}

impl MessageBubble {
    /// Рисует отдельный пузырь и переносит длинное сообщение по доступной ширине.
    pub fn render(self, palette: Palette) -> Div {
        self.render_with_content(palette, None, true, None, None, false)
    }

    /// Укладывает подготовленное тело и прикрепляет наложение к общему пузырю.
    pub(in crate::gpui_client) fn render_with_content(
        self,
        palette: Palette,
        content: Option<BubbleContent>,
        ends_group: bool,
        accessory: Option<AnyElement>,
        overlay: Option<AnyElement>,
        full_width: bool,
    ) -> Div {
        let metadata = self.metadata(palette, false);
        let body = match content {
            Some(content) => content.render(metadata),
            None => message_body(self.plain_body(palette), metadata, false),
        };
        self.render_frame(palette, body, ends_group, accessory, overlay, full_width)
    }

    /// Расставляет скругления только на углах, где медиа совпадает с краем пузыря.
    pub(in crate::gpui_client) fn media_corners(
        &self,
        ends_group: bool,
        has_footer: bool,
    ) -> Corners<Pixels> {
        let mut corners = message_bubble_radii(self.own, ends_group);
        if self.author.is_some() {
            corners.top_left = px(0.);
            corners.top_right = px(0.);
        }
        if has_footer {
            corners.bottom_left = px(0.);
            corners.bottom_right = px(0.);
        }
        corners
    }

    /// Прижимает медиа к краям пузыря, а одиночное фото позволяет ужать бабл до рамки.
    pub(in crate::gpui_client) fn render_with_media(
        self,
        palette: Palette,
        media: Div,
        caption: Option<BubbleContent>,
        shrink_to_media: bool,
        ends_group: bool,
        accessory: Option<AnyElement>,
        overlay: Option<AnyElement>,
    ) -> Div {
        let time_on_media = caption.is_none();
        let metadata = self.metadata(palette, time_on_media);
        let (media_metadata, caption_metadata) = if time_on_media {
            (metadata, None)
        } else {
            (None, metadata)
        };
        let bubble = self
            .surface(palette, ends_group)
            .when(!shrink_to_media, |bubble| bubble.w_full())
            .children(
                self.author_label(palette)
                    .map(|author| author.px(px(12.)).pt(px(8.)).pb_1()),
            )
            .child(media.relative().children(media_metadata.map(|time| {
                time.absolute()
                    .right_2()
                    .bottom_2()
                    .px_2()
                    .py_0p5()
                    .rounded_full()
                    .bg(rgba(0x000000a6))
                    .debug_selector(|| "history-media-time".into())
            })))
            .children(caption.map(|caption| {
                let has_tail = matches!(caption, BubbleContent::InlineTail(_));
                let caption = match caption {
                    // Интерактивный текст не задаёт ширину сам, поэтому отдаём ему свободное место.
                    BubbleContent::Inline(content) => BubbleContent::Inline(
                        div().flex_1().min_w_0().child(content).into_any_element(),
                    ),
                    BubbleContent::InlineTail(content) => BubbleContent::InlineTail(
                        div().flex_1().min_w_0().child(content).into_any_element(),
                    ),
                    blocks => blocks,
                };
                let rendered = caption.render(caption_metadata);
                if has_tail {
                    div().px(px(12.)).py(px(8.)).child(rendered)
                } else {
                    rendered.px(px(12.)).py(px(8.))
                }
            }))
            .children(accessory.map(|accessory| {
                div()
                    .px(px(12.))
                    .pb(px(8.))
                    .when(time_on_media, |footer| footer.pt(px(8.)))
                    .child(accessory)
            }))
            .children(overlay)
            .debug_selector(|| "history-media-bubble".into());
        self.align(bubble)
    }

    /// Готовит одинаковый текстовый элемент для обычного и раскрытого пузыря.
    fn plain_body(&self, palette: Palette) -> Div {
        label(palette, TextRole::Body, self.body.clone())
            .min_w_0()
            .max_w_full()
    }

    /// Собирает фон, автора и выбранную раскладку тела в общий пузырь.
    fn render_frame(
        self,
        palette: Palette,
        body: Div,
        ends_group: bool,
        accessory: Option<AnyElement>,
        overlay: Option<AnyElement>,
        full_width: bool,
    ) -> Div {
        let bubble = self
            .surface(palette, ends_group)
            .when(full_width, |bubble| bubble.w_full())
            .px(px(12.))
            .py(px(8.))
            .children(self.author_label(palette))
            .child(body)
            .children(accessory)
            .children(overlay);
        self.align(bubble)
    }

    /// Показывает время и статус с цветом фона текста или кадра.
    pub(in crate::gpui_client) fn metadata(&self, palette: Palette, on_media: bool) -> Option<Div> {
        let color = if on_media {
            rgba(0xffffffff).into()
        } else {
            palette.message_time
        };
        let status = self.delivery.filter(|_| self.own).map(|state| match state {
            DeliveryMark::Pending | DeliveryMark::Unknown => Icon::Pending.element(px(16.), color),
            DeliveryMark::Sent => Icon::Check.element(px(16.), color),
            DeliveryMark::Delivered => Icon::DoneAll.element(px(16.), color),
            DeliveryMark::Read => {
                Icon::DoneAll.element(px(16.), if on_media { color } else { palette.accent })
            }
            DeliveryMark::Failed => {
                Icon::Error.element(px(16.), if on_media { color } else { palette.danger })
            }
        });
        (self.time.is_some() || status.is_some()).then(|| {
            div()
                .ml_auto()
                .flex()
                .flex_none()
                .items_center()
                .gap(px(4.))
                .children(
                    self.time
                        .clone()
                        .map(|time| label(palette, TextRole::Caption, time).text_color(color)),
                )
                .children(status)
                .debug_selector(|| "message-meta".into())
        })
    }

    /// Подбирает невидимый хвост по фактической ширине времени и статуса отправки.
    pub(in crate::gpui_client) fn tail_reserve(
        &self,
        palette: Palette,
        window: &Window,
    ) -> Option<BubbleTailReserve> {
        let time_width = self
            .time
            .as_ref()
            .map(|time| label_width(palette, TextRole::Caption, time, window));
        let status = self.own.then_some(self.delivery).flatten();
        if time_width.is_none() && status.is_none() {
            return None;
        }
        let reserve_width = time_width.unwrap_or(px(0.))
            + status.map_or(px(0.), |_| px(16.))
            + if time_width.is_some() && status.is_some() {
                px(4.)
            } else {
                px(0.)
            };
        // Буквы задают измеримую ширину и не схлопываются как пробелы в конце строки.
        let reserve_glyph = SharedString::from("M");
        let glyph_width = label_width(palette, TextRole::Body, &reserve_glyph, window);
        if glyph_width <= px(0.) {
            return None;
        }
        let mut count = (f32::from(reserve_width / glyph_width).ceil() as usize).max(1);
        let mut suffix = reserve_glyph.repeat(count);
        while label_width(palette, TextRole::Body, &suffix.clone().into(), window) < reserve_width {
            count += 1;
            suffix = reserve_glyph.repeat(count);
        }
        Some(BubbleTailReserve {
            suffix: suffix.into(),
        })
    }

    /// Оставляет резерв только когда время останется внутри пузыря.
    pub(in crate::gpui_client) fn tail_reserve_without_reactions(
        &self,
        palette: Palette,
        window: &Window,
        has_reaction_chips: bool,
    ) -> Option<BubbleTailReserve> {
        (!has_reaction_chips)
            .then(|| self.tail_reserve(palette, window))
            .flatten()
    }

    /// Ограничивает ширину сообщения и подставляет цвет отправителя.
    fn surface(&self, palette: Palette, ends_group: bool) -> Div {
        message_bubble_surface(self.own, ends_group)
            .relative()
            .max_w(relative(0.88))
            .min_w_0()
            .debug_selector(|| "message-bubble-surface".into())
            .bg(if self.own {
                palette.message_own
            } else {
                palette.message
            })
    }

    /// Показывает автора в начале группы сообщений.
    pub(super) fn author_label(&self, palette: Palette) -> Option<Div> {
        self.author.as_ref().map(|author| {
            label(palette, TextRole::Label, one_line(author))
                .text_color(palette.accent)
                .whitespace_nowrap()
                .overflow_hidden()
        })
    }

    /// Оставляет входящие слева и исходящие справа в ширине той же строки.
    pub(super) fn align(&self, bubble: impl IntoElement) -> Div {
        div()
            .w_full()
            .min_w_0()
            .flex()
            .when(self.own, |row| row.justify_end())
            .child(bubble)
    }
}

/// Размещает подпись и время в одной строке, когда для них достаточно места.
fn message_body(content: impl IntoElement, metadata: Option<Div>, has_tail: bool) -> Div {
    if has_tail {
        return div()
            .relative()
            .min_w_0()
            .child(content)
            .children(metadata.map(|time| time.absolute().right_0().bottom_0()))
            .debug_selector(|| "message-body".into());
    }
    div()
        .flex()
        .flex_wrap()
        .items_end()
        .gap_x(px(8.))
        .gap_y(px(2.))
        .child(content)
        .children(metadata)
}

/// Оставляет карточкам ширину тела. Время может перенестись под реакции в узком пузыре.
fn message_blocks(content: AnyElement, reactions: Vec<AnyElement>, metadata: Option<Div>) -> Div {
    let footer = (!reactions.is_empty() || metadata.is_some()).then(|| {
        div()
            .flex()
            .flex_wrap()
            .min_w_0()
            .items_end()
            .gap_1()
            .mt_1()
            .children(reactions)
            .children(metadata.map(|time| {
                time.pl_1()
                    .debug_selector(|| "message-block-footer-time".into())
            }))
            .debug_selector(|| "message-block-footer".into())
    });
    div()
        .flex()
        .flex_col()
        .min_w_0()
        .child(content)
        .children(footer)
}

/// Уменьшает радиус со стороны автора, чтобы соседние пузыри выглядели как группа.
fn message_bubble_surface(own: bool, ends_group: bool) -> Div {
    let corners = message_bubble_radii(own, ends_group);
    div()
        .rounded_tl(corners.top_left)
        .rounded_tr(corners.top_right)
        .rounded_bl(corners.bottom_left)
        .rounded_br(corners.bottom_right)
}

/// Возвращает общую форму для фона пузыря и крайних кадров альбома.
fn message_bubble_radii(own: bool, ends_group: bool) -> Corners<Pixels> {
    let mut radii = Corners::all(MESSAGE_BUBBLE_RADIUS);
    let (top, bottom) = if own {
        (&mut radii.top_right, &mut radii.bottom_right)
    } else {
        (&mut radii.top_left, &mut radii.bottom_left)
    };
    if ends_group {
        *bottom = PANEL_RADIUS;
    } else {
        *top = SURFACE_RADIUS;
        *bottom = SURFACE_RADIUS;
    }
    radii
}

#[cfg(test)]
mod radius_tests {
    use super::*;

    #[cfg(feature = "gpui-test")]
    /// На широком окне пузырь использует 88% строки без потолка в 680 px.
    #[gpui::test]
    fn message_bubble_width_scales_on_ultrawide_windows(cx: &mut gpui::TestAppContext) {
        /// Рисует длинный текст, чтобы ширину задавал предел пузыря.
        struct WideMessage {
            own: bool,          // Направление проверяемого сообщения.
            body: SharedString, // Текст, задающий естественную ширину пузыря.
            media: bool,        // Включает пример медиа вместо текстового содержимого.
        }
        impl gpui::Render for WideMessage {
            /// Собирает сообщение в ширине окна для измерения общего компонента.
            fn render(
                &mut self,
                window: &mut gpui::Window,
                cx: &mut gpui::Context<Self>,
            ) -> impl IntoElement {
                let palette = Palette::for_window(window, cx);
                let bubble = MessageBubble {
                    author: None,
                    body: self.body.clone(),
                    time: Some("10:32".into()),
                    own: self.own,
                    delivery: Some(DeliveryMark::Read),
                };
                let rendered = if self.media {
                    bubble.render_with_media(
                        palette,
                        div()
                            .w(px(2400.))
                            .max_w_full()
                            .h(px(100.))
                            .debug_selector(|| "wide-message-media".into()),
                        None,
                        false,
                        true,
                        None,
                        None,
                    )
                } else {
                    bubble.render(palette)
                };
                div()
                    .size_full()
                    .child(rendered.debug_selector(|| "wide-message-row".into()))
            }
        }

        let (view, ui) = cx.add_window_view(|_, _| WideMessage {
            own: true,
            body: "Широкое сообщение. ".repeat(120).into(),
            media: false,
        });
        ui.simulate_resize(gpui::size(px(2400.), px(900.)));
        ui.run_until_parked();
        for own in [true, false] {
            ui.update(|_, app| {
                view.update(app, |message, cx| {
                    message.own = own;
                    cx.notify();
                });
            });
            ui.run_until_parked();
            let row = ui.debug_bounds("wide-message-row").unwrap();
            let bubble = ui.debug_bounds("message-bubble-surface").unwrap();
            assert!(bubble.size.width > px(680.), "направление own={own}");
            assert!(
                bubble.size.width <= row.size.width * 0.88 + px(1.),
                "направление own={own}: bubble={:?}, row={:?}",
                bubble.size,
                row.size
            );
        }

        ui.update(|_, app| {
            view.update(app, |message, cx| {
                message.body = "Привет".into();
                cx.notify();
            });
        });
        ui.run_until_parked();
        let short = ui.debug_bounds("message-bubble-surface").unwrap();
        assert!(short.size.width < px(680.), "короткое сообщение: {short:?}");

        ui.update(|_, app| {
            view.update(app, |message, cx| {
                message.media = true;
                cx.notify();
            });
        });
        ui.run_until_parked();
        let row = ui.debug_bounds("wide-message-row").unwrap();
        let bubble = ui.debug_bounds("message-bubble-surface").unwrap();
        let media = ui.debug_bounds("wide-message-media").unwrap();
        assert!(bubble.size.width <= row.size.width * 0.88 + px(1.));
        assert!(media.size.width <= bubble.size.width + px(1.));

        ui.update(|_, app| {
            view.update(app, |message, cx| {
                message.media = false;
                message.body = "Широкое сообщение. ".repeat(120).into();
                cx.notify();
            });
        });
        ui.simulate_resize(gpui::size(px(760.), px(900.)));
        ui.run_until_parked();
        let row = ui.debug_bounds("wide-message-row").unwrap();
        let bubble = ui.debug_bounds("message-bubble-surface").unwrap();
        assert!(bubble.size.width <= row.size.width * 0.88 + px(1.));
    }

    #[cfg(feature = "gpui-test")]
    /// Время занимает зарезервированное место внизу последней строки текста.
    #[gpui::test]
    fn message_tail_reserve_keeps_metadata_on_the_last_text_line(cx: &mut gpui::TestAppContext) {
        /// Показывает длинный текст с невидимым резервом хвоста.
        struct TailMessage {
            layout: Option<gpui::TextLayout>, // Разметка текста с хвостом из последнего кадра.
            tail_start: usize,                // Начало невидимого резерва в отображаемом тексте.
            tail_end: usize,                  // Последний байтовый индекс резерва.
            body_end: usize,                  // Конец исходного текста до резерва.
        }

        impl gpui::Render for TailMessage {
            /// Собирает тот же текстовый пузырь, который использует история.
            fn render(
                &mut self,
                window: &mut gpui::Window,
                cx: &mut gpui::Context<Self>,
            ) -> impl IntoElement {
                let palette = Palette::for_window(window, cx);
                let message = MessageBubble {
                    author: None,
                    body: "Текст ".repeat(19).trim_end().to_owned().into(),
                    time: Some("10:32".into()),
                    own: true,
                    delivery: Some(DeliveryMark::Read),
                };
                let reserve = message.tail_reserve(palette, window).unwrap();
                let body = "Текст ".repeat(19).trim_end().to_owned();
                let visible = reserve.append(&body);
                self.tail_start = body.len() + BUBBLE_TAIL_BREAK.len();
                self.tail_end = visible.len() - 1;
                self.body_end = body.len();
                let styled = gpui::StyledText::new(visible.clone()).with_highlights([(
                    body.len() + BUBBLE_TAIL_BREAK.len()..visible.len(),
                    gpui::HighlightStyle {
                        color: Some(palette.text.opacity(0.)),
                        ..Default::default()
                    },
                )]);
                self.layout = Some(styled.layout().clone());
                let content = super::super::text_role(palette, TextRole::Body)
                    .min_w_0()
                    .max_w_full()
                    .child(styled)
                    .debug_selector(|| "message-tail-text".into())
                    .into_any_element();
                div().size_full().child(message.render_with_content(
                    palette,
                    Some(BubbleContent::InlineTail(content)),
                    true,
                    None,
                    None,
                    false,
                ))
            }
        }

        let (view, ui) = cx.add_window_view(|_, _| TailMessage {
            layout: None,
            tail_start: 0,
            tail_end: 0,
            body_end: 0,
        });
        ui.simulate_resize(gpui::size(px(250.), px(640.)));
        ui.run_until_parked();
        let body = ui.debug_bounds("message-tail-text").unwrap();
        let meta = ui.debug_bounds("message-meta").unwrap();
        let bubble = ui.debug_bounds("message-bubble-surface").unwrap();
        ui.read(|app| {
            let layout = view.read(app).layout.as_ref().unwrap();
            assert!(layout.wrapped_text().lines().count() > 1);
            assert!(layout.wrapped_text().ends_with('M'));
            let tail = layout
                .position_for_index(view.read(app).tail_start)
                .unwrap();
            let tail_end = layout
                .position_for_index(view.read(app).tail_end)
                .unwrap();
            let last_body = layout
                .position_for_index(view.read(app).body_end - 1)
                .unwrap();
            assert!(
                tail.y > last_body.y,
                "хвост должен перенестись: {tail:?}, {last_body:?}, body={body:?}, bubble={bubble:?}"
            );
            assert!(
                tail.y - last_body.y <= layout.line_height() + px(1.),
                "хвост должен занять ровно следующую строку: {tail:?}, {last_body:?}"
            );
            assert_eq!(tail_end.y, tail.y, "резерв должен занимать одну строку");
        });
        assert_eq!(meta.bottom(), body.bottom());
        assert_eq!(meta.right(), body.right());
        assert!(meta.top() >= body.bottom() - px(20.));
    }

    #[cfg(feature = "gpui-test")]
    /// Реальная полоса реакций переводит время в footer без скрытого резерва.
    #[gpui::test]
    fn narrow_reaction_footer_does_not_keep_tail_reserve(cx: &mut gpui::TestAppContext) {
        struct ReactionMessage;
        impl gpui::Render for ReactionMessage {
            /// Рисует короткое сообщение и чип реакции в узком окне.
            fn render(
                &mut self,
                window: &mut gpui::Window,
                cx: &mut gpui::Context<Self>,
            ) -> impl IntoElement {
                let palette = Palette::for_window(window, cx);
                let message = MessageBubble {
                    author: None,
                    body: "Коротко".into(),
                    time: Some("10:32".into()),
                    own: false,
                    delivery: None,
                };
                let chip = div()
                    .h(px(22.))
                    .px(px(6.))
                    .child("👍 1")
                    .debug_selector(|| "test-reaction-chip".into())
                    .into_any_element();
                let reactions = vec![chip];
                let reserve =
                    message.tail_reserve_without_reactions(palette, window, !reactions.is_empty());
                assert!(reserve.is_none());
                let content = super::super::text_role(palette, TextRole::Body)
                    .child("Коротко")
                    .debug_selector(|| "reaction-footer-body".into())
                    .into_any_element();
                message.render_with_content(
                    palette,
                    Some(BubbleContent::Blocks { content, reactions }),
                    true,
                    None,
                    None,
                    false,
                )
            }
        }

        let (_, ui) = cx.add_window_view(|_, _| ReactionMessage);
        ui.simulate_resize(gpui::size(px(350.), px(400.)));
        ui.run_until_parked();
        let body = ui.debug_bounds("reaction-footer-body").unwrap();
        let metadata = ui.debug_bounds("message-block-footer-time").unwrap();
        let chip = ui.debug_bounds("test-reaction-chip").unwrap();
        assert!(metadata.top() >= body.bottom());
        assert!(chip.top() >= body.bottom());
    }

    #[cfg(feature = "gpui-test")]
    /// Короткая подпись под широким медиа остаётся в одной строке рядом со временем.
    #[gpui::test]
    fn media_caption_keeps_available_width(cx: &mut gpui::TestAppContext) {
        /// Показывает пузырь с медиа и короткой подписью.
        struct CaptionExample;
        impl gpui::Render for CaptionExample {
            /// Собирает подпись тем же способом, что и история.
            fn render(
                &mut self,
                window: &mut gpui::Window,
                cx: &mut gpui::Context<Self>,
            ) -> impl IntoElement {
                let palette = Palette::for_window(window, cx);
                let message = MessageBubble {
                    author: None,
                    body: "".into(),
                    time: Some("10:32".into()),
                    own: false,
                    delivery: None,
                };
                let reserve = message.tail_reserve(palette, window).unwrap();
                let visible = reserve.append("Отец");
                let caption_text = gpui::StyledText::new(visible.clone()).with_highlights([(
                    "Отец".len() + BUBBLE_TAIL_BREAK.len()..visible.len(),
                    gpui::HighlightStyle {
                        color: Some(palette.text.opacity(0.)),
                        ..Default::default()
                    },
                )]);
                let caption = div()
                    .flex()
                    .flex_col()
                    .min_w_0()
                    .max_w_full()
                    .gap_1()
                    .child(
                        super::super::text_role(palette, TextRole::Body)
                            .min_w_0()
                            .max_w_full()
                            .child(caption_text)
                            .debug_selector(|| "caption-text".into()),
                    );
                message.render_with_media(
                    palette,
                    div().w(px(400.)).h(px(100.)),
                    Some(BubbleContent::InlineTail(caption.into_any_element())),
                    false,
                    true,
                    None,
                    None,
                )
            }
        }
        let (_, ui) = cx.add_window_view(|_, _| CaptionExample);
        let text = ui.debug_bounds("caption-text").unwrap();
        let body = ui.debug_bounds("message-body").unwrap();
        let meta = ui.debug_bounds("message-meta").unwrap();
        assert!(
            text.size.width > px(30.) && text.size.height < px(30.),
            "Подпись перенеслась по буквам: {text:?}"
        );
        assert_eq!(meta.bottom(), body.bottom());
        assert_eq!(meta.right(), body.right());
    }

    /// Проверяет четыре формы пузыря по направлению и месту сообщения в группе.
    #[test]
    fn message_bubble_rounds_corners_by_author_and_group_position() {
        for (own, ends_group, expected) in [
            (true, true, [18., 18., 6., 18.]),
            (false, true, [18., 18., 18., 6.]),
            (true, false, [18., 10., 10., 18.]),
            (false, false, [10., 18., 18., 10.]),
        ] {
            let mut bubble = message_bubble_surface(own, ends_group);
            let corners = &bubble.style().corner_radii;
            assert_eq!(
                [
                    corners.top_left,
                    corners.top_right,
                    corners.bottom_right,
                    corners.bottom_left,
                ],
                expected.map(|radius| Some(px(radius).into())),
            );
        }
    }
}

#[cfg(test)]
mod contrast_tests {
    use gpui::rgb;

    use super::*;

    /// Цвет канала может совпасть со штатным текстом любой палитры.
    #[test]
    fn channel_initials_keep_contrast_on_server_colors() {
        for color in [0x1a2330, 0xf7fbff, 0x777777, 0x0000ff, 0xff0000, 0x00ff00] {
            let background = rgb(color).into();
            let a = super::super::theme::relative_luminance(background);
            let b = super::super::theme::relative_luminance(avatar_foreground(background));
            let ratio = (a.max(b) + 0.05) / (a.min(b) + 0.05);
            assert!(ratio >= 4.5, "#{color:06x}: {ratio:.2}:1");
        }
    }
}
