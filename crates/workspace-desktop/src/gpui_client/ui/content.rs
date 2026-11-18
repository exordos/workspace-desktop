//! Компоненты чатов: аватары, счётчики, строки каталога и пузыри сообщений.

use gpui::{
    App, Div, ElementId, FocusHandle, ImageSource, ObjectFit, SharedString, Stateful, Window, div,
    img, prelude::*, px,
};
use unicode_segmentation::UnicodeSegmentation;

use super::{
    Icon, Palette, TextRole,
    controls::activate,
    label,
    theme::{CONTROL_BORDER, CONTROL_RADIUS, ellipsized_label},
};

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
    pub image: Option<ImageSource>, // Изображение, выбранное экраном.
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
        let diameter = match self.size {
            AvatarSize::Small => 32.,
            AvatarSize::Medium => 40.,
        };
        self.render_at(palette, px(diameter), palette.background)
    }

    /// Рисует аватар указанного размера и цвета для карточки профиля или канала.
    pub(in crate::gpui_client) fn render_at(
        self,
        palette: Palette,
        size: gpui::Pixels,
        background: gpui::Hsla,
    ) -> Div {
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
            .bg(background)
            .when(initials.trim().is_empty(), |avatar| {
                avatar.child(Icon::Account.element(px(20.), palette.text_secondary))
            })
            .when(!initials.trim().is_empty(), |avatar| {
                avatar.child(label(
                    palette,
                    if size >= px(48.) {
                        TextRole::ProfileHeading
                    } else {
                        TextRole::Initials
                    },
                    initials,
                ))
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
        let row = div()
            .id(self.id)
            .w_full()
            .min_w_0()
            .min_h(px(60.))
            .px(px(9.))
            .py(px(9.))
            .flex()
            .gap(px(12.))
            .rounded(CONTROL_RADIUS)
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
            .child(Self::text(palette, self.title, self.preview))
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
            .children(preview.map(|preview| {
                let text = match preview.sender {
                    Some(sender) => format!("{sender}: {}", preview.text),
                    None => preview.text.to_string(),
                };
                ellipsized_label(palette, TextRole::Caption, one_line(&text))
                    .debug_selector(|| "chat-row-preview".into())
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
    /// Рисует пузырь и переносит длинное сообщение по доступной ширине.
    pub fn render(self, palette: Palette) -> Div {
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
            .child(
                label(palette, TextRole::Body, self.body)
                    .min_w_0()
                    .max_w_full(),
            )
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
        let bubble = div()
            .max_w(px(680.))
            .min_w_0()
            .px(px(12.))
            .py(px(8.))
            .rounded(px(8.))
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
            .child(body);
        div()
            .w_full()
            .min_w_0()
            .flex()
            .when(self.own, |row| row.justify_end())
            .child(bubble)
    }
}
