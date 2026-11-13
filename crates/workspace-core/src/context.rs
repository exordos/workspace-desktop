//! Описывает, как приложение различает аккаунты, проекты и отдельные входы.

use serde::{Deserialize, Serialize};

/// Локальный идентификатор аккаунта, общий для его сессий на одном сервере.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AccountId(pub String); // Строковый идентификатор из адреса сервера и пользователя.

/// Определяет, кому и какому проекту принадлежат сессия и локальные данные.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct OwnerContext {
    pub account_id: AccountId,   // Локальный идентификатор аккаунта.
    pub server: String,          // Базовый адрес сервера Workspace.
    pub organization_id: String, // Идентификатор организации. Если он неизвестен, при входе используется адрес сервера.
    pub project_id: String,      // Идентификатор выбранного проекта.
    pub user_id: String,         // Идентификатор пользователя на сервере.
}

/// Номер сессии, который помогает отличить новый вход от старого.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SessionGeneration(pub u64); // Порядковый номер, выданный авторизацией.
