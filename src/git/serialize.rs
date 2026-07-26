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
    pub level: String,
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
    pub steps: Vec<SerStep>,
}

/// list.json — машиночитаемый снимок версии (то, что парсит проекция при push).
fn list_json(v: &VersionData) -> String {
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
            m.insert("level".into(), serde_json::json!(s.level));
            m.insert("why".into(), serde_json::json!(s.why));
            m.insert("section".into(), serde_json::json!(s.section));
            m.insert("subtasks".into(), serde_json::json!(s.subtasks));
            m.insert("refs".into(), serde_json::Value::Array(refs));
            serde_json::Value::Object(m)
        })
        .collect();
    let root = serde_json::json!({
        "title": v.title,
        "desc": v.desc,
        "tags": v.tags,
        "ordered": v.ordered,
        "version": v.version,
        "steps": steps,
    });
    let mut s = serde_json::to_string_pretty(&root).unwrap();
    s.push('\n');
    s
}

/// JSON-строка (как JSON.stringify(s)) — для front-matter.
fn json_str(s: &str) -> String {
    serde_json::to_string(s).unwrap()
}

/// Ссылка → markdown-элемент (label или [label](url)).
fn ref_item(r: &StepRef) -> String {
    match &r.url {
        Some(u) => format!("[{}]({})", r.label, u),
        None => r.label.clone(),
    }
}

/// README.md — точный порт serialize.ts readme().
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
        if !s.desc.is_empty() {
            lines.push(format!("   {}", s.desc.replace('\n', "\n   ")));
        }
        if !s.command.is_empty() {
            lines.push(String::new());
            lines.push("   ```sh".to_string());
            lines.push(format!("   {}", s.command));
            lines.push("   ```".to_string());
        }
        if !s.why.is_empty() {
            lines.push(format!("   > why: {}", s.why));
        }
        for st in &s.subtasks {
            lines.push(format!("   - [ ] {}", st));
        }
        for r in &s.refs {
            lines.push(format!("   - {}", ref_item(r)));
        }
        lines.push(String::new());
    }

    let mut out = lines.join("\n");
    while out.contains("\n\n\n") {
        out = out.replace("\n\n\n", "\n\n");
    }
    format!("{}\n", out.trim_end())
}

fn pad(n: i32, width: usize) -> String {
    format!("{:0>width$}", n.to_string(), width = width)
}

/// slugifyStep из serialize.ts: [^a-z0-9а-я]+ → '-', trim '-', 40 симв., default 'step'.
fn slugify_step(s: &str) -> String {
    let lower = s.to_lowercase();
    let mut out = String::new();
    let mut prev_dash = false;
    for c in lower.chars() {
        let keep = c.is_ascii_lowercase() || c.is_ascii_digit() || ('а'..='я').contains(&c);
        if keep {
            out.push(c);
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    let sliced: String = out.trim_matches('-').chars().take(40).collect();
    if sliced.is_empty() { "step".to_string() } else { sliced }
}

/// steps/NN-slug.md — точный порт serialize.ts stepFile().
fn step_file(s: &SerStep, width: usize) -> (String, String) {
    let mut front: Vec<String> =
        vec!["---".to_string(), format!("title: {}", json_str(&s.title)), format!("level: {}", s.level)];
    if !s.section.is_empty() {
        front.push(format!("section: {}", json_str(&s.section)));
    }
    if !s.command.is_empty() {
        front.push(format!("command: {}", json_str(&s.command)));
    }
    front.push("---".to_string());
    front.push(String::new());

    let mut body: Vec<String> = Vec::new();
    if !s.desc.is_empty() {
        body.push(s.desc.clone());
        body.push(String::new());
    }
    if !s.why.is_empty() {
        body.push(format!("**Why:** {}", s.why));
        body.push(String::new());
    }
    if !s.subtasks.is_empty() {
        for st in &s.subtasks {
            body.push(format!("- [ ] {}", st));
        }
        body.push(String::new());
    }
    if !s.refs.is_empty() {
        for r in &s.refs {
            body.push(format!("- {}", ref_item(r)));
        }
        body.push(String::new());
    }

    let content = format!("{}{}\n", front.join("\n"), body.join("\n").trim_end());
    let path = format!("steps/{}-{}.md", pad(s.n, width), slugify_step(&s.title));
    (path, content)
}

/// Полный набор файлов версии: list.json, README.md и steps/NN-slug.md
/// (то, что кладётся в дерево коммита vN).
pub fn version_files(v: &VersionData) -> Vec<(String, String)> {
    let width = std::cmp::max(2, v.steps.len().to_string().len());
    let mut files = vec![("README.md".to_string(), readme(v)), ("list.json".to_string(), list_json(v))];
    // .md пишем ТОЛЬКО шаг-блокам; text/image живут в README + list.json.
    for s in &v.steps {
        if is_step_block(s) {
            files.push(step_file(s, width));
        }
    }
    files
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
            level: "required".into(),
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
        let files = version_files(&ver(vec![step(1, "Install Redis"), step(2, "Configure")]));
        let paths: Vec<_> = files.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(
            paths,
            vec!["README.md", "list.json", "steps/01-install-redis.md", "steps/02-configure.md"]
        );
    }

    #[test]
    fn list_json_key_order_and_trailing_newline() {
        let files = version_files(&ver(vec![step(1, "Install Redis")]));
        let lj = &files.iter().find(|(p, _)| p == "list.json").unwrap().1;
        assert!(lj.starts_with("{\n  \"title\": \"Redis Caching\","), "key order title-first");
        assert!(lj.ends_with('\n'));
    }

    #[test]
    fn slugify_matches_ts() {
        assert_eq!(slugify_step("Install Redis!!!"), "install-redis");
        assert_eq!(slugify_step("Установка Redis"), "установка-redis");
        assert_eq!(slugify_step("@#$%^&*()"), "step");
        assert_eq!(slugify_step(&"a".repeat(60)), "a".repeat(40));
    }

    #[test]
    fn pad_widths() {
        assert_eq!(pad(7, 2), "07");
        assert_eq!(pad(1, 3), "001");
        assert_eq!(pad(100, 2), "100");
    }

    #[test]
    fn step_only_list_json_has_no_type_or_content() {
        // Байт-совместимость с TS: у шага type/content НЕ сериализуются.
        let files = version_files(&ver(vec![step(1, "Install Redis")]));
        let lj = &files.iter().find(|(p, _)| p == "list.json").unwrap().1;
        assert!(!lj.contains("\"type\""), "no type key for step");
        assert!(!lj.contains("\"content\""), "no content key for step");
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
        // .md только у 2 шагов
        let md_paths: Vec<_> =
            files.iter().map(|(p, _)| p.as_str()).filter(|p| p.starts_with("steps/")).collect();
        assert_eq!(md_paths, vec!["steps/01-first.md", "steps/04-second.md"]);
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
