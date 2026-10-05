//! Follow several log files in parallel, colouring their lines with regex rules.
//!
//! This library contains everything that doesn't depend on the terminal UI:
//! configuration, line processing and file following.

pub mod config;
pub mod line;
pub mod pattern;
pub mod style;
pub mod wrap;

#[cfg(test)]
mod test_util;
