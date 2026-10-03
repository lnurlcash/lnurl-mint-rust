//! LDK's log records, into the `log` crate under the `ldk` target.

use lightning::util::logger::{Level, Logger, Record};

#[derive(Debug, Default)]
pub struct LdkLogger;

impl Logger for LdkLogger {
    fn log(&self, record: Record) {
        let level = match record.level {
            // incredibly verbose, and of no use to an operator
            Level::Gossip => return,
            Level::Trace => log::Level::Trace,
            Level::Debug => log::Level::Debug,
            Level::Info => log::Level::Info,
            Level::Warn => log::Level::Warn,
            Level::Error => log::Level::Error,
        };
        log::log!(target: "ldk", level, "[{}:{}] {}", record.module_path, record.line, record.args);
    }
}
