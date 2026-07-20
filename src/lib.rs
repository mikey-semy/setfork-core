//! setfork-core как библиотека: модули доступны интеграционным тестам (tests/)
//! и бинарю (main.rs). Точка входа сервера — src/main.rs.
pub mod blocks;
pub mod db;
pub mod git;
pub mod pb;
pub mod pb_domain;
pub mod ratelimit;
pub mod services;
pub mod telemetry;
