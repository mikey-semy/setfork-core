//! Централизованная конфигурация: все env читаются ОДИН раз на старте, с
//! валидацией (аудит 2026-07-20, P2-14: чтение было ad-hoc по модулям, токен
//! читался дважды — main и ratelimit).
//!
//! Вне Config остаётся только GIT_DATA_DIR: он нужен не всем режимам
//! (golden-CLI работают без него) и читается лениво в git::repo::root() —
//! наличие проверяет require_git_data_dir в main до старта сервера/reproject.
use std::net::SocketAddr;

pub struct Config {
    pub database_url: String,
    pub pgpool_max: u32,
    /// Адрес gRPC-сервера (SETFORK_CORE_ADDR, дефолт 127.0.0.1:50051).
    pub addr: SocketAddr,
    /// Адрес /metrics; None = выключено ('0'/'off').
    pub metrics_addr: Option<SocketAddr>,
    /// Bearer-токен канала (без префикса). None = токена нет.
    pub token: Option<String>,
    /// Явный опт-аут auth для локального dev (SETFORK_ALLOW_INSECURE=1).
    pub allow_insecure: bool,
    pub rpm: u32,
    pub rpm_heavy: u32,
    /// Потолок ПРИНИМАЕМОГО gRPC-сообщения, байты (SETFORK_MAX_RECV_MB, дефолт 32 МБ).
    ///
    /// Только приём. Дефолты tonic 0.14.6 асимметричны (сверено с исходником
    /// `codec/mod.rs`): `DEFAULT_MAX_RECV_MESSAGE_SIZE` = 4 МиБ, а
    /// `DEFAULT_MAX_SEND_MESSAGE_SIZE` = `usize::MAX`. То есть необъявленный
    /// потолок есть ровно на входе, и бьёт он по `ReceivePack`: тело пуша едет
    /// ОДНИМ сообщением, и на 4 МиБ push молча перестал бы проходить.
    ///
    /// Отдачу (`max_encoding_message_size`) НЕ трогаем сознательно: сейчас она не
    /// ограничена, и выставить туда конечное число значило бы СОЗДАТЬ потолок для
    /// клона и бандла там, где его нет. Памяти это не сэкономило бы: пак и так
    /// собирается в `Vec<u8>` целиком, лимит лишь уронил бы отправку постфактум.
    pub max_recv_bytes: usize,
}

/// Порог размера bare-репо, байты; 0 = без ограничения
/// (SETFORK_REPO_LIMIT_MB, дефолт 64 МБ). Превышение → отказ приёма пуша.
///
/// Не поле `Config`, а ленивый аксессор (как `mirror::mirror_secret`): значение
/// нужно сервису на пути запроса, а `GitCoreSvc` конструируется от одного пула —
/// тащить конфиг через него, CLI-режимы и тесты дороже, чем одно чтение env.
/// Читается ровно раз.
pub fn repo_limit_bytes() -> u64 {
    static V: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *V.get_or_init(|| env_u32("SETFORK_REPO_LIMIT_MB", 64) as u64 * 1024 * 1024)
}

// SETFORK_LOG_JSON читает init_tracing в main напрямую: подписчик логов
// поднимается ДО Config::from_env, иначе warn'ы валидации конфига пропадут.

fn env_u32(name: &str, default: u32) -> u32 {
    match std::env::var(name) {
        Ok(s) => match s.trim().parse::<u32>() {
            Ok(v) => v,
            // Опечатка в конфиге не должна МОЛЧА откатывать значение на дефолт.
            Err(_) => {
                tracing::warn!(var = name, value = %s, "not a number, using {default}");
                default
            }
        },
        Err(_) => default,
    }
}

impl Config {
    pub fn from_env() -> Result<Config, String> {
        let database_url =
            std::env::var("DATABASE_URL").map_err(|_| "DATABASE_URL is not set (see .env)".to_string())?;

        let pgpool_max = match env_u32("PGPOOL_MAX", 10) {
            0 => {
                tracing::warn!("PGPOOL_MAX=0 is meaningless, using 10");
                10
            }
            v => v,
        };

        let addr_raw = std::env::var("SETFORK_CORE_ADDR").unwrap_or_else(|_| "127.0.0.1:50051".into());
        let addr: SocketAddr = addr_raw
            .parse()
            .map_err(|e| format!("SETFORK_CORE_ADDR '{addr_raw}' is invalid ({e}) - expected host:port"))?;

        let maddr_raw = std::env::var("SETFORK_METRICS_ADDR").unwrap_or_else(|_| "127.0.0.1:9464".into());
        let metrics_addr = if maddr_raw == "0" || maddr_raw.eq_ignore_ascii_case("off") {
            None
        } else {
            Some(
                maddr_raw
                    .parse::<SocketAddr>()
                    .map_err(|e| format!("SETFORK_METRICS_ADDR '{maddr_raw}' is invalid ({e})"))?,
            )
        };

        let token = std::env::var("SETFORK_CORE_TOKEN").ok().filter(|t| !t.is_empty());
        let allow_insecure = std::env::var("SETFORK_ALLOW_INSECURE")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        Ok(Config {
            database_url,
            pgpool_max,
            addr,
            metrics_addr,
            token,
            allow_insecure,
            rpm: env_u32("SETFORK_RPC_RPM", 600),
            rpm_heavy: env_u32("SETFORK_RPC_RPM_HEAVY", 60),
            // 0 бессмысленен (сообщение нулевого размера не пройдёт вообще) —
            // трактуем как «оставить дефолт», а не как «запретить всё».
            max_recv_bytes: match env_u32("SETFORK_MAX_RECV_MB", 32) {
                0 => {
                    tracing::warn!("SETFORK_MAX_RECV_MB=0 would block every RPC, using 32");
                    32 * 1024 * 1024
                }
                mb => mb as usize * 1024 * 1024,
            },
        })
    }
}
