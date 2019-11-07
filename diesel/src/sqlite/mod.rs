//! Provides types and functions related to working with SQLite
//!
//! Much of this module is re-exported from database agnostic locations.
//! However, if you are writing code specifically to extend Diesel on
//! SQLite, you may need to work with this module directly.

mod backend;
mod connection;
mod types;

pub mod query_builder;

use std::sync::Arc;
pub use self::backend::{Sqlite, SqliteType};
pub use self::connection::SqliteConnection;
pub use self::query_builder::SqliteQueryBuilder;
pub use self::connection::is_readonly_tx;

pub trait ErrorHook {
    fn on_error(&self, code: i32);
}

static mut ERROR_HOOK: Option<Box<Arc<ErrorHook>>> = None;

fn on_error(code: i32) {
    unsafe {
        if let Some(ref hook) = ERROR_HOOK {
            hook.on_error(code);
        }
    }
}

/// init error hook, must init first
pub fn init_error_hook(err_hook: Box<Arc<ErrorHook>>) {
    unsafe {
        ERROR_HOOK = Some(err_hook);
    }
}
