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
#[derive(Debug)]
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
    /// Ссылка без адреса И без подписи: проекция молча выбросила бы её. Ссылка одним
    /// адресом законна (#148) — придирки к ней нет.
    RefEmpty,
}

impl IssueCode {
    /// Код на проводе — то, по чему вызывающий выбирает текст в своём словаре.
    pub fn as_str(self) -> &'static str {
        match self {
            IssueCode::Syntax => "syntax",
            IssueCode::Schema => "schema",
            IssueCode::StepTitleRequired => "step_title_required",
            IssueCode::RefEmpty => "ref_empty",
        }
    }
}

/// Схема — ровно тот файл, что опубликован по `serialize::schema_url()`. Вшита в
/// бинарь: правила формата обязаны ехать вместе с кодом, который его пишет.
const SCHEMA: &str = include_str!("../../schema/list.v1.json");

/// Разбор ровно тех полей, по которым мягкий парс МОЛЧА выбрасывает данные.
#[derive(Deserialize)]
struct LossProbe {
    #[serde(rename = "type")]
    block_type: Option<String>,
    title: Option<String>,
    refs: Option<Vec<RefProbe>>,
}

#[derive(Deserialize)]
struct RefProbe {
    label: Option<String>,
    url: Option<String>,
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

    // Пустую ссылку ловят И схема (anyOf «адрес или подпись», #148), И смысловая
    // проверка. Автору — одна понятная придирка ref_empty, а не две о том же: гасим
    // ТОЛЬКО нарушение anyOf ровно по адресу этой ссылки. Остальные придирки схемы к
    // той же ссылке (лишний ключ, формат адреса) остаются — разбор отдаёт ВСЕ придирки
    // разом (находка Codex на #149: гашение по пути прятало их до следующего прохода).
    let loss = silent_loss_issues(&value);
    let empty_ref = |p: &str| loss.iter().any(|l| l.code == IssueCode::RefEmpty && p == l.path);
    let mut issues: Vec<CanonIssue> = schema_issues(&value)
        .into_iter()
        .filter(|(s, any_of)| !(*any_of && empty_ref(&s.path)))
        .map(|(s, _)| s)
        .collect();
    issues.extend(loss);
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

/// Придирки схемы. Крейт отдаёт `instance_path` уже в виде JSON Pointer. Второе
/// значение — нарушено ли `anyOf`: у ссылки это правило «адрес или подпись», его
/// дубль с понятной `ref_empty` гасит `parse_canon`.
fn schema_issues(value: &serde_json::Value) -> Vec<(CanonIssue, bool)> {
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
        .map(|e| {
            let any_of = matches!(e.kind(), jsonschema::error::ValidationErrorKind::AnyOf { .. });
            let issue = CanonIssue {
                path: e.instance_path().to_string(),
                code: IssueCode::Schema,
                message: e.to_string(),
                line: 0,
                column: 0,
            };
            (issue, any_of)
        })
        .collect()
}

/// Места, где мягкий парс МОЛЧА выбрасывает данные, а схема пропускает: пустая
/// строка — законная строка, и остановить её может только смысл, а не тип.
///
/// Таких мест ровно столько, сколько отбрасываний в `parse_steps`. Появится новое —
/// сюда обязана приехать и придирка, иначе редактор снова начнёт молча терять ввод.
fn silent_loss_issues(value: &serde_json::Value) -> Vec<CanonIssue> {
    let Some(steps) = value.get("steps").and_then(|s| s.as_array()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (i, raw) in steps.iter().enumerate() {
        let Ok(probe) = serde_json::from_value::<LossProbe>(raw.clone()) else { continue };
        let blank = |v: &Option<String>| v.as_deref().unwrap_or_default().trim().is_empty();

        // Шаг без заголовка проекция выбрасывает целиком.
        if crate::blocks::is_step_type(probe.block_type.as_deref().unwrap_or_default()) && blank(&probe.title)
        {
            out.push(CanonIssue {
                path: format!("/steps/{i}/title"),
                code: IssueCode::StepTitleRequired,
                message: "a step block requires a non-empty title".into(),
                line: 0,
                column: 0,
            });
        }
        // Ссылка без адреса И без подписи — выбрасывается целиком (#148). Одним адресом
        // ссылка законна: интерфейс показывает её доменом, и разбор её сохраняет.
        for (k, r) in probe.refs.unwrap_or_default().iter().enumerate() {
            if !crate::git::serialize::keeps_ref(r.label.as_deref().unwrap_or(""), r.url.as_deref()) {
                out.push(CanonIssue {
                    path: format!("/steps/{i}/refs/{k}"),
                    code: IssueCode::RefEmpty,
                    message: "a reference requires a url or a label".into(),
                    line: 0,
                    column: 0,
                });
            }
        }
    }
    out
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
    fn whole_canon_parses() {
        let parsed = parse_canon(&canon(&step("Шаг"))).unwrap_or_else(|e| panic!("{} придирок", e.len()));
        assert_eq!(parsed.title, "Т");
        assert_eq!(parsed.version, 1);
        assert_eq!(parsed.steps.len(), 1);
        assert_eq!(parsed.steps[0].title, "Шаг");
    }

    #[test]
    fn syntax_error_points_at_the_break() {
        let Err(issues) = parse_canon("{\"title\": \n}") else {
            panic!("битый JSON обязан отвергнуться")
        };
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].code, IssueCode::Syntax);
        assert_eq!(issues[0].line, 2, "строка разрыва, а не начало файла");
    }

    #[test]
    fn step_without_title_is_an_error_not_silent_loss() {
        let Err(issues) = parse_canon(&canon(&step(""))) else {
            panic!("шаг без заголовка обязан отвергнуться")
        };
        assert_eq!(issues[0].code, IssueCode::StepTitleRequired);
        assert_eq!(issues[0].path, "/steps/0/title", "указатель на само поле");
    }

    #[test]
    fn presentational_block_needs_no_title() {
        // level ядро пишет ВСЕГДА, и у блока тоже: в БД это перечисление, пустым не бывает.
        let block = r#"{"n":1,"type":"text","content":{"md":"текст"},"title":"","desc":"","command":"","level":"required","why":"","section":"","subtasks":[],"refs":[]}"#;
        let parsed = parse_canon(&canon(block)).unwrap_or_else(|e| panic!("{} придирок", e.len()));
        assert_eq!(parsed.steps.len(), 1);
        assert_eq!(parsed.steps[0].block_type, "text");
    }

    #[test]
    fn all_complaints_arrive_at_once() {
        // Посторонний ключ корня (схема) и шаг без заголовка (семантика) — оба сразу,
        // иначе правка идёт по кругу: сохранил → узнал следующую.
        let text = format!(
            r#"{{"$schema":"u","title":"Т","desc":"","tags":[],"ordered":true,"version":1,"lishnee":1,"steps":[{}]}}"#,
            step("")
        );
        let Err(issues) = parse_canon(&text) else {
            panic!("обе придирки обязаны прийти")
        };
        assert!(issues.iter().any(|i| i.code == IssueCode::Schema), "посторонний ключ");
        assert!(issues.iter().any(|i| i.code == IssueCode::StepTitleRequired), "шаг без заголовка");
    }

    /// Ссылка одним адресом — законна (#148): интерфейс показывает её доменом, и фронт
    /// хранит такие ссылки. Раньше разбор отвергал её, а мягкий парс выбрасывал молча.
    #[test]
    fn url_only_ref_is_kept_with_or_without_label_key() {
        for r in [r#"{"label":"   ","url":"https://example.com"}"#, r#"{"url":"https://example.com"}"#] {
            let s = format!(
                r#"{{"n":1,"title":"Ш","desc":"","command":"","level":"required","why":"","section":"","subtasks":[],"refs":[{r}]}}"#
            );
            let parts = parse_canon(&canon(&s))
                .unwrap_or_else(|e| panic!("ссылка одним адресом отвергнута: {r} → {e:?}"));
            let refs = &parts.steps[0].refs;
            assert_eq!(refs.len(), 1, "ссылка одним адресом пропала: {r}");
            assert_eq!(refs[0].url.as_deref(), Some("https://example.com"));
        }
    }

    /// Ссылка без адреса И без подписи — не ссылка. Мягкий парс её выбрасывает, значит
    /// разбор обязан сказать об этом, а не промолчать.
    #[test]
    fn empty_ref_is_an_error_not_silent_loss() {
        let s = r#"{"n":1,"title":"Ш","desc":"","command":"","level":"required","why":"","section":"","subtasks":[],"refs":[{"label":"   "}]}"#;
        let Err(issues) = parse_canon(&canon(s)) else {
            panic!("пустая ссылка обязана отвергнуться")
        };
        let it = issues.iter().find(|i| i.code == IssueCode::RefEmpty).expect("придирка");
        assert_eq!(it.path, "/steps/0/refs/0");
        // Схема ловит ту же ссылку (anyOf), но автору — ОДНА понятная придирка.
        assert_eq!(issues.len(), 1, "дубль придирки схемы: {issues:?}");
    }

    /// У пустой ссылки гасится только дубль «адрес или подпись», а чужие придирки к
    /// ней остаются: разбор отдаёт все придирки за один проход (находка Codex на #149).
    #[test]
    fn empty_ref_keeps_unrelated_schema_complaints() {
        let s = r#"{"n":1,"title":"Ш","desc":"","command":"","level":"required","why":"","section":"","subtasks":[],"refs":[{"label":" ","unexpected":true}]}"#;
        let Err(issues) = parse_canon(&canon(s)) else { panic!("обязана отвергнуться") };
        assert!(issues.iter().any(|i| i.code == IssueCode::RefEmpty), "ref_empty: {issues:?}");
        assert!(
            issues.iter().any(|i| i.code == IssueCode::Schema && i.message.contains("unexpected")),
            "придирка к лишнему ключу потерялась: {issues:?}"
        );
    }

    /// Опубликованная схема сама выражает правило keeps_ref (#148): сторонний валидатор
    /// по ней отвергает то же, что ядро, и принимает то же.
    #[test]
    fn published_schema_matches_keeps_ref() {
        let issues_for = |r: &str| {
            let s = format!(
                r#"{{"n":1,"title":"Ш","desc":"","command":"","level":"required","why":"","section":"","subtasks":[],"refs":[{r}]}}"#
            );
            let v: serde_json::Value = serde_json::from_str(&canon(&s)).unwrap();
            schema_issues(&v)
        };
        for ok in [
            r#"{"url":"https://example.com"}"#,
            r#"{"label":"  ","url":"https://example.com"}"#,
            r#"{"label":"док"}"#,
        ] {
            assert!(issues_for(ok).is_empty(), "схема отвергла законную ссылку {ok}: {:?}", issues_for(ok));
        }
        for bad in [r#"{}"#, r#"{"label":"   "}"#, r#"{"url":""}"#] {
            assert!(!issues_for(bad).is_empty(), "схема приняла пустую ссылку {bad}");
        }
    }

    #[test]
    fn foreign_value_is_caught_by_schema_with_pointer() {
        let bad = r#"{"n":1,"title":"Ш","desc":"","command":"","level":"КАКОЙ-ТО","why":"","section":"","subtasks":[],"refs":[]}"#;
        let Err(issues) = parse_canon(&canon(bad)) else {
            panic!("level вне реестра обязан отвергнуться")
        };
        let it = issues.iter().find(|i| i.code == IssueCode::Schema).expect("придирка схемы");
        assert_eq!(it.path, "/steps/0/level");
    }
}
