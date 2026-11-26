pub(crate) mod accounts;
pub mod bot;
mod pan115;
mod quark;
pub(crate) mod search;
mod tianyi;

use crate::{AppError, Shared};
use axum::Router;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

pub(crate) fn init_schema(conn: &Connection) -> Result<(), AppError> {
    accounts::init_schema(conn)?;
    search::init_schema(conn)?;
    bot::init_schema(conn)
}

pub(crate) fn router() -> Router<Shared> {
    accounts::router()
        .merge(search::router())
        .merge(bot::router())
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Backup {
    accounts: accounts::Backup,
    channels: search::Backup,
    bot: bot::Backup,
}

pub(crate) fn export_config(conn: &Connection) -> Result<Backup, AppError> {
    Ok(Backup {
        accounts: accounts::export_config(conn)?,
        channels: search::export_config(conn)?,
        bot: bot::export_config(conn)?,
    })
}

pub(crate) fn restore_config(conn: &Connection, backup: Backup) -> Result<(), AppError> {
    accounts::restore_config(conn, backup.accounts)?;
    search::restore_config(conn, backup.channels)?;
    bot::restore_config(conn, backup.bot)
}
