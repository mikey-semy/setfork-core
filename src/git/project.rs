//! Обратная проекция git → БД: чтение состояния списка из дерева коммита
//! (list.json + steps/*.md, порт project.ts) и запись новой версии после
//! push/merge, когда сдвинулся main.
use super::MAIN_REF;
use super::repo::join_err;
use crate::blocks::is_step_type;
use crate::db;
use serde::Deserialize;
use sqlx::PgPool;
use std::path::Path;
use uuid::Uuid;

// Спроецированный шаг (строки; LocaleText соберётся в db::add_version как {"en": …}).
pub struct ProjRef {
    pub label: String,
    pub url: Option<String>,
}
pub struct ProjStep {
    // Ф2a-довесок: картинка (S3-ключ) и пометка «нужен человек» из list.json.
    // needs_human: None = поля в файле не было (старый клон) → значение переносится
    // из текущей версии по block_id; Some(false) — явное снятие пометки пушем.
    pub image_key: Option<String>,
    pub needs_human: Option<bool>,
    pub needs_human_ask: Option<String>,
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
    // Ф2a-довесок: картинка (S3-ключ) и пометка «нужен человек» читаются из файла.
    // needs_human — тристейт: None (нет поля) = «не знаем» → перенос из текущей
    // версии по block_id (CarryOver); Some(false) — явное снятие пометки пушем.
    #[serde(rename = "imageKey")]
    image_key: Option<String>,
    level: Option<String>,
    #[serde(rename = "needsHuman")]
    needs_human: Option<bool>,
    #[serde(rename = "needsHumanAsk")]
    needs_human_ask: Option<String>,
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
    version: Option<i32>,
    steps: Option<Vec<RawStep>>,
}

/// Owned-снимок tip'а: (hex sha, содержимое list.json, subject коммита).
/// git2-объекты не Send — извлекаем всё до await. steps/*.md больше не читаются
/// (Ф2b): list.json — единственный вход проекции; старые папки в истории
/// просто игнорируются.
type TipData = (String, Vec<u8>, String);

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
    Some((oid.to_string(), raw, subject))
}

/// Витрина из канона (Ф2b): парсит list.json и строит README тем же кодом, что
/// материализация версий (`serialize::readme` через `version_files`). Нужна путям,
/// которые кладут в дерево ГОТОВЫЙ канон (запись в ветку, ручной резолв): раньше
/// они меняли list.json, а README тащился старым блобом и протухал до следующей
/// веб-версии. None — канон не разобрался (README тогда не трогаем: битый вход
/// не повод стирать витрину).
pub fn readme_from_canon(raw: &[u8]) -> Option<String> {
    let parsed: RawList = serde_json::from_slice(raw).ok()?;
    let steps = parsed
        .steps
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .enumerate()
        .map(|(idx, r)| crate::git::bundle::SerStep {
            n: r.n.unwrap_or((idx as i32) + 1),
            block_type: r.block_type.clone(),
            content: r.content.clone().unwrap_or(serde_json::Value::Null),
            block_id: r.block_id.clone(),
            title: r.title.clone().unwrap_or_default(),
            desc: r.desc.clone().unwrap_or_default(),
            command: r.command.clone().unwrap_or_default(),
            image_key: r.image_key.clone(),
            level: r.level.clone().unwrap_or_default(),
            needs_human: r.needs_human.unwrap_or(false),
            needs_human_ask: r.needs_human_ask.clone(),
            why: r.why.clone().unwrap_or_default(),
            section: r.section.clone().unwrap_or_default(),
            subtasks: r.subtasks.clone().unwrap_or_default(),
            refs: r
                .refs
                .as_ref()
                .map(|rs| {
                    rs.iter()
                        .map(|x| crate::git::bundle::StepRef {
                            label: x.label.clone().unwrap_or_default(),
                            url: x.url.clone(),
                        })
                        .collect()
                })
                .unwrap_or_default(),
        })
        .collect();
    let v = crate::git::bundle::VersionData {
        version: parsed.version.unwrap_or(0),
        note: String::new(),
        ts: 0,
        title: parsed.title.clone().unwrap_or_default(),
        desc: parsed.desc.clone().unwrap_or_default(),
        tags: parsed.tags.clone().unwrap_or_default(),
        ordered: parsed.ordered.unwrap_or(true),
        kind: parsed.kind.clone(),
        steps,
    };
    crate::git::serialize::version_files(&v).into_iter().find(|(p, _)| p == "README.md").map(|(_, c)| c)
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

// Общий парс шагов из list.json — единственного входа проекции (Ф2b).
fn parse_steps(steps_raw: &[RawStep]) -> Vec<ProjStep> {
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
            image_key: s.image_key.clone().filter(|k| !k.trim().is_empty()),
            needs_human: s.needs_human,
            needs_human_ask: s.needs_human_ask.clone().filter(|a| !a.trim().is_empty()),
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
    let (tip, raw, _subject) = read_ref_tip(bare, refname)?;
    snapshot_from_data(tip, &raw)
}

/// Материализация произвольного коммита (merge-base для трёхстороннего merge).
pub fn commit_snapshot(bare: &Path, sha: &str) -> Option<BranchSnapshotData> {
    let repo = git2::Repository::open_bare(bare).ok()?;
    let oid = git2::Oid::from_str(sha).ok()?;
    let (tip, raw, _subject) = read_commit_data(&repo, oid)?;
    snapshot_from_data(tip, &raw)
}

fn snapshot_from_data(tip: String, raw: &[u8]) -> Option<BranchSnapshotData> {
    let parsed: RawList = serde_json::from_slice(raw).ok()?;
    let steps = parse_steps(parsed.steps.as_deref().unwrap_or(&[]));
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
    let Some((tip, raw, subject)) =
        tokio::task::spawn_blocking(move || read_tip(&bare_read)).await.map_err(join_err)?
    else {
        return Ok(None);
    };
    let parsed: RawList = match serde_json::from_slice(&raw) {
        Ok(p) => p,
        Err(e) => {
            // Пользовательский вход: битый list.json — не сбой сервиса, но след оставляем.
            tracing::warn!(%template_id, error = %e, "projection: list.json does not parse, no version created");
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

    let steps = parse_steps(steps_raw);

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
        tracing::error!(%template_id, ver, error = %e, "projection: version created but metadata not updated");
    }
    // Тег vN на запушенный коммит (для maxTagVersion/истории). Без тега ensure_repo
    // может повторно досыпать версию поверх (шум истории, не потеря данных).
    let bare_tag = bare.to_path_buf();
    match tokio::task::spawn_blocking(move || tag_version(&bare_tag, ver, &tip)).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::error!(%template_id, ver, error = %e, "projection: tag not created"),
        Err(e) => {
            tracing::error!(%template_id, ver, error = %e, "projection: tag not created (task aborted)")
        }
    }
    Ok(Some(ver))
}

#[cfg(test)]
mod tests {
    use super::{RawStep, parse_steps, readme_from_canon, strip_v_prefix};
    use crate::git::bundle::{SerStep, StepRef};

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

    // ── parse_steps: правила набора/фильтрации ────────────────────────────────
    // Ф2b: пер-шаговых md-оверрайдов больше нет — steps/*.md удалены из формата,
    // list.json единственный вход; их тесты ушли вместе с фичей (см. коммит).

    fn raw(n: Option<i32>, ty: Option<&str>, title: &str) -> RawStep {
        RawStep {
            n,
            block_id: None,
            block_type: ty.map(str::to_string),
            content: ty.map(|_| serde_json::json!({"md": "x"})),
            title: Some(title.to_string()),
            desc: Some(String::new()),
            command: Some(String::new()),
            image_key: None,
            level: Some("required".into()),
            needs_human: None,
            needs_human_ask: None,
            why: None,
            section: None,
            subtasks: None,
            refs: None,
        }
    }

    #[test]
    fn parse_steps_filters_untitled_steps_but_keeps_blocks() {
        // Шаг без title — мусор, выбрасывается; text-блок без title валиден.
        let steps = parse_steps(&[
            raw(Some(1), None, ""),
            raw(Some(2), Some("text"), ""),
            raw(Some(3), None, "Kept"),
        ]);
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].block_type, "text");
        assert_eq!(steps[1].title, "Kept");
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
            image_key: None,
            level: "required".into(),
            needs_human: false,
            needs_human_ask: None,
            why: "always".into(),
            section: "Setup".into(),
            subtasks: vec!["sub a".into(), "sub b".into()],
            refs: vec![StepRef { label: "docs".into(), url: Some("https://x".into()) }],
        }
    }

    fn ver(steps: Vec<SerStep>) -> crate::git::bundle::VersionData {
        crate::git::bundle::VersionData {
            version: 4,
            note: String::new(),
            ts: 0,
            title: "L".into(),
            desc: "D".into(),
            tags: vec!["t".into()],
            ordered: true,
            kind: None,
            steps,
        }
    }

    // ── Разбор list.json: перенос покрытия из TS (parseList) перед Ф0b ──────

    /// Round-trip: сериализованная версия читается обратно тем же составом —
    /// и шаги, и презентационные блоки с их payload.
    #[test]
    fn list_json_round_trips_steps_and_blocks() {
        use crate::git::bundle::version_files;
        let v = ver(vec![
            ser("First", "do it", "echo hi"),
            SerStep {
                n: 2,
                block_type: Some("text".into()),
                content: serde_json::json!({ "md": "note" }),
                ..ser("", "", "")
            },
        ]);
        let raw = version_files(&v).into_iter().find(|(p, _)| p == "list.json").unwrap().1;
        let snap = super::snapshot_from_data("sha".into(), raw.as_bytes()).expect("снапшот");

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
        assert!(super::snapshot_from_data("s".into(), b"not json").is_none());
        assert!(super::snapshot_from_data("s".into(), b"[]").is_none(), "массив — не список");

        let empty = super::snapshot_from_data("s".into(), b"{}").expect("пустой объект");
        assert!(empty.title.is_empty() && empty.steps.is_empty());
        assert!(empty.ordered, "по умолчанию список упорядоченный");
    }

    // ── Ф2b: витрина из канона ────────────────────────────────────────────────

    /// ПАРИТЕТ ДВУХ ПУТЕЙ README: витрина, пересобранная из канона (пути записи
    /// готового list.json — ветка, ручной резолв), обязана быть байт-в-байт той
    /// же, что у материализации версии. Иначе README снова начнёт жить двумя
    /// жизнями — ровно от этого Ф2b и избавляется.
    #[test]
    fn readme_из_канона_совпадает_с_материализацией() {
        let v = ver(vec![
            ser("First", "do it", "echo hi"),
            SerStep {
                n: 2,
                block_type: Some("image".into()),
                content: serde_json::json!({ "ref": "https://img/x.png", "caption": "вид" }),
                ..ser("", "", "")
            },
        ]);
        let files = crate::git::bundle::version_files(&v);
        let canon = &files.iter().find(|(p, _)| p == "list.json").unwrap().1;
        let readme = &files.iter().find(|(p, _)| p == "README.md").unwrap().1;
        assert_eq!(
            readme_from_canon(canon.as_bytes()).as_deref(),
            Some(readme.as_str()),
            "две дороги к README обязаны сходиться"
        );
    }

    /// Битый канон витрину не трогает (None), а не подменяет её пустышкой.
    #[test]
    fn битый_канон_не_даёт_readme() {
        assert_eq!(readme_from_canon(b"not json"), None);
    }
}
