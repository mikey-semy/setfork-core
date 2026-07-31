//! setfork-core как библиотека: модули доступны интеграционным тестам (tests/)
//! и бинарю (main.rs). Точка входа сервера — src/main.rs.
pub mod blocks;
pub mod config;
pub mod db;
pub mod gate;
pub mod git;
pub mod pb;
pub mod pb_domain;
pub mod ratelimit;
pub mod reason;
pub mod services;
pub mod telemetry;
