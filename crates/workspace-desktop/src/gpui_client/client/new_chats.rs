//! Связывает режим создания с общим хостом диалогов и штатными маршрутами истории.

use gpui::{Context, Entity, Window, prelude::*};

use crate::gpui_client::{
    sidebar::{
        Sidebar,
        new_chats::{ChannelDialog, EncryptedChannelFields, Navigation, NewChats},
    },
    ui::TopBarSection,
};

use super::{Client, encrypted_channel::OpenEncryptedChannel};

/// Направляет только подтверждённые цели создания в оболочку окна.
pub(super) fn subscribe(sidebar: &Entity<Sidebar>, cx: &mut Context<Client>) {
    let view = sidebar.read(cx).new_chats();
    cx.subscribe(&view, |client, _, target: &Navigation, cx| {
        client.open_new_chat(target, cx);
    })
    .detach();
}

impl Client {
    /// Открывает тот же экран истории, что использует основной сайдбар.
    fn open_new_chat(&mut self, target: &Navigation, cx: &mut Context<Self>) {
        if self.section != TopBarSection::Chats || self.status().is_some() {
            return;
        }
        let Some(effect) =
            self.shell
                .open_confirmed_chat(&target.session, target.scope, target.entry)
        else {
            return;
        };
        self.apply(Some(effect), cx);
        if let (Some(chat), Some(view)) = (&target.chat, &self.conversation) {
            view.update(cx, |view, cx| view.set_new_chat_heading(chat, cx));
        }
    }

    /// Передаёт создание канала модальному хосту, когда доступно рабочее окно.
    pub(super) fn open_requested_new_channel(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let creation = self.sidebar.read(cx).new_chats();
        let close = creation.update(cx, |view, _| view.take_channel_close());
        if close && self.right_panel.read(cx).modal_is::<ChannelDialog>() {
            self.right_panel
                .update(cx, |host, cx| host.close_modal(window, cx));
            if !creation.read(cx).is_open() {
                self.sidebar.read(cx).focus_search(window, cx);
            }
        }
        if self.section != TopBarSection::Chats
            || self.status().is_some()
            || self.right_panel.read(cx).has_modal()
        {
            return;
        }
        let Some((session, opener, revision)) =
            creation.update(cx, |view: &mut NewChats, _| view.take_channel_request())
        else {
            return;
        };
        let runtime = self.runtime.clone();
        let origin = creation.downgrade();
        let dialog_session = session.clone();
        let dialog = self.right_panel.update(cx, |host, cx| {
            let mut dialog = None;
            host.open_modal(&session, opener.clone(), window, cx, |region, cx| {
                let entity = cx.new(|cx| {
                    ChannelDialog::new(runtime, dialog_session, revision, origin, region, cx)
                });
                dialog = Some(entity.clone());
                entity
            });
            if let Some(dialog) = &dialog {
                let dialog = dialog.downgrade();
                host.set_modal_dismiss(move |app| {
                    dialog
                        .upgrade()
                        .is_none_or(|entity| entity.read(app).can_dismiss())
                });
            }
            dialog
        });
        if let Some(dialog) = dialog {
            cx.subscribe(
                &dialog,
                move |client, _, fields: &EncryptedChannelFields, cx| {
                    client.request_encrypted_channel(
                        OpenEncryptedChannel {
                            session: session.clone(),
                            opener: opener.clone(),
                            fields: fields.clone(),
                        },
                        cx,
                    );
                },
            )
            .detach();
        }
    }
}
