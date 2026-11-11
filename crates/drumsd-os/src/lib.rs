//! Android and Windows desktops behind the drumsd slot API (see pools.rs).
pub mod adb;
pub mod android;
pub mod exec;
pub mod http;
pub mod pools;
pub mod qmp;
pub mod tasks;
pub mod windows;
