//! ScanPro core: everything except the UI, so it can be driven from the GUI,
//! the CLI, and tests alike.

pub mod config;
pub mod export;
pub mod job;
pub mod naming;
pub mod ocr;
pub mod pipeline;
pub mod scanner;
pub mod system;
