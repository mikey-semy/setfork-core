//! Машиночитаемая причина отказа: стабильный код рядом со статусом.
//!
//! # Зачем
//!
//! Фронт различал причины **разбором прозы**: `rawMessage.includes("conflict")`
//! и соседние строки в `features/git/core.remote.ts`. Это ломается от любой
//! правки текста и уже подвело — отказы, добавленные в Ф0 и Ф1 (лишний путь в
//! дереве, переполненный репозиторий, заморожен, в архиве), не сопоставлялись
//! ни с чем, и человек видел общую ошибку вместо причины.
//!
//! AIP-193 говорит про это прямо: клиент обязан опираться на машиночитаемый
//! `reason`, а не на текст, потому что «error messages often contain dynamic
//! segments that express variable information». Формат оттуда же: UPPER_SNAKE_CASE,
//! до 63 символов.
//!
//! # Почему трейлером, а не `grpc-status-details-bin`
//!
//! Богатая модель ошибок gRPC (`google.rpc.Status` + `ErrorInfo` в деталях) —
//! это её штатное место, и мы берём оттуда ПРИНЦИП. Но не механику: документация
//! gRPC сама перечисляет цену — реализации несогласованы между языками, детали
//! невидимы прокси и логгерам, трейлеры хуже жмутся HPACK и могут упереться в
//! лимит размера заголовков. Платить это имеет смысл ради чужих клиентов и
//! стандартной оснастки; у нас канал приватный, закрыт общим токеном, proto не
//! публикуется, а потребитель ровно один — наш фронт.
//!
//! Тот же ход уже записан в ADR-0011 §3: таксономия ошибок — маппер, а не
//! иерархия, «пересмотреть, если появится второй потребитель ошибок кроме
//! gRPC-границы». Это ровно тот триггер: **появится второй потребитель — переходим
//! на `ErrorInfo` в деталях**, принцип менять не придётся, только транспорт.
use tonic::{Code, Status};

/// Ключ трейлера с причиной. Строчные буквы и дефис — как принято у gRPC-метаданных.
pub const REASON_KEY: &str = "sf-reason";

/// Причины, которые фронт различает и показывает человеку по-разному.
///
/// Значения — часть контракта провода: их читает `core.remote.ts`. Менять
/// значение = ломать клиента, поэтому добавлять новые можно, переименовывать
/// существующие — только парной правкой.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// Имя ветки/тега не проходит валидацию.
    BadName,
    /// Ветка с таким именем уже есть.
    Exists,
    /// Списка, ветки или базы не существует.
    NotFound,
    /// Ветка `main` защищена (удаление, переименование).
    Protected,
    /// Слияние не проходит чисто.
    Conflict,
    /// Вливать нечего: ветка не впереди базы.
    NothingToMerge,
    /// Ветку подвинули между чтением и записью (оптимистичная блокировка).
    Stale,
    /// В дереве коммита есть путь вне allowlist'а (Ф0).
    ForeignPath,
    /// В дереве коммита нет `list.json` (канон обязателен).
    MissingListJson,
    /// Репозиторий списка превысил порог размера (Ф0).
    RepoTooLarge,
    /// Список заморожен владельцем (Ф1, вердикт приложения).
    Frozen,
    /// Список в архиве (Ф1, вердикт приложения).
    Archived,
    /// Предусловие записи спросить не удалось — fail-closed (Ф1).
    GateUnavailable,
    /// Имя тега зарезервировано под версии (`vN`).
    ReservedTagName,
}

impl Reason {
    /// Значение на проводе. UPPER_SNAKE_CASE по AIP-193.
    pub const fn as_str(self) -> &'static str {
        match self {
            Reason::BadName => "BAD_NAME",
            Reason::Exists => "EXISTS",
            Reason::NotFound => "NOT_FOUND",
            Reason::Protected => "PROTECTED",
            Reason::Conflict => "CONFLICT",
            Reason::NothingToMerge => "NOTHING_TO_MERGE",
            Reason::Stale => "STALE",
            Reason::ForeignPath => "FOREIGN_PATH",
            Reason::MissingListJson => "MISSING_LIST_JSON",
            Reason::RepoTooLarge => "REPO_TOO_LARGE",
            Reason::Frozen => "FROZEN",
            Reason::Archived => "ARCHIVED",
            Reason::GateUnavailable => "GATE_UNAVAILABLE",
            Reason::ReservedTagName => "RESERVED_TAG_NAME",
        }
    }
}

/// Статус с причиной в трейлере.
///
/// Текст сообщения остаётся английским и человеческим — он идёт в логи и в
/// отладку; клиент на него больше не смотрит. Разделение намеренное: текст можно
/// менять свободно, причину — нет.
pub fn status(code: Code, reason: Reason, message: impl Into<String>) -> Status {
    let mut md = tonic::metadata::MetadataMap::new();
    // Значение из as_str() — всегда ASCII, разбор не может провалиться; но
    // unwrap здесь всё равно неуместен: молча потерять причину хуже, чем
    // отдать статус без неё, а собрать ключ криво нельзя по построению.
    if let Ok(v) = reason.as_str().parse() {
        md.insert(REASON_KEY, v);
    }
    Status::with_metadata(code, message.into(), md)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn причина_едет_в_трейлере() {
        let s = status(Code::FailedPrecondition, Reason::Conflict, "merge conflict");
        assert_eq!(s.code(), Code::FailedPrecondition);
        assert_eq!(s.message(), "merge conflict");
        assert_eq!(s.metadata().get(REASON_KEY).and_then(|v| v.to_str().ok()), Some("CONFLICT"));
    }

    /// Формат из AIP-193: UPPER_SNAKE_CASE, до 63 символов, без ведущих цифр и
    /// без крайних подчёркиваний. Проверяем ВСЕ значения — вручную такое
    /// разъезжается при добавлении новой причины.
    #[test]
    fn все_причины_по_формату_aip_193() {
        let all = [
            Reason::BadName,
            Reason::Exists,
            Reason::NotFound,
            Reason::Protected,
            Reason::Conflict,
            Reason::NothingToMerge,
            Reason::Stale,
            Reason::ForeignPath,
            Reason::MissingListJson,
            Reason::RepoTooLarge,
            Reason::Frozen,
            Reason::Archived,
            Reason::GateUnavailable,
            Reason::ReservedTagName,
        ];
        let mut seen = std::collections::HashSet::new();
        for r in all {
            let s = r.as_str();
            assert!(!s.is_empty() && s.len() <= 63, "{s}: длина");
            assert!(
                s.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'),
                "{s}: символы"
            );
            assert!(!s.starts_with('_') && !s.ends_with('_'), "{s}: крайние подчёркивания");
            assert!(!s.starts_with(|c: char| c.is_ascii_digit()), "{s}: ведущая цифра");
            assert!(seen.insert(s), "{s}: значение продублировано — клиент не различит причины");
        }
    }
}
