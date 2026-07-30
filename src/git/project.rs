//! Обратная проекция git → БД: чтение состояния списка из дерева коммита
//! (list.json + steps/*.md, порт project.ts) и запись новой версии после
//! push/merge, когда сдвинулся main.
use super::MAIN_REF;
use super::repo::join_err;
use crate::blocks::is_step_type;
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
    // Блочная модель: '' = шаг; 'text'|'image' — презентационный блок. content —
    // payload не-step блока (Null у шага).
    pub block_type: String,
    pub content: serde_json::Value,
    // Стабильная идентичность блока сквозь версии, как её принёс list.json.
    // Раньше пуш её ТЕРЯЛ (в git не сериализовалась) — проекция создавала версию
    // с новыми id, и дифф после пуша откатывался на сопоставление по заголовку.
    pub block_id: Option<String>,
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
    n: Option<i32>,
    #[serde(rename = "blockId")]
    block_id: Option<String>,
    #[serde(rename = "type")]
    block_type: Option<String>,
    content: Option<serde_json::Value>,
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
    // Тип списка (Ф2a): до этого терялся на push — проекция его не читала.
    kind: Option<String>,
    steps: Option<Vec<RawStep>>,
}

/// Owned-снимок tip'а: (hex sha, содержимое list.json, subject коммита,
/// steps/NN-*.md по 1-based номеру NN). git2-объекты не Send — извлекаем всё до await.
type TipData = (String, Vec<u8>, String, HashMap<i32, String>);

fn read_tip(bare: &Path) -> Option<TipData> {
    read_ref_tip(bare, MAIN_REF)
}

/// То же для произвольного ref (ветки) — база просмотра списка «на ветке».
pub fn read_ref_tip(bare: &Path, refname: &str) -> Option<TipData> {
    let repo = git2::Repository::open_bare(bare).ok()?;
    let tip = repo.refname_to_id(refname).ok()?;
    read_commit_data(&repo, tip)
}

// Общее чтение материализации из конкретного коммита (ref-tip или merge-base).
fn read_commit_data(repo: &git2::Repository, oid: git2::Oid) -> Option<TipData> {
    let commit = repo.find_commit(oid).ok()?;
    let tree = commit.tree().ok()?;
    let entry = tree.get_path(Path::new("list.json")).ok()?;
    let blob = entry.to_object(repo).ok()?;
    let raw = blob.as_blob()?.content().to_vec();
    let subject = commit.summary().ok().flatten().unwrap_or("").to_string();
    let steps = read_step_files(repo, &tree);
    Some((oid.to_string(), raw, subject, steps))
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
        // git2 0.21: name() → Result (не-UTF8 имя — ошибка, а не None).
        let name = match e.name() {
            Ok(n) => n,
            Err(_) => continue,
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
        if let Some(blob) = e.to_object(repo).ok().and_then(|o| o.into_blob().ok())
            && let Ok(s) = String::from_utf8(blob.content().to_vec())
        {
            out.insert(n, s);
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
    let desc = if desc_lines.is_empty() { None } else { Some(desc_lines.join("\n")) };

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

// Общий парс шагов: list.json (набор/порядок) + steps/NN-*.md (пер-шаговые оверрайды).
fn parse_steps(steps_raw: &[RawStep], step_md: &HashMap<i32, String>) -> Vec<ProjStep> {
    // Шаг-блок без title — мусор; не-step блоки (text/image) валидны и без title.
    // orig_n — номер шага из list.json (совпадает с именем steps/NN-*.md для оверрайда).
    let mut kept: Vec<(i32, ProjStep)> = Vec::new();
    for (idx, s) in steps_raw.iter().enumerate() {
        let bt = s.block_type.clone().unwrap_or_default();
        let is_step = is_step_type(&bt);
        if is_step && s.title.as_deref().unwrap_or("").trim().is_empty() {
            continue;
        }
        let step = ProjStep {
            block_type: if is_step { String::new() } else { bt },
            block_id: s.block_id.clone().filter(|v| !v.trim().is_empty()),
            content: if is_step {
                serde_json::Value::Null
            } else {
                s.content.clone().unwrap_or(serde_json::Value::Null)
            },
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
                        .map(|r| ProjRef { label: r.label.clone().unwrap_or_default(), url: r.url.clone() })
                        .collect()
                })
                .unwrap_or_default(),
        };
        kept.push((s.n.unwrap_or((idx as i32) + 1), step));
    }

    // list.json — источник истины для НАБОРА/порядка блоков; steps/NN-*.md — опциональные
    // пер-шаговые оверрайды контента (title/desc/command), только у шаг-блоков.
    // Ключ .md — номер шага из list.json (orig_n), а не позиция среди блоков.
    for (orig_n, step) in kept.iter_mut() {
        if !is_step_type(&step.block_type) {
            continue;
        }
        let Some(content) = step_md.get(orig_n) else { continue };
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

    kept.into_iter().map(|(_, s)| s).collect()
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

/// Снапшот ветки по refname: мета list.json + шаги с tip'а. Для read-only рендера.
pub fn branch_snapshot(bare: &Path, refname: &str) -> Option<BranchSnapshotData> {
    let (tip, raw, _subject, step_md) = read_ref_tip(bare, refname)?;
    snapshot_from_data(tip, &raw, &step_md)
}

/// Материализация произвольного коммита (merge-base для трёхстороннего merge).
pub fn commit_snapshot(bare: &Path, sha: &str) -> Option<BranchSnapshotData> {
    let repo = git2::Repository::open_bare(bare).ok()?;
    let oid = git2::Oid::from_str(sha).ok()?;
    let (tip, raw, _subject, step_md) = read_commit_data(&repo, oid)?;
    snapshot_from_data(tip, &raw, &step_md)
}

fn snapshot_from_data(tip: String, raw: &[u8], step_md: &HashMap<i32, String>) -> Option<BranchSnapshotData> {
    let parsed: RawList = serde_json::from_slice(raw).ok()?;
    let steps = parse_steps(parsed.steps.as_deref().unwrap_or(&[]), step_md);
    Some(BranchSnapshotData {
        tip,
        title: parsed.title.unwrap_or_default(),
        desc: parsed.desc.unwrap_or_default(),
        tags: parsed.tags.unwrap_or_default(),
        ordered: parsed.ordered.unwrap_or(true),
        steps,
    })
}

/// Проецирует запушенный tip main → новая версия списка (порт project.ts projectPushedCommit).
/// Источник контента — list.json в корне дерева.
///
/// Семантика результата (важно для вызывающих):
/// - `Ok(Some(v))` — создана версия v;
/// - `Ok(None)` — проецировать нечего (нет валидного list.json/steps) — НЕ ошибка;
/// - `Err(_)` — реальный сбой записи в БД: git-данные целы, версия НЕ создана.
///   Вызывающий обязан громко залогировать; восстановление — `reproject <owner> <slug>`
///   (CLI-режим в main.rs).
pub async fn project_pushed_commit(
    pool: &PgPool,
    template_id: Uuid,
    bare: &Path,
) -> Result<Option<i32>, sqlx::Error> {
    // git2-объекты не Send и блокируют поток → всё git-чтение в spawn_blocking,
    // наружу только owned-данные.
    let bare_read = bare.to_path_buf();
    let Some((tip, raw, subject, step_md)) =
        tokio::task::spawn_blocking(move || read_tip(&bare_read)).await.map_err(join_err)?
    else {
        return Ok(None);
    };
    let parsed: RawList = match serde_json::from_slice(&raw) {
        Ok(p) => p,
        Err(e) => {
            // Пользовательский вход: битый list.json — не сбой сервиса, но след оставляем.
            tracing::warn!(%template_id, error = %e, "проекция: list.json не парсится — версия не создана");
            return Ok(None);
        }
    };
    let Some(steps_raw) = parsed.steps.as_ref() else {
        return Ok(None);
    };

    let note: String = {
        let stripped: String = strip_v_prefix(&subject).chars().take(200).collect();
        if stripped.trim().is_empty() { "pushed via git".to_string() } else { stripped }
    };

    let steps = parse_steps(steps_raw, &step_md);

    let ver = db::add_version(pool, template_id, &note, &steps).await?;
    // Мета и тег — вторичны: их сбой не отменяет созданную версию, но виден в логе.
    if let Err(e) = db::update_meta(
        pool,
        template_id,
        parsed.title.clone(),
        parsed.desc.clone(),
        parsed.tags.clone(),
        parsed.ordered,
        parsed.kind.clone(),
    )
    .await
    {
        tracing::error!(%template_id, ver, error = %e, "проекция: версия создана, но мета не обновлена");
    }
    // Тег vN на запушенный коммит (для maxTagVersion/истории). Без тега ensure_repo
    // может повторно досыпать версию поверх (шум истории, не потеря данных).
    let bare_tag = bare.to_path_buf();
    match tokio::task::spawn_blocking(move || tag_version(&bare_tag, ver, &tip)).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::error!(%template_id, ver, error = %e, "проекция: тег не поставлен"),
        Err(e) => {
            tracing::error!(%template_id, ver, error = %e, "проекция: тег не поставлен (задача прервана)")
        }
    }
    Ok(Some(ver))
}

#[cfg(test)]
mod tests {
    use super::{ParsedStepMd, RawStep, parse_step_md, parse_steps, strip_v_prefix};
    use crate::git::bundle::{SerStep, StepRef};
    use std::collections::HashMap;

    #[test]
    fn strips_vn_prefix() {
        assert_eq!(strip_v_prefix("v12: pushed via git"), "pushed via git");
        assert_eq!(strip_v_prefix("v1:   spaced"), "spaced");
        assert_eq!(strip_v_prefix("v3"), "v3"); // нет двоеточия — не трогаем
        assert_eq!(strip_v_prefix("hello world"), "hello world");
        assert_eq!(strip_v_prefix("version 2"), "version 2"); // не vN:
        assert_eq!(strip_v_prefix("v7: "), "");
        assert_eq!(strip_v_prefix("v: x"), "v: x"); // «v» без цифр — не префикс версии
    }

    // ── parse_steps: правила набора/фильтрации/оверрайдов (cargo-mutants 2026-07-20
    // показал, что они не были покрыты напрямую) ──────────────────────────────

    fn raw(n: Option<i32>, ty: Option<&str>, title: &str) -> RawStep {
        RawStep {
            n,
            block_id: None,
            block_type: ty.map(str::to_string),
            content: ty.map(|_| serde_json::json!({"md": "x"})),
            title: Some(title.to_string()),
            desc: Some(String::new()),
            command: Some(String::new()),
            level: Some("required".into()),
            why: None,
            section: None,
            subtasks: None,
            refs: None,
        }
    }

    #[test]
    fn parse_steps_filters_untitled_steps_but_keeps_blocks() {
        // Шаг без title — мусор, выбрасывается; text-блок без title валиден.
        let steps = parse_steps(
            &[raw(Some(1), None, ""), raw(Some(2), Some("text"), ""), raw(Some(3), None, "Kept")],
            &HashMap::new(),
        );
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].block_type, "text");
        assert_eq!(steps[1].title, "Kept");
    }

    #[test]
    fn parse_steps_md_override_keyed_by_orig_n_and_steps_only() {
        // Ключ оверрайда — номер шага из list.json (n, при отсутствии — позиция+1),
        // НЕ позиция после фильтрации; на не-step блоки оверрайд не действует.
        let md = HashMap::from([
            (3, "---\ntitle: \"Overridden\"\nlevel: required\n---\n\nnew desc\n".to_string()),
            (2, "---\ntitle: \"Block override must be ignored\"\nlevel: required\n---\n".to_string()),
        ]);
        let steps = parse_steps(
            &[raw(Some(1), None, ""), raw(Some(2), Some("text"), ""), raw(Some(3), None, "Orig")],
            &md,
        );
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].block_type, "text", "блок не тронут оверрайдом");
        assert_eq!(steps[1].title, "Overridden", "оверрайд нашёл шаг по orig_n=3");
        assert_eq!(steps[1].desc, "new desc");
    }

    #[test]
    fn parse_steps_ignores_empty_md_title_and_missing_fields() {
        // Пустой title в .md не перекрывает list.json; отсутствующие в .md поля
        // (command без front-matter-строки) остаются из list.json.
        let md = HashMap::from([(1, "---\ntitle: \"\"\nlevel: required\n---\n\nonly desc\n".to_string())]);
        let mut base = raw(Some(1), None, "Keep me");
        base.command = Some("keep-cmd".into());
        let steps = parse_steps(&[base], &md);
        assert_eq!(steps[0].title, "Keep me", "пустой md-title игнорируется");
        assert_eq!(steps[0].desc, "only desc", "desc из md применён");
        assert_eq!(steps[0].command, "keep-cmd", "command не тронут (нет в md)");
    }

    #[test]
    fn parse_steps_positional_n_when_absent() {
        // Без поля n ключ оверрайда — позиция в list.json (idx+1).
        let md =
            HashMap::from([(2, "---\ntitle: \"Second overridden\"\nlevel: required\n---\n".to_string())]);
        let steps = parse_steps(&[raw(None, None, "One"), raw(None, None, "Two")], &md);
        assert_eq!(steps[1].title, "Second overridden");
    }

    fn ser(title: &str, desc: &str, command: &str) -> SerStep {
        SerStep {
            n: 1,
            block_type: None,
            content: serde_json::Value::Null,
            block_id: None,
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
        crate::git::bundle::version_files(&crate::git::bundle::VersionData {
            version: 1,
            note: String::new(),
            ts: 0,
            title: "L".into(),
            desc: String::new(),
            tags: vec![],
            ordered: true,
            kind: None,
            steps: vec![SerStep {
                n: s.n,
                block_type: None,
                content: serde_json::Value::Null,
                block_id: None,
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

    // ── Разбор list.json: перенос покрытия из TS (parseList) перед Ф0b ──────

    /// Round-trip: сериализованная версия читается обратно тем же составом —
    /// и шаги, и презентационные блоки с их payload.
    #[test]
    fn list_json_round_trips_steps_and_blocks() {
        use crate::git::bundle::{VersionData, version_files};
        let v = VersionData {
            version: 4,
            note: String::new(),
            ts: 0,
            title: "L".into(),
            desc: "D".into(),
            tags: vec!["t".into()],
            ordered: true,
            kind: None,
            steps: vec![
                ser("First", "do it", "echo hi"),
                SerStep {
                    n: 2,
                    block_type: Some("text".into()),
                    content: serde_json::json!({ "md": "note" }),
                    ..ser("", "", "")
                },
            ],
        };
        let raw = version_files(&v).into_iter().find(|(p, _)| p == "list.json").unwrap().1;
        let snap = super::snapshot_from_data("sha".into(), raw.as_bytes(), &HashMap::new()).expect("снапшот");

        assert_eq!((snap.title.as_str(), snap.desc.as_str(), snap.ordered), ("L", "D", true));
        assert_eq!(snap.tags, vec!["t".to_string()]);
        assert_eq!(snap.steps.len(), 2, "шаг и блок оба доехали");
        assert_eq!(snap.steps[0].title, "First");
        assert_eq!(snap.steps[0].command, "echo hi");
        assert_eq!(snap.steps[1].block_type, "text");
        assert_eq!(snap.steps[1].content, serde_json::json!({ "md": "note" }));
    }

    /// Мусор не должен притворяться списком. ВНИМАНИЕ на пустой объект: он
    /// разбирается в снапшот БЕЗ шагов (все поля опциональны), а не отвергается —
    /// TS-реализация на `{}` возвращала null. Расхождение зафиксировано осознанно:
    /// `pre-receive` требует лишь НАЛИЧИЯ list.json, поэтому `{}` реально может
    /// приехать пушем, и пустой снапшот честнее отказа «ветки нет».
    #[test]
    fn list_json_garbage_is_rejected_but_empty_object_is_empty_snapshot() {
        assert!(super::snapshot_from_data("s".into(), b"not json", &HashMap::new()).is_none());
        assert!(
            super::snapshot_from_data("s".into(), b"[]", &HashMap::new()).is_none(),
            "массив — не список"
        );

        let empty = super::snapshot_from_data("s".into(), b"{}", &HashMap::new()).expect("пустой объект");
        assert!(empty.title.is_empty() && empty.steps.is_empty());
        assert!(empty.ordered, "по умолчанию список упорядоченный");
    }

    #[test]
    fn parse_handles_unusual_content() {
        // Мусор/без front-matter → безопасно: title/command None, тело как desc.
        let p = parse_step_md("no front matter here");
        assert_eq!(p, ParsedStepMd { title: None, desc: Some("no front matter here".into()), command: None });
    }
}
