//! Сериализация версии списка в файлы git-дерева (list.json, README.md,
//! steps/NN-slug.md) — байт-в-байт зеркало TS serialize.ts: golden-сверка
//! сравнивает выхлоп с inproc-реализацией фронта. Материализация в репо —
//! соседний модуль bundle (типы ре-экспортируются оттуда для старых путей).
use crate::blocks::is_step_type;

// Доменные структуры для сериализации версии в git-дерево (порт serialize.ts/bundle.ts).
pub struct StepRef {
    pub label: String,
    pub url: Option<String>,
}
pub struct SerStep {
    pub n: i32,
    // Блочная модель: None/Some("step") = шаг; иначе text/image. content — payload
    // не-step блока (Null у шага). type/content сериализуем ТОЛЬКО у не-step —
    // старые step-only списки дают байт-в-байт тот же list.json (golden с TS).
    pub block_type: Option<String>,
    pub content: serde_json::Value,
    // Стабильная идентичность блока сквозь версии. В list.json пишется ТОЛЬКО
    // когда есть: строки без block_id дают байт-в-байт прежний файл, поэтому
    // golden-фикстуры и паритет с TS не ломаются.
    pub block_id: Option<String>,
    pub title: String,
    pub desc: String,
    pub command: String,
    /// S3-ключ картинки шага (решение владельца, Ф2a-довесок): в канон идёт КЛЮЧ,
    /// а не подписанный URL — подпись imgproxy протухает при ротации ключей, и
    /// канон в git начал бы врать задним числом. Кликабельную ссылку даст README
    /// при пересборке (Ф2b). Пишется только при наличии (байты старых не меняются).
    pub image_key: Option<String>,
    pub level: String,
    /// «Здесь нужен человек» — продуктовая часть пункта (Ф2a-довесок).
    /// Пишется только при true; явный false в ФАЙЛЕ — способ снять пометку пушем
    /// (тристейт читает проекция, сериализация false не пишет).
    pub needs_human: bool,
    /// Вопрос к пометке (en-проекция); пишется только при needs_human и непустом.
    pub needs_human_ask: Option<String>,
    /// Разрушительный пункт: команда необратима. Пишется только при true —
    /// байты списков без пометки не меняются (та же дисциплина, что needs_human).
    pub danger: bool,
    pub why: String,
    pub section: String,
    pub subtasks: Vec<String>,
    pub refs: Vec<StepRef>,
}

/// Шаг-блок ли (у него собственные поля; у text/image — content).
fn is_step_block(s: &SerStep) -> bool {
    s.block_type.as_deref().is_none_or(is_step_type)
}
pub struct VersionData {
    pub version: i32,
    pub note: String,
    pub ts: i64, // unix epoch (для GIT_AUTHOR_DATE, UTC → "+0000")
    pub title: String,
    pub desc: String,
    pub tags: Vec<String>,
    pub ordered: bool,
    /// Тип списка (ADR-0010): procedure|inventory|checklist|criteria|options|recipe.
    /// None — не определён (старые списки): поле в канон не пишется вовсе, чтобы
    /// их байты не менялись без нужды (тот же приём, что blockId).
    pub kind: Option<String>,
    pub steps: Vec<SerStep>,
}

/// Допустимые значения `kind` — ЗЕРКАЛО setfork-frontend/src/shared/ai/list-kind.ts
/// (LIST_KINDS). Меняться обязаны парой: значение, которого нет здесь, проекция
/// молча отбросит (санитизация чужого git-входа), и тип потеряется.
pub const LIST_KINDS: [&str; 6] = ["procedure", "inventory", "checklist", "criteria", "options", "recipe"];

/// Валидное значение kind? (для санитизации проекции и валидации записи)
pub fn is_valid_kind(s: &str) -> bool {
    LIST_KINDS.contains(&s)
}

/// URL опубликованной JSON Schema манифеста — пишется в каждый list.json ключом
/// `$schema` (автодополнение в редакторе сразу после клона). ОТ ПЕРЕМЕННОЙ
/// (решение владельца 2026-07-30): у инстансов свои домены, и зашитый чужой
/// домен давал бы молча не работающее автодополнение из-за шейпинга. Внутри
/// инстанса URL стабилен — детерминизм SHA не страдает; golden фиксируют дефолт.
pub fn schema_url() -> &'static str {
    static URL: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    URL.get_or_init(|| {
        std::env::var("SETFORK_SCHEMA_URL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "https://setfork.com/schema/list.v1.json".to_string())
    })
}

/// list.json — машиночитаемый снимок версии (то, что парсит проекция при push).
/// Публична: ядро — единственный владелец формата, и запись канона в ветку
/// (write::commit_list_json) обязана идти ЧЕРЕЗ ЭТУ функцию, а не через свою
/// сериализацию на клиенте. Иначе правила формата живут в двух местах.
pub fn list_json(v: &VersionData) -> String {
    let steps: Vec<serde_json::Value> = v
        .steps
        .iter()
        .map(|s| {
            let refs: Vec<serde_json::Value> = s
                .refs
                .iter()
                .map(|r| {
                    let mut m = serde_json::Map::new();
                    m.insert("label".into(), serde_json::Value::String(r.label.clone()));
                    if let Some(u) = &r.url {
                        m.insert("url".into(), serde_json::Value::String(u.clone()));
                    }
                    serde_json::Value::Object(m)
                })
                .collect();
            // Порядок ключей строго как в TS (serialize.ts SerStep): для не-step
            // блоков type/content идут сразу после n; у шага их нет вовсе.
            let mut m = serde_json::Map::new();
            m.insert("n".into(), serde_json::json!(s.n));
            // Идентичность — сразу после n и только при наличии (порядок ключей = TS bundle.ts).
            if let Some(bid) = &s.block_id {
                m.insert("blockId".into(), serde_json::Value::String(bid.clone()));
            }
            if !is_step_block(s) {
                m.insert("type".into(), serde_json::Value::String(s.block_type.clone().unwrap_or_default()));
                m.insert("content".into(), s.content.clone());
            }
            m.insert("title".into(), serde_json::json!(s.title));
            m.insert("desc".into(), serde_json::json!(s.desc));
            m.insert("command".into(), serde_json::json!(s.command));
            // Ф2a-довесок: новые поля пишутся ТОЛЬКО при наличии — байты списков
            // без картинок/пометок не меняются (та же дисциплина, что blockId/kind).
            // Пустой ключ в файл не пишем: в каноне пустое значение ЗНАЧИТ «снять
            // картинку», и писать его у шага, где картинки и так нет, — писать шум.
            if let Some(ik) = s.image_key.as_deref().map(str::trim).filter(|k| !k.is_empty()) {
                m.insert("imageKey".into(), serde_json::Value::String(ik.to_string()));
            }
            m.insert("level".into(), serde_json::json!(s.level));
            if s.needs_human {
                m.insert("needsHuman".into(), serde_json::json!(true));
                if let Some(ask) = &s.needs_human_ask {
                    m.insert("needsHumanAsk".into(), serde_json::Value::String(ask.clone()));
                }
            }
            if s.danger {
                m.insert("danger".into(), serde_json::json!(true));
            }
            m.insert("why".into(), serde_json::json!(s.why));
            m.insert("section".into(), serde_json::json!(s.section));
            m.insert("subtasks".into(), serde_json::json!(s.subtasks));
            m.insert("refs".into(), serde_json::Value::Array(refs));
            serde_json::Value::Object(m)
        })
        .collect();
    // Порядок ключей корня зафиксирован осознанно (Ф2a): $schema первым
    // (конвенция редакторов), kind — рядом с ordered (свойство списка) и только
    // при наличии, version и steps замыкают. Меняется только сознательно —
    // это байты публичного контракта.
    let mut root = serde_json::Map::new();
    root.insert("$schema".into(), serde_json::json!(schema_url()));
    root.insert("title".into(), serde_json::json!(v.title));
    root.insert("desc".into(), serde_json::json!(v.desc));
    root.insert("tags".into(), serde_json::json!(v.tags));
    root.insert("ordered".into(), serde_json::json!(v.ordered));
    if let Some(k) = &v.kind {
        root.insert("kind".into(), serde_json::json!(k));
    }
    root.insert("version".into(), serde_json::json!(v.version));
    root.insert("steps".into(), serde_json::Value::Array(steps));
    let mut s = serde_json::to_string_pretty(&serde_json::Value::Object(root)).unwrap();
    s.push('\n');
    s
}

/// Ссылка → markdown-элемент (label или [label](url)).
fn ref_item(r: &StepRef) -> String {
    match &r.url {
        Some(u) => format!("[{}]({})", r.label, u),
        None => r.label.clone(),
    }
}

/// README.md — точный порт serialize.ts readme().
/// Блок кода, который НЕЛЬЗЯ закрыть изнутри.
///
/// Ограждение из трёх кавычек закрывалось содержимым: команду пишет автор списка,
/// и `` ``` `` внутри неё превращали остаток README в размеченный текст. По
/// CommonMark §4.5 закрывающее ограждение не короче открывающего — значит открываем
/// длиннее самой длинной цепочки кавычек внутри, и закрыть его содержимым нельзя.
///
/// Отступ получает КАЖДАЯ строка: раньше его видела только первая, и многострочная
/// команда со второй строки вываливалась из пункта списка.
fn code_block(code: &str, indent: &str, lang: &str) -> Vec<String> {
    let normalized = code.replace("\r\n", "\n").replace('\r', "\n");
    let mut longest = 0usize;
    let mut run = 0usize;
    for ch in normalized.chars() {
        if ch == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    let fence = "`".repeat(longest.max(2) + 1);
    let mut out = vec![format!("{}{}{}", indent, fence, lang)];
    // `split('\n')`, а НЕ `lines()`: последний отбрасывает завершающую пустую строку,
    // а TS-зеркало (`markdownCodeBlock`) её сохраняет. Реализации обязаны давать
    // одинаковые байты — на этом стоит golden-сверка.
    out.extend(normalized.split('\n').map(|l| format!("{}{}", indent, l)));
    out.push(format!("{}{}", indent, fence));
    out
}

fn readme(v: &VersionData) -> String {
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!("# {}", v.title));
    lines.push(String::new());
    if !v.desc.is_empty() {
        lines.push(v.desc.clone());
        lines.push(String::new());
    }
    if !v.tags.is_empty() {
        lines.push(v.tags.iter().map(|t| format!("`{}`", t)).collect::<Vec<_>>().join(" "));
        lines.push(String::new());
    }
    let kind = if v.ordered { "Ordered list" } else { "Unordered set" };
    // Счётчик «items» и нумерация — только по шаг-блокам (презентационные вне счёта).
    let step_count = v.steps.iter().filter(|s| is_step_block(s)).count();
    lines.push(format!("> {} · v{} · {} items", kind, v.version, step_count));
    lines.push(String::new());

    let mut section = String::new();
    let mut step_num = 0usize;
    for s in v.steps.iter() {
        if !is_step_block(s) {
            // Презентационные блоки — inline в README.
            match s.block_type.as_deref() {
                Some("text") => {
                    if let Some(md) = s.content.get("md").and_then(|v| v.as_str())
                        && !md.is_empty()
                    {
                        lines.push(String::new());
                        lines.push(md.to_string());
                        lines.push(String::new());
                    }
                }
                Some("image") => {
                    let r = s.content.get("ref").and_then(|v| v.as_str()).unwrap_or("");
                    if !r.is_empty() {
                        let cap = s.content.get("caption").and_then(|v| v.as_str()).unwrap_or("");
                        lines.push(String::new());
                        lines.push(format!("![{}]({})", cap, r));
                        lines.push(String::new());
                    }
                }
                _ => {}
            }
            continue;
        }
        if !s.section.is_empty() && s.section != section {
            section = s.section.clone();
            lines.push(String::new());
            lines.push(format!("## {}", section));
            lines.push(String::new());
        }
        step_num += 1;
        let marker = if v.ordered { format!("{}.", step_num) } else { "-".to_string() };
        let lvl = if !s.level.is_empty() && s.level != "required" {
            format!(" _({})_", s.level)
        } else {
            String::new()
        };
        lines.push(format!("{} **{}**{}", marker, s.title, lvl));
        // Продолжение пункта отступается ПО ДЛИНЕ МАРКЕРА, а не на фиксированные три
        // пробела: у пункта «10.» маркер уже четыре символа, и трёх пробелов мало —
        // по CommonMark содержимое перестаёт принадлежать пункту и выпадает из него.
        let indent = " ".repeat(marker.chars().count() + 1);
        if !s.desc.is_empty() {
            lines.push(format!("{}{}", indent, s.desc.replace('\n', &format!("\n{}", indent))));
        }
        if !s.command.is_empty() {
            lines.push(String::new());
            lines.extend(code_block(&s.command, &indent, "sh"));
        }
        if !s.why.is_empty() {
            lines.push(format!("{}> why: {}", indent, s.why));
        }
        for st in &s.subtasks {
            lines.push(format!("{}- [ ] {}", indent, st));
        }
        for r in &s.refs {
            lines.push(format!("{}- {}", indent, ref_item(r)));
        }
        lines.push(String::new());
    }

    // Подвал (Ф2b): README — генерируемая витрина, и это должно быть видно ДО
    // того, как правку в него потеряли. Канон — list.json, правят его.
    lines.push(String::new());
    lines.push("---".to_string());
    lines.push(String::new());
    lines.push(
        "> Generated by SetFork — the canonical source is [list.json](./list.json). \
         Edit list.json; manual changes to this file are overwritten on the next version."
            .to_string(),
    );

    let mut out = lines.join("\n");
    while out.contains("\n\n\n") {
        out = out.replace("\n\n\n", "\n\n");
    }
    format!("{}\n", out.trim_end())
}

/// .gitattributes дерева: форджа сворачивает витрину в диффах (README всё равно
/// рендерится на главной — linguist-generated влияет только на дифф и статистику).
pub const GITATTRIBUTES: &str = "README.md linguist-generated=true\n";

/// Разрешённый состав дерева (трек git-surface, Ф0). Основание — ADR-0014:
/// «в git уходит только то, что обязано пережить `git clone` и вернуться через
/// `git push`».
///
/// Приём не самодельный: у GitLab это штатные push rules — «Prohibited filenames»
/// (регексп по именам файлов) и «Maximum file size», проверяемые при пуше
/// (docs.gitlab.com/user/project/repository/push_rules). Разница лишь в том, что
/// у них это настройка проекта, а у нас формат дерева фиксирован, поэтому список
/// зашит.
///
/// До Ф0 правило было записано, но ничем не защищено: `pre-receive`
/// требовал `list.json` и не запрещал НИЧЕГО другого, поэтому в репозиторий
/// списка можно было залить произвольные файлы — проекция их игнорировала,
/// git хранил вечно, а зеркало вывозило на чужую форджу под именем владельца.
///
/// `steps/*.md` — ЛЕГАСИ-исключение, а не часть формата: Ф2b убрала их из
/// генерации, но у репо, в которые с тех пор не писали, они всё ещё лежат в
/// дереве. Жёсткий отказ ломал бы пуш из такого клона на ровном месте, поэтому
/// они принимаются молча-игнорируемыми (как и раньше) до первой записи, которая
/// перестроит дерево. Убрать исключение, когда легаси-деревьев не останется.
/// Предикат применяется к ПУТЯМ ЛИСТЬЕВ (файлы и гитлинки), не к каталогам:
/// каталог сам по себе ничего не хранит, и судить его отдельно значит либо
/// пропускать мусор, либо называть в отказе бесполезное `assets` вместо
/// `assets/x.png`. Поэтому голого `steps` здесь НЕТ — файл с таким именем в
/// корне обязан быть отвергнут, каталог `steps` проверяется по содержимому.
pub fn tree_path_allowed(path: &str) -> bool {
    matches!(path, "README.md" | "list.json" | ".gitattributes")
        || path
            .strip_prefix("steps/")
            .and_then(|name| name.strip_suffix(".md"))
            // Имя обязано быть НЕПУСТЫМ: `steps/.md` шелльное правило (`[^/]+`)
            // отвергает, а прежняя проверка здесь пропускала — две реализации
            // одного правила расходились, и мягче была наша (F7 линзы 02).
            .is_some_and(|stem| !stem.is_empty() && !stem.contains('/'))
}

/// Полный набор файлов версии (Ф2b): ровно ДВА представления — README.md
/// (витрина для человека) и list.json (канон для машины) + служебный
/// .gitattributes. steps/*.md удалены из формата: это было третье представление
/// того же контента, которое парсилось обратно лишь частично и два из трёх
/// путей записи штатно выбрасывали его из дерева.
pub fn version_files(v: &VersionData) -> Vec<(String, String)> {
    vec![
        ("README.md".to_string(), readme(v)),
        ("list.json".to_string(), list_json(v)),
        (".gitattributes".to_string(), GITATTRIBUTES.to_string()),
    ]
}

// Сообщение коммита: `git commit -m` добавляет завершающий \n — воспроизводим для SHA-идентичности.
pub(super) fn commit_message(v: &VersionData) -> String {
    let boilerplate = matches!(v.note.as_str(), "initial" | "edit" | "seeded");
    let msg = if !v.note.is_empty() && !boilerplate {
        format!("v{}: {}", v.version, v.note)
    } else {
        format!("v{}", v.version)
    };
    format!("{msg}\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(n: i32, title: &str) -> SerStep {
        SerStep {
            n,
            block_type: None,
            content: serde_json::Value::Null,
            block_id: None,
            title: title.into(),
            desc: String::new(),
            command: String::new(),
            image_key: None,
            level: "required".into(),
            needs_human: false,
            needs_human_ask: None,
            danger: false,
            why: String::new(),
            section: String::new(),
            subtasks: vec![],
            refs: vec![],
        }
    }
    fn block(n: i32, ty: &str, content: serde_json::Value) -> SerStep {
        SerStep { block_type: Some(ty.into()), content, ..step(n, "") }
    }
    fn ver(steps: Vec<SerStep>) -> VersionData {
        VersionData {
            version: 3,
            note: "add caching".into(),
            ts: 0,
            title: "Redis Caching".into(),
            desc: "Set up".into(),
            tags: vec!["redis".into()],
            ordered: true,
            kind: None,
            steps,
        }
    }

    // Идентичность блока в list.json: пишется только когда есть, и стоит сразу
    // после n. Условная запись — то, что сохраняет байт-в-байт паритет с TS на
    // старых данных (у них block_id пуст) и не ломает golden-фикстуры.
    #[test]
    fn list_json_carries_block_id_only_when_present() {
        let without = version_files(&ver(vec![step(1, "Install Redis")]));
        let json_without = &without.iter().find(|(p, _)| p == "list.json").unwrap().1;
        assert!(!json_without.contains("blockId"), "у шага без идентичности поля быть не должно");

        let mut s = step(1, "Install Redis");
        s.block_id = Some("11111111-2222-3333-4444-555555555555".into());
        let with = version_files(&ver(vec![s]));
        let json_with = &with.iter().find(|(p, _)| p == "list.json").unwrap().1;
        assert!(json_with.contains("\"blockId\": \"11111111-2222-3333-4444-555555555555\""));
        // Порядок ключей = TS: n, затем blockId, затем title — сравниваем ВНУТРИ
        // массива шагов (на верхнем уровне list.json свой "title", он идёт раньше).
        let steps_at = json_with.find("\"steps\"").unwrap();
        let in_steps = &json_with[steps_at..];
        let n_at = in_steps.find("\"n\"").unwrap();
        let bid_at = in_steps.find("\"blockId\"").unwrap();
        let title_at = in_steps.find("\"title\"").unwrap();
        assert!(n_at < bid_at && bid_at < title_at, "blockId должен идти между n и title");
    }

    #[test]
    fn version_files_paths() {
        // Ф2b: ровно два представления + служебный .gitattributes, steps/ нет.
        let files = version_files(&ver(vec![step(1, "Install Redis"), step(2, "Configure")]));
        let paths: Vec<_> = files.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(paths, vec!["README.md", "list.json", ".gitattributes"]);
        let ga = &files.iter().find(|(p, _)| p == ".gitattributes").unwrap().1;
        assert!(ga.contains("README.md linguist-generated=true"), "витрина свёрнута в диффах форджи");
    }

    #[test]
    fn list_json_key_order_and_trailing_newline() {
        let files = version_files(&ver(vec![step(1, "Install Redis")]));
        let lj = &files.iter().find(|(p, _)| p == "list.json").unwrap().1;
        // Ф2a: $schema стал первым ключом (осознанное изменение формата), title — за ним.
        assert!(lj.starts_with("{\n  \"$schema\": "), "key order $schema-first: {lj}");
        assert!(lj.contains("\n  \"title\": \"Redis Caching\","), "title сразу после $schema");
        assert!(lj.ends_with('\n'));
    }

    #[test]
    fn step_only_list_json_has_no_type_or_content() {
        // Байт-совместимость с TS: у шага type/content НЕ сериализуются.
        let files = version_files(&ver(vec![step(1, "Install Redis")]));
        let lj = &files.iter().find(|(p, _)| p == "list.json").unwrap().1;
        assert!(!lj.contains("\"type\""), "no type key for step");
        assert!(!lj.contains("\"content\""), "no content key for step");
    }

    /// Регрессия: команду пишет автор списка, и она не имеет права закрыть
    /// ограждение — иначе остаток README перестаёт быть кодом (карточка export/001
    /// во фронте; здесь тот же README собирает ядро).
    #[test]
    fn command_cannot_close_the_code_fence() {
        let mut s = step(1, "Run");
        s.command = "echo ok\n```\n<img src=x onerror=alert(1)>".into();
        let files = version_files(&ver(vec![s]));
        let readme = &files.iter().find(|(p, _)| p == "README.md").unwrap().1;

        let lines: Vec<&str> = readme.lines().collect();
        let open = lines.iter().position(|l| l.trim().starts_with("```")).unwrap();
        let fence_len = lines[open].trim().chars().take_while(|c| *c == '`').count();
        assert!(fence_len > 3, "ограждение длиннее содержимого: {}", lines[open]);

        // Закрывающее — первая последующая строка ТОЛЬКО из кавычек нужной длины.
        let close = open
            + 1
            + lines[open + 1..]
                .iter()
                .position(|l| {
                    let t = l.trim();
                    !t.is_empty() && t.chars().all(|c| c == '`') && t.len() >= fence_len
                })
                .unwrap();
        let inside = &lines[open + 1..close];
        assert!(inside.iter().any(|l| l.contains("<img src=x onerror=alert(1)>")));
        assert!(inside.iter().any(|l| l.trim() == "```"));
    }

    /// Команда с завершающим переводом строки даёт те же байты, что TS-зеркало:
    /// `lines()` съедал бы пустую хвостовую строку, а `split` — нет.
    #[test]
    fn trailing_newline_in_command_is_preserved() {
        let mut s = step(1, "Run");
        s.command = "make all
"
        .into();
        let files = version_files(&ver(vec![s]));
        let readme = &files.iter().find(|(p, _)| p == "README.md").unwrap().1;
        assert!(
            readme.contains(
                "   make all
   
   ```"
            ),
            "{}",
            readme
        );
    }

    /// У пункта «10.» маркер длиннее, и трёх пробелов продолжению уже не хватает.
    #[test]
    fn tenth_item_keeps_its_body() {
        let steps: Vec<SerStep> = (1..=10)
            .map(|n| {
                let mut s = step(n, &format!("S{}", n));
                s.command = "make all".into();
                s
            })
            .collect();
        let files = version_files(&ver(steps));
        let readme = &files.iter().find(|(p, _)| p == "README.md").unwrap().1;
        let lines: Vec<&str> = readme.lines().collect();
        let head = lines.iter().position(|l| l.starts_with("10. ")).unwrap();
        let body = lines[head + 1..].iter().find(|l| !l.trim().is_empty()).unwrap();
        assert!(body.starts_with("    "), "тело десятого пункта: {:?}", body);
    }

    /// Многострочная команда обязана целиком остаться в отступе пункта.
    #[test]
    fn multiline_command_keeps_list_indent() {
        let mut s = step(1, "Run");
        s.command = "cd /tmp\nmake all".into();
        let files = version_files(&ver(vec![s]));
        let readme = &files.iter().find(|(p, _)| p == "README.md").unwrap().1;
        assert!(readme.contains("   cd /tmp"), "{}", readme);
        assert!(readme.contains("   make all"), "{}", readme);
    }

    #[test]
    fn non_step_blocks_serialize_and_get_no_md() {
        let v = ver(vec![
            step(1, "First"),
            block(2, "text", serde_json::json!({ "md": "Some **intro**" })),
            block(3, "image", serde_json::json!({ "ref": "img/abc", "caption": "Diagram" })),
            step(4, "Second"),
        ]);
        let files = version_files(&v);
        // README: text инлайн, image как ![], счётчик и нумерация только по шагам
        let readme = &files.iter().find(|(p, _)| p == "README.md").unwrap().1;
        assert!(readme.contains("Some **intro**"));
        assert!(readme.contains("![Diagram](img/abc)"));
        assert!(readme.contains("· 2 items"));
        assert!(readme.contains("1. **First**"));
        assert!(readme.contains("2. **Second**"));
        // list.json: type/content у не-step блоков, ключи после n
        let lj = &files.iter().find(|(p, _)| p == "list.json").unwrap().1;
        assert!(lj.contains("\"type\": \"text\""));
        assert!(lj.contains("\"md\": \"Some **intro**\""));
        assert!(lj.contains("\"type\": \"image\""));
    }

    // ── Перенос покрытия из TS перед удалением второй реализации (Ф0b) ──────
    // Эти случаи проверялись ТОЛЬКО в serialize.test.ts фронта. Удалить его,
    // не перенеся их, значило бы молча потерять проверки формата: снаружи это
    // выглядит как «тесты дублировались», а на деле дублировались не все.

    /// README — витрина списка: заголовок, теги, строка-маркер вида списка.
    #[test]
    fn readme_carries_title_tags_and_ordered_marker() {
        let files = version_files(&ver(vec![step(1, "Install Redis")]));
        let readme = &files.iter().find(|(p, _)| p == "README.md").unwrap().1;
        assert!(readme.contains("# Redis Caching"), "заголовок: {readme}");
        assert!(readme.contains("`redis`"), "теги в обратных кавычках: {readme}");
        assert!(readme.contains("> Ordered list · v3 · 1 items"), "маркер вида/версии: {readme}");
        assert!(readme.ends_with('\n'));

        // Неупорядоченный список маркируется иначе — иначе смысл списка врёт.
        let mut v = ver(vec![step(1, "Install Redis")]);
        v.ordered = false;
        let files = version_files(&v);
        let readme = &files.iter().find(|(p, _)| p == "README.md").unwrap().1;
        assert!(readme.contains("> Unordered set · v3 · 1 items"), "{readme}");
    }

    /// Явный type="step" и отсутствующий type — одно и то же. Разойдись они,
    /// один и тот же список давал бы разные файлы в зависимости от того, чем
    /// он записан (проекция пуша ставит '' , БД хранит 'step').
    #[test]
    fn explicit_step_type_is_identical_to_absent_type() {
        let with_type = version_files(&ver(vec![SerStep {
            block_type: Some("step".into()),
            ..step(1, "Install Redis")
        }]));
        let without = version_files(&ver(vec![step(1, "Install Redis")]));
        let paths = |f: &Vec<(String, String)>| f.iter().map(|(p, _)| p.clone()).collect::<Vec<_>>();
        assert_eq!(paths(&with_type), paths(&without), "набор файлов не зависит от формы type");
        let readme = |f: &Vec<(String, String)>| f.iter().find(|(p, _)| p == "README.md").unwrap().1.clone();
        assert_eq!(readme(&with_type), readme(&without), "README не зависит от формы type");
    }

    // ── Ф2a: манифест — версионированный публичный контракт ─────────────────

    /// $schema пишется ВСЕГДА и первым ключом (конвенция редакторов): это и есть
    /// автодополнение сразу после git clone.
    #[test]
    fn canon_starts_with_the_schema_link() {
        let lj = list_json(&ver(vec![step(1, "x")]));
        let first_line = lj.lines().nth(1).expect("вторая строка");
        assert!(
            first_line.trim_start().starts_with("\"$schema\": "),
            "$schema — первый ключ корня: {first_line}"
        );
        assert!(lj.contains(schema_url()), "URL из schema_url(): {lj}");
    }

    /// kind: пишется между ordered и version ТОЛЬКО при наличии — старые списки
    /// (kind не определён) дают байт-в-байт прежний файл (как blockId).
    #[test]
    fn kind_is_written_only_when_present_and_in_place() {
        let mut v = ver(vec![step(1, "x")]);
        let without = list_json(&v);
        assert!(!without.contains("\"kind\""), "без kind поля нет: {without}");

        v.kind = Some("recipe".into());
        let with = list_json(&v);
        let idx_ordered = with.find("\"ordered\"").expect("ordered");
        let idx_kind = with.find("\"kind\": \"recipe\"").expect("kind в каноне");
        let idx_version = with.find("\"version\"").expect("version");
        assert!(idx_ordered < idx_kind && idx_kind < idx_version, "порядок ключей: ordered < kind < version");
        // README (витрина) kind не несёт — это Ф2b-зона, не трогаем.
        let readme = version_files(&v).into_iter().find(|(p, _)| p == "README.md").unwrap().1;
        assert!(!readme.contains("recipe"), "kind не течёт в README: {readme}");
    }

    /// Реестр kind — зеркало LIST_KINDS фронта; валидатор ровно по нему.
    #[test]
    fn kind_registry_matches_the_validator() {
        for k in LIST_KINDS {
            assert!(is_valid_kind(k), "{k}");
        }
        for bad in ["", "step", "Recipe", "процедура", "list"] {
            assert!(!is_valid_kind(bad), "{bad:?}");
        }
    }

    #[test]
    fn commit_message_strips_boilerplate_and_appends_newline() {
        let mut v = ver(vec![step(1, "x")]);
        assert_eq!(commit_message(&v), "v3: add caching\n");
        v.note = "edit".into(); // boilerplate → без note
        assert_eq!(commit_message(&v), "v3\n");
        v.note = String::new();
        assert_eq!(commit_message(&v), "v3\n");
    }
}
