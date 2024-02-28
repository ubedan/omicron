// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Nexus functionality, divided into capability groups

use nexus_db_queries::db;
use slog::Logger;

mod firewall_rules;
mod sled_agent;

pub use firewall_rules::FirewallRules;
pub use sled_agent::SledAgent;

pub trait Base {
    fn log(&self) -> &Logger;
    fn datastore(&self) -> &db::DataStore;
}
