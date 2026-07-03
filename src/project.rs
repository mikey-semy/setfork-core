use crate::db;
use serde::Deserialize;
use sqlx::PgPool;
use std::path::Path;
use std::process::Command;
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

fn git_out(bare: &str, args: &[&str]) -> Option<Vec<u8>> {
    let out = Command::new("git").arg("-C").arg(bare).args(args).output().ok()?;
    if out.status.success() {
        Some(out.stdout)
    } else {
        None
    }
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
pub async fn project_pushed_commit(pool: &PgPool, template_id: Uuid, bare: &Path) -> Option<i32> {
    let bare_s = bare.to_string_lossy().to_string();
    let tip = String::from_utf8_lossy(&git_out(&bare_s, &["rev-parse", "main"])?)
        .trim()
        .to_string();
    let raw = git_out(&bare_s, &["show", &format!("{}:list.json", tip)])?;
    let parsed: RawList = serde_json::from_slice(&raw).ok()?;
    let steps_raw = parsed.steps.as_ref()?;

    let subject = String::from_utf8_lossy(&git_out(&bare_s, &["log", "-1", "--format=%s", &tip])?)
        .trim()
        .to_string();
    let note: String = {
        let stripped: String = strip_v_prefix(&subject).chars().take(200).collect();
        if stripped.trim().is_empty() {
            "pushed via git".to_string()
        } else {
            stripped
        }
    };

    let steps: Vec<ProjStep> = steps_raw
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

    let ver = db::add_version(pool, template_id, &note, &steps).await.ok()?;
    let _ = db::update_meta(pool, template_id, parsed.title.clone(), parsed.desc.clone(), parsed.tags.clone(), parsed.ordered).await;
    // Тег vN на запушенный коммит (для maxTagVersion/истории).
    let _ = Command::new("git")
        .args(["-C", &bare_s, "tag", "-f", &format!("v{}", ver), &tip])
        .output();
    Some(ver)
}
