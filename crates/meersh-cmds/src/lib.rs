//! Commands that work on data, independent of where the data comes from. They are the
//! object world counterpart of `grep`, `cut`, `sort`, `head` and `jq`.

mod authority;
mod filters;
mod formats;
mod meta;
mod session;

use meersh_engine::Engine;

pub fn register(engine: &mut Engine) {
    authority::register(engine);
    filters::register(engine);
    formats::register(engine);
    meta::register(engine);
    session::register(engine);
}
