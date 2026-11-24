//! Компоненты чатов: аватары, счётчики, строки каталога и пузыри сообщений.

use gpui::{
    AnyElement, App, Div, ElementId, FocusHandle, Hsla, ImageSource, ObjectFit, SharedString,
    Stateful, Window, div, img, prelude::*, px,
};
use unicode_segmentation::UnicodeSegmentation;

use crate::gpui_client::preferences::Language;

use super::{
    Icon, Palette, TextRole,
    controls::activate,
    label,
    theme::{
        CONTROL_BORDER, MESSAGE_BUBBLE_RADIUS, PANEL_RADIUS, SURFACE_RADIUS, ellipsized_label,
    },
};

/// Показывает календарную дату из ISO-строки Workspace на языке профиля.
pub(in crate::gpui_client) fn profile_joined_date(value: &str, language: Language) -> Option<String> {
    let date = value.get(..10)?;
    let mut parts = date.split('-');
    let year = parts.next()?;
    let month = parts.next()?.parse::<usize>().ok()?.checked_sub(1)?;
    let day = parts.next()?.parse::<u8>().ok()?;
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
    ]
    .get(month)?;
    Some(language.tr_with(
        "desktop.profile.joinedDate",
        &[
            ("year", year),
            ("month", language.tr(month_key)),
            ("day", &day.to_string()),
        ],
    ))
}

#[cfg(test)]
mod date_tests {
    use super::*;

    /// Использует подпись месяца и порядок частей даты из выбранного каталога.
    #[test]
    fn profile_joined_date_uses_locale_template() {
        let date = "2025-05-01T04:05:06Z";
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
            .child(Self::text_with_caption(
                palette,
                self.title,
                self.preview,
                caption,
            ))
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
        palette: Palette,
        title: SharedString,
        preview: Option<ChatRowPreview>,
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
            .children(caption.or_else(|| {
                preview.map(|preview| {
                    let text = match preview.sender {
                        Some(sender) => format!("{sender}: {}", preview.text),
                        None => preview.text.to_string(),
                    };
                    ellipsized_label(palette, TextRole::Caption, one_line(&text))
                        .debug_selector(|| "chat-row-preview".into())
                        .into_any_element()
                })
            }))
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

impl MessageBubble {
    /// Рисует отдельный пузырь и переносит длинное сообщение по доступной ширине.
    pub fn render(self, palette: Palette) -> Div {
        self.render_in_group(palette, true)
    }

    /// Рисует текстовый пузырь с учётом положения сообщения в группе.
    pub(in crate::gpui_client) fn render_in_group(self, palette: Palette, ends_group: bool) -> Div {
        let body = self.plain_body(palette);
        self.render_with_body(palette, body, ends_group)
    }

    /// Рисует тот же пузырь с выделяемым текстом или блоками цитат.
    pub(in crate::gpui_client) fn render_with_body(
        self,
        palette: Palette,
        content: impl gpui::IntoElement,
        ends_group: bool,
    ) -> Div {
        self.render_body(palette, content, ends_group, None)
    }

    /// Показывает дополнительный блок под текстом и временем уже отрисованного сообщения.
    pub(in crate::gpui_client) fn render_with_accessory(
        self,
        palette: Palette,
        content: Option<gpui::AnyElement>,
        ends_group: bool,
        accessory: gpui::AnyElement,
    ) -> Div {
        let content = content.unwrap_or_else(|| self.plain_body(palette).into_any_element());
        self.render_body(palette, content, ends_group, Some(accessory))
    }

    /// Готовит одинаковый текстовый элемент для обычного и раскрытого пузыря.
    fn plain_body(&self, palette: Palette) -> Div {
        label(palette, TextRole::Body, self.body.clone())
            .min_w_0()
            .max_w_full()
    }

    /// Собирает общую разметку пузыря с дополнительным блоком под основным содержимым.
    fn render_body(
        self,
        palette: Palette,
        content: impl gpui::IntoElement,
        ends_group: bool,
        accessory: Option<gpui::AnyElement>,
    ) -> Div {
        let status = self.delivery.filter(|_| self.own).map(|state| match state {
            DeliveryMark::Pending | DeliveryMark::Unknown => {
                Icon::Pending.element(px(16.), palette.message_time)
            }
            DeliveryMark::Sent => Icon::Check.element(px(16.), palette.message_time),
            DeliveryMark::Delivered => Icon::DoneAll.element(px(16.), palette.message_time),
            DeliveryMark::Read => Icon::DoneAll.element(px(16.), palette.accent),
            DeliveryMark::Failed => Icon::Error.element(px(16.), palette.danger),
        });
        let body = div()
            .flex()
            .flex_wrap()
            .items_end()
            .gap_x(px(8.))
            .gap_y(px(2.))
            .child(content)
            .when(self.time.is_some() || status.is_some(), |body| {
                body.child(
                    div()
                        .ml_auto()
                        .flex()
                        .flex_none()
                        .items_center()
                        .gap(px(4.))
                        .children(self.time.map(|time| {
                            label(palette, TextRole::Caption, time).text_color(palette.message_time)
                        }))
                        .children(status),
                )
            });
        let bubble = message_bubble_surface(self.own, ends_group)
            .max_w(px(680.))
            .min_w_0()
            .px(px(12.))
            .py(px(8.))
            .bg(if self.own {
                palette.message_own
            } else {
                palette.message
            })
            .children(self.author.map(|author| {
                label(palette, TextRole::Label, one_line(&author))
                    .text_color(palette.accent)
                    .whitespace_nowrap()
                    .overflow_hidden()
            }))
            .child(body)
            .children(accessory);
        div()
            .w_full()
            .min_w_0()
            .flex()
            .when(self.own, |row| row.justify_end())
            .child(bubble)
    }
}

/// Уменьшает радиус со стороны автора, чтобы соседние пузыри выглядели как группа.
fn message_bubble_surface(own: bool, ends_group: bool) -> Div {
    let surface = div().rounded(MESSAGE_BUBBLE_RADIUS);
    match (own, ends_group) {
        (true, true) => surface.rounded_br(PANEL_RADIUS),
        (false, true) => surface.rounded_bl(PANEL_RADIUS),
        (true, false) => surface.rounded_r(SURFACE_RADIUS),
        (false, false) => surface.rounded_l(SURFACE_RADIUS),
    }
}

#[cfg(test)]
mod radius_tests {
    use super::*;

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
