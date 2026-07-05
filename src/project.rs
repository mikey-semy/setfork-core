use crate::db;
use serde::Deserialize;
use sqlx::PgPool;
use std::collections::HashMap;
use std::path::Path;
use uuid::Uuid;

// Спроецированный шаг (строки; LocaleText соберётся в db::add_version как {"en": …}).
pub struct ProjRef {
    pub label: String,
    pub url: Option<String>,
}
pub struct ProjStep {
    pub title: String,
    pub desc: String,
    pub command: String,
    pub level: String,
    pub why: String,
    pub section: String,
    pub subtasks: Vec<String>,
    pub refs: Vec<ProjRef>,
}

// Мягкий парс list.json (все поля optional) — порт ParsedList из project.ts.
#[derive(Deserialize)]
struct RawRef {
    label: Option<String>,
    url: Option<String>,
}
#[derive(Deserialize)]
struct RawStep {
    title: Option<String>,
    desc: Option<String>,
    command: Option<String>,
    level: Option<String>,
    why: Option<String>,
    section: Option<String>,
    subtasks: Option<Vec<String>>,
    refs: Option<Vec<RawRef>>,
}
#[derive(Deserialize)]
struct RawList {
    title: Option<String>,
    desc: Option<String>,
    tags: Option<Vec<String>>,
    ordered: Option<bool>,
    steps: Option<Vec<RawStep>>,
}

// Читает tip main через git2: (hex tip, содержимое list.json, subject коммита, файлы steps/*.md).
// Все git2-объекты — не Send, поэтому извлекаем owned-данные ДО любого await.
// steps: map "NN" (базовое имя без slug/расширения → 1-based индекс) → содержимое .md.
fn read_tip(bare: &Path) -> Option<(String, Vec<u8>, String, HashMap<i32, String>)> {
    read_ref_tip(bare, "refs/heads/main")
}

// То же для произвольного ref (ветки) — база просмотра списка «на ветке».
pub fn read_ref_tip(bare: &Path, refname: &str) -> Option<(String, Vec<u8>, String, HashMap<i32, String>)> {
    let repo = git2::Repository::open_bare(bare).ok()?;
    let tip = repo.refname_to_id(refname).ok()?;
    let commit = repo.find_commit(tip).ok()?;
    let tree = commit.tree().ok()?;
    let entry = tree.get_path(Path::new("list.json")).ok()?;
    let blob = entry.to_object(&repo).ok()?;
    let raw = blob.as_blob()?.content().to_vec();
    let subject = commit.summary().unwrap_or("").to_string();
    let steps = read_step_files(&repo, &tree);
    Some((tip.to_string(), raw, subject, steps))
}

// Собирает steps/NN-*.md из дерева: ключ — префикс NN (число до первого '-'), значение — контент.
// Индекс парсим из имени файла (как его пишет version_files через pad(n, width)).
fn read_step_files(repo: &git2::Repository, tree: &git2::Tree) -> HashMap<i32, String> {
    let mut out = HashMap::new();
    let steps_tree = match tree.get_path(Path::new("steps")).ok().and_then(|e| e.to_object(repo).ok()) {
        Some(obj) => match obj.into_tree() {
            Ok(t) => t,
            Err(_) => return out,
        },
        None => return out,
    };
    for e in steps_tree.iter() {
        let name = match e.name() {
            Some(n) => n,
            None => continue,
        };
        if !name.ends_with(".md") {
            continue;
        }
        // NN — цифры до первого '-' (или до '.md', если slug пуст).
        let digits: String = name.chars().take_while(|c| c.is_ascii_digit()).collect();
        let n: i32 = match digits.parse() {
            Ok(n) => n,
            Err(_) => continue,
        };
        if let Some(blob) = e.to_object(repo).ok().and_then(|o| o.into_blob().ok()) {
            if let Ok(s) = String::from_utf8(blob.content().to_vec()) {
                out.insert(n, s);
            }
        }
    }
    out
}

/// Разобранный per-step .md — безопасное подмножество полей (title/desc/command),
/// которое можно однозначно распарсить обратно из формата bundle::step_file.
#[derive(Debug, PartialEq)]
pub struct ParsedStepMd {
    pub title: Option<String>,
    pub desc: Option<String>,
    pub command: Option<String>,
}

/// Значение front-matter вида `key: <json-строка>` или `key: raw` → строка.
/// version_files пишет title/section/command как JSON.stringify(...), level — сырьём.
fn front_value(raw: &str) -> String {
    let t = raw.trim();
    if t.starts_with('"') {
        serde_json::from_str::<String>(t).unwrap_or_else(|_| t.to_string())
    } else {
        t.to_string()
    }
}

/// Обратный парс steps/NN-*.md → title/desc/command (зеркало bundle::step_file).
/// Возвращает None для полей, которых нет в файле; вызывающий берёт их из list.json.
/// Безопасное подмножество: subtasks/refs/why/section/level НЕ реэкспортируем
/// (их источник — list.json), чтобы round-trip оставался идемпотентным.
pub fn parse_step_md(content: &str) -> ParsedStepMd {
    let mut title = None;
    let mut command = None;

    // 1) Front-matter между первой парой строк "---".
    let mut lines = content.lines();
    let mut body_start = 0usize; // индекс строки, с которой начинается тело
    if lines.next() == Some("---") {
        let mut idx = 1usize;
        for line in content.lines().skip(1) {
            idx += 1;
            if line.trim_end() == "---" {
                body_start = idx; // после закрывающего "---"
                break;
            }
            if let Some(rest) = line.strip_prefix("title:") {
                title = Some(front_value(rest));
            } else if let Some(rest) = line.strip_prefix("command:") {
                command = Some(front_value(rest));
            }
        }
    }

    // 2) Тело: desc — это текст ДО первого маркера (**Why:**, subtask "- [ ]", ref "- ").
    //    version_files кладёт desc первым блоком и отделяет пустой строкой.
    let body: Vec<&str> = content.lines().skip(body_start).collect();
    let mut desc_lines: Vec<&str> = Vec::new();
    for line in &body {
        let t = line.trim_start();
        if t.starts_with("**Why:**") || t.starts_with("- [ ] ") || t.starts_with("- ") {
            break;
        }
        desc_lines.push(line);
    }
    // Обрезаем ведущие/замыкающие пустые строки (front-matter отделён пустой строкой).
    while desc_lines.first().map(|l| l.trim().is_empty()).unwrap_or(false) {
        desc_lines.remove(0);
    }
    while desc_lines.last().map(|l| l.trim().is_empty()).unwrap_or(false) {
        desc_lines.pop();
    }
    let desc = if desc_lines.is_empty() {
        None
    } else {
        Some(desc_lines.join("\n"))
    };

    ParsedStepMd { title, desc, command }
}

// Тег vN на запушенный tip (git2, force).
fn tag_version(bare: &Path, ver: i32, tip_hex: &str) -> Result<(), git2::Error> {
    let repo = git2::Repository::open_bare(bare)?;
    let oid = git2::Oid::from_str(tip_hex)?;
    let obj = repo.find_object(oid, Some(git2::ObjectType::Commit))?;
    repo.tag_lightweight(&format!("v{ver}"), &obj, true)?;
    Ok(())
}

/// Срезает префикс "vN: " из subject коммита (порт /^v\d+:\s*/).
fn strip_v_prefix(s: &str) -> String {
    let b = s.as_bytes();
    if b.first() == Some(&b'v') {
        let mut i = 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i > 1 && i < b.len() && b[i] == b':' {
            i += 1;
            while i < b.len() && b[i] == b' ' {
                i += 1;
            }
            return s[i..].to_string();
        }
    }
    s.to_string()
}

/// Проецирует запушенный tip main → новая версия списка (порт project.ts projectPushedCommit).
/// Источник контента — list.json в корне дерева. Возвращает номер версии или None.
// Общий парс шагов: list.json (набор/порядок) + steps/NN-*.md (пер-шаговые оверрайды).
fn parse_steps(steps_raw: &[RawStep], step_md: &HashMap<i32, String>) -> Vec<ProjStep> {
    let mut steps: Vec<ProjStep> = steps_raw
        .iter()
        .filter(|s| !s.title.as_deref().unwrap_or("").trim().is_empty())
        .map(|s| ProjStep {
            title: s.title.clone().unwrap_or_default(),
            desc: s.desc.clone().unwrap_or_default(),
            command: s.command.clone().unwrap_or_default(),
            level: s.level.clone().unwrap_or_default(),
            why: s.why.clone().unwrap_or_default(),
            section: s.section.clone().unwrap_or_default(),
            subtasks: s.subtasks.clone().unwrap_or_default(),
            refs: s
                .refs
                .as_ref()
                .map(|rs| {
                    rs.iter()
                        .filter(|r| !r.label.as_deref().unwrap_or("").trim().is_empty())
                        .map(|r| ProjRef {
                            label: r.label.clone().unwrap_or_default(),
                            url: r.url.clone(),
                        })
                        .collect()
                })
                .unwrap_or_default(),
        })
        .collect();

    // list.json — источник истины для НАБОРА/порядка шагов; steps/NN-*.md — опциональные
    // per-step оверрайды контента. Для шага с 1-based индексом NN (совпадает с именем файла)
    // берём title/desc/command из .md там, где они ОТЛИЧАЮТСЯ от list.json.
    // Отсутствующий/непарсибельный .md → значения list.json (без ошибки).
    for (i, step) in steps.iter_mut().enumerate() {
        let nn = (i as i32) + 1;
        let Some(content) = step_md.get(&nn) else { continue };
        let md = parse_step_md(content);
        if let Some(t) = md.title.filter(|t| !t.trim().is_empty() && *t != step.title) {
            step.title = t;
        }
        if let Some(d) = md.desc.filter(|d| *d != step.desc) {
            step.desc = d;
        }
        if let Some(c) = md.command.filter(|c| *c != step.command) {
            step.command = c;
        }
    }

    steps
}

/// Снапшот произвольного ref (ветки): мета list.json + шаги. Для read-only рендера.
pub struct BranchSnapshotData {
    pub tip: String,
    pub title: String,
    pub desc: String,
    pub tags: Vec<String>,
    pub ordered: bool,
    pub steps: Vec<ProjStep>,
}

pub fn branch_snapshot(bare: &Path, refname: &str) -> Option<BranchSnapshotData> {
    let (tip, raw, _subject, step_md) = read_ref_tip(bare, refname)?;
    let parsed: RawList = serde_json::from_slice(&raw).ok()?;
    let steps = parse_steps(parsed.steps.as_deref().unwrap_or(&[]), &step_md);
    Some(BranchSnapshotData {
        tip,
        title: parsed.title.unwrap_or_default(),
        desc: parsed.desc.unwrap_or_default(),
        tags: parsed.tags.unwrap_or_default(),
        ordered: parsed.ordered.unwrap_or(true),
        steps,
    })
}

pub async fn project_pushed_commit(pool: &PgPool, template_id: Uuid, bare: &Path) -> Option<i32> {
    // git2-объекты не Send → читаем всё owned до await.
    let (tip, raw, subject, step_md) = read_tip(bare)?;
    let parsed: RawList = serde_json::from_slice(&raw).ok()?;
    let steps_raw = parsed.steps.as_ref()?;

    let note: String = {
        let stripped: String = strip_v_prefix(&subject).chars().take(200).collect();
        if stripped.trim().is_empty() {
            "pushed via git".to_string()
        } else {
            stripped
        }
    };

    let steps = parse_steps(steps_raw, &step_md);

    let ver = db::add_version(pool, template_id, &note, &steps).await.ok()?;
    let _ = db::update_meta(pool, template_id, parsed.title.clone(), parsed.desc.clone(), parsed.tags.clone(), parsed.ordered).await;
    // Тег vN на запушенный коммит (для maxTagVersion/истории).
    let _ = tag_version(bare, ver, &tip);
    Some(ver)
}

#[cfg(test)]
mod tests {
    use super::{parse_step_md, strip_v_prefix, ParsedStepMd};
    use crate::bundle::{SerStep, StepRef};

    #[test]
    fn strips_vn_prefix() {
        assert_eq!(strip_v_prefix("v12: pushed via git"), "pushed via git");
        assert_eq!(strip_v_prefix("v1:   spaced"), "spaced");
        assert_eq!(strip_v_prefix("v3"), "v3"); // нет двоеточия — не трогаем
        assert_eq!(strip_v_prefix("hello world"), "hello world");
        assert_eq!(strip_v_prefix("version 2"), "version 2"); // не vN:
        assert_eq!(strip_v_prefix("v7: "), "");
    }

    fn ser(title: &str, desc: &str, command: &str) -> SerStep {
        SerStep {
            n: 1,
            title: title.into(),
            desc: desc.into(),
            command: command.into(),
            level: "required".into(),
            why: "always".into(),
            section: "Setup".into(),
            subtasks: vec!["sub a".into(), "sub b".into()],
            refs: vec![StepRef { label: "docs".into(), url: Some("https://x".into()) }],
        }
    }

    // Генерирует контент .md ровно так, как это делает version_files (bundle::step_file).
    fn gen_md(s: &SerStep) -> String {
        crate::bundle::version_files(&crate::bundle::VersionData {
            version: 1,
            note: String::new(),
            ts: 0,
            title: "L".into(),
            desc: String::new(),
            tags: vec![],
            ordered: true,
            steps: vec![SerStep {
                n: s.n,
                title: s.title.clone(),
                desc: s.desc.clone(),
                command: s.command.clone(),
                level: s.level.clone(),
                why: s.why.clone(),
                section: s.section.clone(),
                subtasks: s.subtasks.clone(),
                refs: s.refs.iter().map(|r| StepRef { label: r.label.clone(), url: r.url.clone() }).collect(),
            }],
        })
        .into_iter()
        .find(|(p, _)| p.starts_with("steps/"))
        .unwrap()
        .1
    }

    #[test]
    fn roundtrip_recovers_title_desc_command() {
        // generate → parse обязан вернуть исходные title/desc/command.
        let step = ser("Install Redis", "Grab the binary\nand run it", "brew install redis");
        let md = gen_md(&step);
        let p = parse_step_md(&md);
        assert_eq!(p.title.as_deref(), Some("Install Redis"));
        assert_eq!(p.desc.as_deref(), Some("Grab the binary\nand run it"));
        assert_eq!(p.command.as_deref(), Some("brew install redis"));
    }

    #[test]
    fn roundtrip_stable_without_desc_or_command() {
        // Пустые desc/command → в файле их нет → parse отдаёт None (берём из list.json).
        let step = ser("Just a title", "", "");
        let md = gen_md(&step);
        let p = parse_step_md(&md);
        assert_eq!(p.title.as_deref(), Some("Just a title"));
        assert_eq!(p.desc, None);
        assert_eq!(p.command, None);
    }

    #[test]
    fn modified_md_overrides_listjson() {
        // Пользователь отредактировал .md: изменённые поля должны отличаться от list.json.
        let orig = ser("Old title", "old desc", "old cmd");
        let mut edited = orig;
        edited.title = "New title".into();
        edited.desc = "new desc".into();
        edited.command = "new cmd".into();
        let p = parse_step_md(&gen_md(&edited));
        assert_eq!(p.title.as_deref(), Some("New title"));
        assert_eq!(p.desc.as_deref(), Some("new desc"));
        assert_eq!(p.command.as_deref(), Some("new cmd"));
    }

    #[test]
    fn parse_handles_unusual_content() {
        // Мусор/без front-matter → безопасно: title/command None, тело как desc.
        let p = parse_step_md("no front matter here");
        assert_eq!(p, ParsedStepMd { title: None, desc: Some("no front matter here".into()), command: None });
    }
}
