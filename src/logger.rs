use owo_colors::OwoColorize;
use std::fmt::Display;

pub fn log_info_impl(msg: impl Display) {
    println!(
        "{} {} {}",
        chrono::Utc::now()
            .format("%Y-%m-%dT%H:%M:%S%.6fZ")
            .to_string()
            .bright_black(),
        "[INFO]".blue(),
        msg
    );
}

pub fn log_warn_impl(msg: impl Display) {
    println!(
        "{} {} {}",
        chrono::Utc::now()
            .format("%Y-%m-%dT%H:%M:%S%.6fZ")
            .to_string()
            .bright_black(),
        "[WARN]".yellow(),
        msg
    );
}

pub fn log_error_impl(msg: impl Display) {
    println!(
        "{} {} {}",
        chrono::Utc::now()
            .format("%Y-%m-%dT%H:%M:%S%.6fZ")
            .to_string()
            .bright_black(),
        "[ERROR]".red(),
        msg
    );
}

#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => {
        $crate::logger::log_info_impl(format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_warn {
    ($($arg:tt)*) => {
        $crate::logger::log_warn_impl(format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_error {
    ($($arg:tt)*) => {
        $crate::logger::log_error_impl(format_args!($($arg)*))
    };
}

pub(crate) use log_error;
pub(crate) use log_info;
pub(crate) use log_warn;
