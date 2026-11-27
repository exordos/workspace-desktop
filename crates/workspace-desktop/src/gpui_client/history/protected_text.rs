//! Измеряет и рисует открытый текст сообщения с повторной проверкой доступа на каждом шаге GPUI.

use std::{
    cell::{Cell, RefCell},
    ops::Range,
    rc::Rc,
    sync::Arc,
};

use super::super::{
    inline_references::{MentionNames, reference_in_range},
    protected_quotes::QuoteText,
    references::time_zone_hint,
};
use crate::platform::message_time::format_local_time_zone;
use gpui::{
    App, AvailableSpace, Bounds, Element, ElementId, GlobalElementId, InspectorElementId,
    IntoElement, LayoutId, Pixels, SharedString, Size, Style, TextAlign, WhiteSpace, Window,
    WrappedLine,
};
use workspace_core::messenger::{
    catalog::ProtectedSidebarSummary,
    encryption::{AcceptedDecryption, ProtectedMessage},
    references::WorkspaceReferenceTarget,
};
use zeroize::Zeroizing;

use crate::gpui_client::preferences::Language;
use crate::gpui_client::ui::BUBBLE_TAIL_BREAK;

/// Отзывный материал сообщения для постановки в layout текущего кадра.
pub(in crate::gpui_client) struct ProtectedText(pub Arc<ProtectedMessage>); // Core проверяет сессию и версию ключа.

/// Текст с разрешением, которое проверяется при реальном замере и выводе глифов.
pub(in crate::gpui_client) struct GuardedText {
    source: TextSource, // Снимок либо его байтовый диапазон, действующие только вместе с разрешением.
    tail_suffix: Option<SharedString>, // Невидимое место под время последней строки обычного тела.
}

/// Общие источники отзываемого текста. Открытая строка не становится полем GPUI Entity.
#[derive(Clone)]
enum TextSource {
    Body(Arc<ProtectedMessage>, Option<Range<usize>>), // Тело или проверяемый поддиапазон.
    Fragment(Arc<AcceptedDecryption>),                 // Отдельно принятый защищённый фрагмент.
    Reference {
        access: Arc<ProtectedMessage>, // Действующее разрешение на это тело.
        range: Range<usize>,           // Границы заранее проверенной ссылки в теле.
        language: Language, // Используется для времени и подписи неизвестного пользователя.
        names: MentionNames, // Уже доступный справочник того же аккаунта.
        zone: bool, // Показывает пояс выбранного момента в отдельной строке панели.
    },
    Sidebar {
        access: Arc<ProtectedSidebarSummary>, // Ключ, который разрешает читать подпись.
        mounted: Rc<Cell<bool>>, // Текущее место подписи в каталоге.
        locale: Language, // Язык служебных вставок видимого аккаунта.
    },
    Quote {
        access: QuoteText, // Исходный и цитирующий ключи с проверенными связью и сроком жизни.
        placeholder: &'static str, // Подпись на языке окна, если фрагмент состоит из ссылки.
    },
}

impl TextSource {
    /// Даёт доступ к строке только на время одной операции рисования.
    fn with_text<T>(&self, read: impl FnOnce(&str) -> T) -> Option<T> {
        match self {
            Self::Body(access, None) => access.with_body(read),
            Self::Body(access, Some(range)) => access
                .with_body(|text| text.get(range.clone()).map(read))
                .flatten(),
            Self::Fragment(access) => access.with_text(read),
            Self::Reference {
                access,
                range,
                language,
                names,
                zone,
            } => access
                .with_body(|text| {
                    reference_in_range(text, range.clone()).map(|reference| {
                        let caption = if *zone {
                            match &reference.target {
                                Some(WorkspaceReferenceTarget::Time(time)) => time_zone_hint(
                                    *language,
                                    &format_local_time_zone(time, language.code()),
                                ),
                                _ => String::new(),
                            }
                        } else {
                            reference.caption(*language, |id| names.get(id))
                        };
                        let caption = Zeroizing::new(caption);
                        read(&caption)
                    })
                })
                .flatten(),
            Self::Sidebar { access, mounted, locale } => mounted.get().then(|| {
                access.with_preview(|preview| {
                    let text = Zeroizing::new(locale.preview_text(preview));
                    read(&text)
                })
            }).flatten(),
            Self::Quote {
                access,
                placeholder,
            } => access.with_text(placeholder, read),
        }
    }

    /// Проверяет разрешение без повторной подготовки исходного фрагмента.
    fn with_current<T>(&self, action: impl FnOnce() -> T) -> Option<T> {
        match self {
            Self::Quote { access, .. } => access.with_current(action),
            Self::Reference { access, .. } => access.with_current(action),
            Self::Sidebar { access, mounted, .. } => mounted.get().then(|| access.with_preview(|_| action())).flatten(),
            _ => self.with_text(|_| action()),
        }
    }
}

impl GuardedText {
    /// Показывает подпись ссылки под разрешением исходного сообщения, оставляя URL в Core.
    pub(in crate::gpui_client::conversation) fn reference(
        access: Arc<ProtectedMessage>,
        range: Range<usize>,
        language: Language,
        names: MentionNames,
    ) -> Self {
        Self {
            source: TextSource::Reference {
                access,
                range,
                language,
                names,
                zone: false,
            },
            tail_suffix: None,
        }
    }

    /// Показывает часовой пояс защищённой ссылки, пока действует её ключ.
    pub(in crate::gpui_client::conversation) fn reference_zone(
        access: Arc<ProtectedMessage>,
        range: Range<usize>,
        language: Language,
    ) -> Self {
        Self {
            source: TextSource::Reference {
                access,
                range,
                language,
                names: MentionNames::default(),
                zone: true,
            },
            tail_suffix: None,
        }
    }

    /// Показывает отдельно раскрытый фрагмент в фазах кадра под его собственным ключом.
    pub(in crate::gpui_client) fn fragment(access: Arc<AcceptedDecryption>) -> Self {
        Self {
            source: TextSource::Fragment(access),
            tail_suffix: None,
        }
    }

    /// Рисует цитату, сохраняя разрешение на оба сообщения до вывода глифов.
    pub(in crate::gpui_client) fn quote(access: QuoteText, placeholder: &'static str) -> Self {
        Self {
            source: TextSource::Quote {
                access,
                placeholder,
            },
            tail_suffix: None,
        }
    }
    /// Отображает только выбранный диапазон без создания постоянной копии Markdown.
    pub(in crate::gpui_client) fn range(
        access: Arc<ProtectedMessage>,
        range: Range<usize>,
    ) -> Self {
        Self {
            source: TextSource::Body(access, Some(range)),
            tail_suffix: None,
        }
    }

    /// Добавляет невидимый резерв к проверенному диапазону защищённого тела.
    pub(in crate::gpui_client) fn range_with_tail(
        access: Arc<ProtectedMessage>,
        range: Range<usize>,
        suffix: SharedString,
    ) -> Self {
        Self {
            source: TextSource::Body(access, Some(range)),
            tail_suffix: Some(suffix),
        }
    }

    /// Показывает короткое превью под текущим ключом в строке каталога.
    pub(in crate::gpui_client) fn summary(
        access: Arc<ProtectedSidebarSummary>,
        mounted: Rc<Cell<bool>>,
        locale: Language,
    ) -> Self {
        Self {
            source: TextSource::Sidebar { access, mounted, locale },
            tail_suffix: None,
        }
    }
}

impl ProtectedText {
    /// Добавляет резерв времени к обычному защищённому телу в рамках того же разрешения.
    pub(in crate::gpui_client) fn with_tail_suffix(self, suffix: SharedString) -> GuardedText {
        GuardedText {
            source: TextSource::Body(self.0, None),
            tail_suffix: Some(suffix),
        }
    }
}

/// Геометрия текста одного кадра, без строки, пригодной к дальнейшим действиям.
pub(in crate::gpui_client) struct FrameText {
    lines: Rc<RefCell<Vec<WrappedLine>>>, // GPUI временно держит сформированные строки.
    line_height: Pixels,                  // Высота строки в роли текста бабла.
    align: TextAlign,                     // Выравнивание внутри вычисленных границ.
}

impl IntoElement for ProtectedText {
    type Element = GuardedText;

    /// Передаёт полный диапазон общему элементу, проверяющему доступ в фазах GPUI.
    fn into_element(self) -> Self::Element {
        GuardedText {
            source: TextSource::Body(self.0, None),
            tail_suffix: None,
        }
    }
}

impl IntoElement for GuardedText {
    type Element = Self;

    /// Сохраняет проверку доступа в фазах GPUI.
    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for GuardedText {
    type RequestLayoutState = FrameText;
    type PrepaintState = bool;

    /// Пользуется адресом внешнего бабла, где есть UUID сообщения.
    fn id(&self) -> Option<ElementId> {
        None
    }

    /// Элемент не требует отдельной привязки отладчика GPUI.
    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    /// Передаёт отложенному замеру отзывный держатель. Текст читается лишь в самом замере.
    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        _: &mut App,
    ) -> (LayoutId, FrameText) {
        let mut style = window.text_style();
        if matches!(self.source, TextSource::Sidebar { .. }) {
            style.line_clamp = Some(1);
        }
        let font_size = style.font_size.to_pixels(window.rem_size());
        let line_height = style
            .line_height
            .to_pixels(font_size.into(), window.rem_size());
        let frame = FrameText {
            lines: Rc::default(),
            line_height,
            align: style.text_align,
        };
        let prepared = frame.lines.clone();
        let access = self.source.clone();
        let tail_suffix = self.tail_suffix.clone();
        let layout =
            window.request_measured_layout(Style::default(), move |known, available, window, _| {
                let width = if style.white_space == WhiteSpace::Normal {
                    known.width.or(match available.width {
                        AvailableSpace::Definite(width) => Some(width),
                        _ => None,
                    })
                } else {
                    None
                };
                let mut prepared = prepared.borrow_mut();
                prepared.clear();
                access
                    .with_text(|text| {
                        let mut shaped = Zeroizing::new(text.to_owned());
                        let mut runs = vec![style.to_run(text.len())];
                        if let Some(suffix) = &tail_suffix {
                            shaped.push_str(BUBBLE_TAIL_BREAK);
                            shaped.push_str(suffix);
                            let tail_len = BUBBLE_TAIL_BREAK.len() + suffix.len();
                            let mut tail_run = style.to_run(tail_len);
                            tail_run.color = style.color.opacity(0.);
                            runs.push(tail_run);
                        }
                        let Ok(lines) = window.text_system().shape_text(
                            SharedString::new(shaped.as_str()),
                            font_size,
                            &runs,
                            width,
                            style.line_clamp,
                        ) else {
                            return Size::default();
                        };
                        let result =
                            lines
                                .iter()
                                .fold(Size::<Pixels>::default(), |mut size, line| {
                                    let line = line.size(line_height);
                                    size.height += line.height;
                                    size.width = size.width.max(line.width).ceil();
                                    size
                                });
                        prepared.extend(lines);
                        result
                    })
                    .unwrap_or_default()
            });
        (layout, frame)
    }

    /// Если ключ уже отозван, прежняя геометрия больше не нужна.
    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        frame: &mut FrameText,
        _: &mut Window,
        _: &mut App,
    ) -> bool {
        let allowed = self.source.with_current(|| ()).is_some();
        if !allowed {
            frame.lines.borrow_mut().clear();
        }
        allowed
    }

    /// Рисует глифы лишь пока разрешение на этот текст действует.
    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        frame: &mut FrameText,
        allowed: &mut bool,
        window: &mut Window,
        cx: &mut App,
    ) {
        if *allowed {
            let _ = self.source.with_current(|| {
                let mut origin = bounds.origin;
                for line in frame.lines.borrow().iter() {
                    let _ = line.paint(
                        origin,
                        frame.line_height,
                        frame.align,
                        Some(bounds),
                        window,
                        cx,
                    );
                    origin.y += line.size(frame.line_height).height;
                }
            });
        }
    }
}

#[cfg(all(test, feature = "gpui-test"))]
mod tests {
    use std::cell::Cell;

    use gpui::{Context, Render, TestAppContext, canvas, div, prelude::*, px, size};
    use uuid::Uuid;
    use workspace_core::{
        context::{AccountId, OwnerContext, SessionGeneration},
        messenger::encryption::{
            EncryptionActivation, EncryptionSession, GroupKey, MessageEncryption,
        },
    };

    use super::super::super::protected_body::{ProtectedLayout, ProtectedReferenceControls};
    use super::*;
    use crate::platform::chat_keys::ProfileChatKeys;
    use crate::gpui_client::ui::{
        BubbleContent, DeliveryMark, MessageBubble, Palette, TextRole, text_role,
    };

    /// Показывает защищённое тело с резервом времени внутри отзываемого текстового элемента.
    struct ProtectedTailView {
        holder: Arc<ProtectedMessage>, // Текст доступен только пока действует его ключ.
    }

    impl Render for ProtectedTailView {
        /// Собирает защищённое сообщение тем же общим баблом, что и открытый текст.
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let palette = Palette::for_window(window, cx);
            let message = MessageBubble {
                author: None,
                body: "".into(),
                time: Some("10:32".into()),
                own: true,
                delivery: Some(DeliveryMark::Read),
            };
            let suffix = message
                .tail_reserve(palette, window)
                .expect("у сообщения есть время и статус")
                .suffix()
                .to_owned()
                .into();
            let layout = self
                .holder
                .with_body(ProtectedLayout::parse)
                .expect("тело всё ещё доступно");
            let names = MentionNames::default();
            let protected = layout.render(
                Uuid::from_u128(777),
                &self.holder,
                palette,
                true,
                None,
                false,
                Some(suffix),
                ProtectedReferenceControls {
                    conversation: None,
                    language: Language::En,
                    names: &names,
                },
                |_, _, _, _| div().into_any_element(),
            );
            let body = text_role(palette, TextRole::Body)
                .min_w_0()
                .max_w_full()
                .child(protected)
                .debug_selector(|| "protected-tail-body".into());
            div().size_full().child(message.render_with_content(
                palette,
                Some(BubbleContent::InlineTail(body.into_any_element())),
                true,
                None,
                None,
                false,
            ))
        }
    }

    /// Оставляет метаданные у последней защищённой строки, измеренной под тем же ключом.
    #[gpui::test]
    fn protected_text_tail_keeps_metadata_at_the_body_edge(cx: &mut TestAppContext) {
        let engine = Arc::new(MessageEncryption::new(Arc::new(
            ProfileChatKeys::temporary(),
        )));
        let activation = engine
            .select_session(Some(EncryptionSession {
                owner: OwnerContext {
                    account_id: AccountId("test".into()),
                    server: "https://workspace.test".into(),
                    organization_id: "org".into(),
                    project_id: "project".into(),
                    user_id: "owner".into(),
                },
                generation: SessionGeneration(1),
                device_id: Uuid::from_u128(42),
            }))
            .unwrap();
        let stream = "10000000-0000-4000-8000-000000000005".parse().unwrap();
        let key = "10000000-0000-4000-8000-000000000080".parse().unwrap();
        engine
            .save_key(&activation, stream, key, &GroupKey::from_bytes([7; 32]))
            .unwrap();
        // Публичный синтетический вектор не содержит реальных пользовательских данных.
        let encoded = "AwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwM1BE3iAOkrgJ3KRf/RmAQZdeFqucTcLZYqkvZLUm+5hT2YqTPupmbNHVCmw77C/ZSMcN1WTelv3nhrIi1blpcecnNKAV8=";
        let holder = Arc::new(
            engine
                .accept(
                    engine
                        .prepare(&activation, stream, key)
                        .unwrap()
                        .decrypt(encoded),
                )
                .unwrap(),
        );
        assert!(holder.with_body(|text| text.len() > 40).unwrap());
        let (_, ui) = cx.add_window_view(|_, _| ProtectedTailView { holder });
        ui.simulate_resize(size(px(250.), px(640.)));
        ui.run_until_parked();
        let body = ui.debug_bounds("message-body").unwrap();
        let protected = ui.debug_bounds("protected-tail-body").unwrap();
        let meta = ui.debug_bounds("message-meta").unwrap();
        assert!(
            protected.size.height >= px(30.),
            "текст должен перенестись: {protected:?}"
        );
        assert_eq!(meta.bottom(), body.bottom());
        assert_eq!(meta.right(), body.right());
    }

    /// Измеряет элемент после того, как рабочий ключ уже успели удалить.
    struct MeasureProbe {
        holder: Arc<ProtectedMessage>, // Результат, действующий в момент регистрации замера.
        engine: Arc<MessageEncryption>, // Позволяет отозвать результат между двумя фазами.
        activation: EncryptionActivation, // Тот же подтверждённый выбор входа.
        stream: Uuid,                  // Стрим тестового тела.
        key: Uuid,                     // Точное поколение тестового тела.
        observed_lines: Rc<Cell<Option<usize>>>, // Сколько открытых строк реально сформировали.
    }

    impl Render for MeasureProbe {
        /// Проводит обе фазы в одном кадре, чтобы порядок операций был управляемым.
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let element = self
                .holder
                .with_body(|text| 0..text.len())
                .map(|range| {
                    GuardedText::range_with_tail(self.holder.clone(), range, "MMMMM".into())
                });
            let mut element = element;
            let engine = self.engine.clone();
            let activation = self.activation.clone();
            let (stream, key) = (self.stream, self.key);
            let observed = self.observed_lines.clone();
            canvas(
                move |_, window, cx| {
                    let Some(element) = element.as_mut() else {
                        return;
                    };
                    // Canvas упорядочивает реальный request_layout и Taffy measure в одном кадре.
                    let (layout, frame) = element.request_layout(None, None, window, cx);
                    engine.remove_key(&activation, stream, key).unwrap();
                    window.compute_layout(
                        layout,
                        size(
                            AvailableSpace::Definite(px(240.)),
                            AvailableSpace::MinContent,
                        ),
                        cx,
                    );
                    observed.set(Some(frame.lines.borrow().len()));
                },
                |_, (), _, _| (),
            )
        }
    }

    /// После удаления ключа отложенный Taffy callback не получает текст или сформированные глифы.
    #[gpui::test]
    fn protected_measure_checks_revocation_after_registration(cx: &mut TestAppContext) {
        let engine = Arc::new(MessageEncryption::new(Arc::new(
            ProfileChatKeys::temporary(),
        )));
        let activation = engine
            .select_session(Some(EncryptionSession {
                owner: OwnerContext {
                    account_id: AccountId("test".into()),
                    server: "https://workspace.test".into(),
                    organization_id: "org".into(),
                    project_id: "project".into(),
                    user_id: "owner".into(),
                },
                generation: SessionGeneration(1),
                device_id: Uuid::from_u128(42),
            }))
            .unwrap();
        let stream = "10000000-0000-4000-8000-000000000005".parse().unwrap();
        let key = "10000000-0000-4000-8000-000000000080".parse().unwrap();
        engine
            .save_key(&activation, stream, key, &GroupKey::from_bytes([7; 32]))
            .unwrap();
        // Публичный синтетический вектор из локального сценария. Реальных секретов в нём нет.
        let encoded = "AwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwM1BE3iAOkrgJ3KRf/RmAQZdeFqucTcLZYqkvZLUm+5hT2YqTPupmbNHVCmw77C/ZSMcN1WTelv3nhrIi1blpcecnNKAV8=";
        let holder = Arc::new(
            engine
                .accept(
                    engine
                        .prepare(&activation, stream, key)
                        .unwrap()
                        .decrypt(encoded),
                )
                .unwrap(),
        );
        assert_eq!(holder.with_body(|text| text.lines().count()), Some(2));
        let observed_lines = Rc::new(Cell::new(None));
        let (_view, ui) = cx.add_window_view(|_, _| MeasureProbe {
            holder,
            engine,
            activation,
            stream,
            key,
            observed_lines: observed_lines.clone(),
        });
        ui.update(|window, app| window.draw(app).clear());
        assert_eq!(observed_lines.get(), Some(0));
    }
}
