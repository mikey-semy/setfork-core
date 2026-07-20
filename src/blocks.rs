//! Блочная модель: канонический предикат «это шаг» и ЕДИНСТВЕННОЕ место
//! правил нормализации type/content (аудит 2026-07-20, P2-13: правило было
//! скопировано в 8 местах — services/list, collab, db, git/project|serialize).
//! Всё остальное — блоки (text/image/poll/…), несущие type/content.
//! Пустая строка приравнена к 'step': proto3 не отличает '' от отсутствия
//! поля, БД хранит 'step' явно.

/// Шаг ли это (все прочие типы — блоки с type/content).
pub fn is_step_type(t: &str) -> bool {
    t.is_empty() || t == "step"
}

/// type для ХРАНЕНИЯ (БД): шаг — всегда явный 'step', блок — как есть.
pub fn storage_type(t: &str) -> String {
    if is_step_type(t) { "step".to_string() } else { t.to_string() }
}

/// type для ПРОВОДА (proto): шаг — '', блок — как есть. None (NULL из БД) = шаг.
pub fn wire_type(t: Option<&str>) -> String {
    match t {
        Some(t) if !is_step_type(t) => t.to_string(),
        _ => String::new(),
    }
}

/// content_json провода → jsonb payload: у шага и при пустом/битом JSON — {}
/// (мягкость к клиентскому вводу, как в TS-адаптерах).
pub fn content_value(step_type: &str, content_json: &str) -> serde_json::Value {
    if is_step_type(step_type) || content_json.is_empty() {
        serde_json::json!({})
    } else {
        serde_json::from_str(content_json).unwrap_or_else(|_| serde_json::json!({}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_predicate_and_normalization() {
        assert!(is_step_type(""));
        assert!(is_step_type("step"));
        assert!(!is_step_type("text"));
        assert_eq!(storage_type(""), "step");
        assert_eq!(storage_type("text"), "text");
        assert_eq!(wire_type(None), "");
        assert_eq!(wire_type(Some("step")), "");
        assert_eq!(wire_type(Some("image")), "image");
    }

    #[test]
    fn content_value_is_lenient() {
        assert_eq!(content_value("step", r#"{"a":1}"#), serde_json::json!({}));
        assert_eq!(content_value("text", ""), serde_json::json!({}));
        assert_eq!(content_value("text", "not json"), serde_json::json!({}));
        assert_eq!(content_value("text", r#"{"md":"x"}"#), serde_json::json!({"md":"x"}));
    }
}
