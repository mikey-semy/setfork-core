//! СТРОГИЙ разбор канона list.json — вход редактора «правка списка как кода».
//!
//! Проекция push (`project.rs`) читает канон МЯГКО и намеренно: чужой клон мог
//! отстать от формата, шаг без заголовка там просто выпадает, и версия всё равно
//! создаётся. Для редактора это ровно то, чего делать нельзя: человек правит текст
//! и обязан узнать, что именно в нём не так, — молчаливая потеря пункта выглядела бы
//! как «сохранил и потерял».
//!
//! Поэтому здесь второй режим чтения того же формата, а не вторая копия формата:
//! правила берутся из ОПУБЛИКОВАННОЙ схемы `schema/list.v1.json` (той самой, чей URL
//! ядро кладёт в `$schema` каждого канона), а не переписываются условиями в коде.
//! Схема меняется — разбор меняется вместе с ней, без правок здесь.
//!
//! Позиции в буфере (строка, колонка) считает не ядро, а редактор: ядру принадлежит
//! ФОРМАТ, а не то, как текст разложен по строкам в чужом окне. Исключение —
//! синтаксическая ошибка: указатель на поле для неё не существует, и место разрыва
//! знает только разборщик.

use serde::Deserialize;

/// Что не так с текстом. `path` — JSON Pointer (RFC 6901) к месту ошибки: редактор
/// находит по нему узел в своём дереве разбора и подсвечивает.
pub struct CanonIssue {
    /// JSON Pointer к узлу: `/steps/3/title`. Пусто — ошибка относится ко всему тексту.
    pub path: String,
    /// Стабильный код для словаря вызывающего. Текст ошибки локализует ОН: язык
    /// читателя знает интерфейс, а не ядро.
    pub code: IssueCode,
    /// Техническое пояснение — для журнала и как запасной текст, если кода мало.
    pub message: String,
    /// Строка (1-based) — только у синтаксической ошибки, иначе 0.
    pub line: u32,
    /// Колонка (1-based) — только у синтаксической ошибки, иначе 0.
    pub column: u32,
}

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub enum IssueCode {
    /// Текст не разобрался как JSON.
    Syntax,
    /// Значение не отвечает опубликованной схеме манифеста.
    Schema,
    /// Шаг без заголовка: проекция молча выбросила бы такой пункт.
    StepTitleRequired,
}

impl IssueCode {
    /// Код на проводе — то, по чему вызывающий выбирает текст в своём словаре.
    pub fn as_str(self) -> &'static str {
        match self {
            IssueCode::Syntax => "syntax",
            IssueCode::Schema => "schema",
            IssueCode::StepTitleRequired => "step_title_required",
        }
    }
}

/// Схема — ровно тот файл, что опубликован по `serialize::schema_url()`. Вшита в
/// бинарь: правила формата обязаны ехать вместе с кодом, который его пишет.
const SCHEMA: &str = include_str!("../../schema/list.v1.json");

#[derive(Deserialize)]
struct StepTitleProbe {
    #[serde(rename = "type")]
    block_type: Option<String>,
    title: Option<String>,
}

/// Строгий разбор: либо содержимое, либо ВСЕ найденные придирки разом.
///
/// Разом — принципиально: редактор, который показывает по одной ошибке за проход,
/// заставляет сохранять по кругу ради следующей.
pub fn parse_canon(text: &str) -> Result<super::project::ListParts, Vec<CanonIssue>> {
    let value: serde_json::Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => {
            return Err(vec![CanonIssue {
                path: String::new(),
                code: IssueCode::Syntax,
                message: e.to_string(),
                line: e.line() as u32,
                column: e.column() as u32,
            }]);
        }
    };

    let mut issues = schema_issues(&value);
    issues.extend(step_title_issues(&value));
    if !issues.is_empty() {
        return Err(issues);
    }

    // Схема пройдена — значения нужного вида, и мягкий разбор уже ничего не потеряет.
    // Он же остаётся ЕДИНСТВЕННЫМ местом, где list.json превращается в шаги.
    let parsed = super::project::parse_list_value(value).ok_or_else(|| {
        vec![CanonIssue {
            path: String::new(),
            code: IssueCode::Schema,
            message: "list.json does not match the manifest shape".into(),
            line: 0,
            column: 0,
        }]
    })?;
    Ok(parsed)
}

/// Придирки схемы. Крейт отдаёт `instance_path` уже в виде JSON Pointer.
fn schema_issues(value: &serde_json::Value) -> Vec<CanonIssue> {
    let schema: serde_json::Value = match serde_json::from_str(SCHEMA) {
        Ok(s) => s,
        // Вшитая схема битой быть не может; если стала — это наш сбой, не читателя.
        Err(e) => {
            tracing::error!(error = %e, "canon: embedded schema is not valid JSON");
            return Vec::new();
        }
    };
    let validator = match jsonschema::draft7::new(&schema) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(error = %e, "canon: embedded schema does not compile");
            return Vec::new();
        }
    };
    validator
        .iter_errors(value)
        .map(|e| CanonIssue {
            path: e.instance_path().to_string(),
            code: IssueCode::Schema,
            message: e.to_string(),
            line: 0,
            column: 0,
        })
        .collect()
}

/// Шаг без заголовка схему проходит (title там — просто строка), а проекция такой
/// пункт ВЫБРАСЫВАЕТ. Для редактора это ошибка, иначе сохранение молча теряет строку.
fn step_title_issues(value: &serde_json::Value) -> Vec<CanonIssue> {
    let Some(steps) = value.get("steps").and_then(|s| s.as_array()) else {
        return Vec::new();
    };
    steps
        .iter()
        .enumerate()
        .filter_map(|(i, raw)| {
            let probe: StepTitleProbe = serde_json::from_value(raw.clone()).ok()?;
            let is_step = crate::blocks::is_step_type(probe.block_type.as_deref().unwrap_or_default());
            let empty = probe.title.as_deref().unwrap_or_default().trim().is_empty();
            (is_step && empty).then(|| CanonIssue {
                path: format!("/steps/{i}/title"),
                code: IssueCode::StepTitleRequired,
                message: "a step block requires a non-empty title".into(),
                line: 0,
                column: 0,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canon(steps: &str) -> String {
        format!(
            r#"{{"$schema":"https://setfork.com/schema/list.v1.json","title":"Т","desc":"","tags":[],"ordered":true,"version":1,"steps":[{steps}]}}"#
        )
    }

    fn step(title: &str) -> String {
        format!(
            r#"{{"n":1,"title":"{title}","desc":"","command":"","level":"required","why":"","section":"","subtasks":[],"refs":[]}}"#
        )
    }

    #[test]
    fn целый_канон_разбирается() {
        let parsed = parse_canon(&canon(&step("Шаг"))).unwrap_or_else(|e| panic!("{} придирок", e.len()));
        assert_eq!(parsed.title, "Т");
        assert_eq!(parsed.version, 1);
        assert_eq!(parsed.steps.len(), 1);
        assert_eq!(parsed.steps[0].title, "Шаг");
    }

    #[test]
    fn синтаксис_показывает_место_разрыва() {
        let Err(issues) = parse_canon("{\"title\": \n}") else { panic!("битый JSON обязан отвергнуться") };
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].code, IssueCode::Syntax);
        assert_eq!(issues[0].line, 2, "строка разрыва, а не начало файла");
    }

    #[test]
    fn шаг_без_заголовка_это_ошибка_а_не_тихая_потеря() {
        let Err(issues) = parse_canon(&canon(&step(""))) else { panic!("шаг без заголовка обязан отвергнуться") };
        assert_eq!(issues[0].code, IssueCode::StepTitleRequired);
        assert_eq!(issues[0].path, "/steps/0/title", "указатель на само поле");
    }

    #[test]
    fn презентационный_блок_живёт_без_заголовка() {
        // level ядро пишет ВСЕГДА, и у блока тоже: в БД это перечисление, пустым не бывает.
        let block = r#"{"n":1,"type":"text","content":{"md":"текст"},"title":"","desc":"","command":"","level":"required","why":"","section":"","subtasks":[],"refs":[]}"#;
        let parsed = parse_canon(&canon(block)).unwrap_or_else(|e| panic!("{} придирок", e.len()));
        assert_eq!(parsed.steps.len(), 1);
        assert_eq!(parsed.steps[0].block_type, "text");
    }

    #[test]
    fn придирки_приходят_разом() {
        // Посторонний ключ корня (схема) и шаг без заголовка (семантика) — оба сразу,
        // иначе правка идёт по кругу: сохранил → узнал следующую.
        let text = format!(
            r#"{{"$schema":"u","title":"Т","desc":"","tags":[],"ordered":true,"version":1,"lishnee":1,"steps":[{}]}}"#,
            step("")
        );
        let Err(issues) = parse_canon(&text) else { panic!("обе придирки обязаны прийти") };
        assert!(issues.iter().any(|i| i.code == IssueCode::Schema), "посторонний ключ");
        assert!(issues.iter().any(|i| i.code == IssueCode::StepTitleRequired), "шаг без заголовка");
    }

    #[test]
    fn чужое_значение_ловится_схемой_с_указателем() {
        let bad = r#"{"n":1,"title":"Ш","desc":"","command":"","level":"КАКОЙ-ТО","why":"","section":"","subtasks":[],"refs":[]}"#;
        let Err(issues) = parse_canon(&canon(bad)) else { panic!("level вне реестра обязан отвергнуться") };
        let it = issues.iter().find(|i| i.code == IssueCode::Schema).expect("придирка схемы");
        assert_eq!(it.path, "/steps/0/level");
    }
}
