use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use git2::{ObjectType, Oid, Repository, Signature, Time};

// Доменные структуры для сериализации версии в git-дерево (порт serialize.ts/bundle.ts).
pub struct StepRef {
    pub label: String,
    pub url: Option<String>,
}
pub struct SerStep {
    pub n: i32,
    pub title: String,
    pub desc: String,
    pub command: String,
    pub level: String,
    pub why: String,
    pub section: String,
    pub subtasks: Vec<String>,
    pub refs: Vec<StepRef>,
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

// Идентичность коммитов — ОДИНАКОВО с TS (store.ts/bundle.ts) для детерминированных SHA.
const AUTHOR_NAME: &str = "SetFork";
const AUTHOR_EMAIL: &str = "git@setfork.com";

/// list.json — машиночитаемый снимок версии (то, что парсит проекция при push).
fn list_json(v: &VersionData) -> String {
    let steps: Vec<serde_json::Value> = v
        .steps
        .iter()
        .map(|s| {
            serde_json::json!({
                "n": s.n,
                "title": s.title,
                "desc": s.desc,
                "command": s.command,
                "level": s.level,
                "why": s.why,
                "section": s.section,
                "subtasks": s.subtasks,
                "refs": s.refs.iter().map(|r| {
                    let mut m = serde_json::Map::new();
                    m.insert("label".into(), serde_json::Value::String(r.label.clone()));
                    if let Some(u) = &r.url { m.insert("url".into(), serde_json::Value::String(u.clone())); }
                    serde_json::Value::Object(m)
                }).collect::<Vec<_>>(),
            })
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
    lines.push(format!("> {} · v{} · {} items", kind, v.version, v.steps.len()));
    lines.push(String::new());

    let mut section = String::new();
    for (i, s) in v.steps.iter().enumerate() {
        if !s.section.is_empty() && s.section != section {
            section = s.section.clone();
            lines.push(String::new());
            lines.push(format!("## {}", section));
            lines.push(String::new());
        }
        let marker = if v.ordered { format!("{}.", i + 1) } else { "-".to_string() };
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
    if sliced.is_empty() {
        "step".to_string()
    } else {
        sliced
    }
}

/// steps/NN-slug.md — точный порт serialize.ts stepFile().
fn step_file(s: &SerStep, width: usize) -> (String, String) {
    let mut front: Vec<String> = vec![
        "---".to_string(),
        format!("title: {}", json_str(&s.title)),
        format!("level: {}", s.level),
    ];
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

pub fn version_files(v: &VersionData) -> Vec<(String, String)> {
    let width = std::cmp::max(2, v.steps.len().to_string().len());
    let mut files = vec![
        ("README.md".to_string(), readme(v)),
        ("list.json".to_string(), list_json(v)),
    ];
    for s in &v.steps {
        files.push(step_file(s, width));
    }
    files
}

fn run_git(args: &[&str]) -> io::Result<()> {
    let out = Command::new("git").args(args).output()?;
    if !out.status.success() {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("git {:?} failed: {}", args, String::from_utf8_lossy(&out.stderr)),
        ));
    }
    Ok(())
}

fn git_io(e: git2::Error) -> io::Error {
    io::Error::new(io::ErrorKind::Other, format!("git2: {e}"))
}

// Сообщение коммита: `git commit -m` добавляет завершающий \n — воспроизводим для SHA-идентичности.
fn commit_message(v: &VersionData) -> String {
    let boilerplate = matches!(v.note.as_str(), "initial" | "edit" | "seeded");
    let msg = if !v.note.is_empty() && !boilerplate {
        format!("v{}: {}", v.version, v.note)
    } else {
        format!("v{}", v.version)
    };
    format!("{msg}\n")
}

// Дерево версии из version_files (README.md, list.json, steps/NN.md); поддерево steps/.
// treebuilder.write() канонично сортирует записи — как git, поэтому SHA дерева совпадает.
fn build_tree(repo: &Repository, v: &VersionData) -> Result<Oid, git2::Error> {
    let mut root = repo.treebuilder(None)?;
    let mut steps = repo.treebuilder(None)?;
    let mut has_steps = false;
    for (path, content) in version_files(v) {
        let blob = repo.blob(content.as_bytes())?;
        if let Some(name) = path.strip_prefix("steps/") {
            steps.insert(name, blob, 0o100644)?;
            has_steps = true;
        } else {
            root.insert(path.as_str(), blob, 0o100644)?;
        }
    }
    if has_steps {
        let steps_oid = steps.write()?;
        root.insert("steps", steps_oid, 0o040000)?;
    }
    root.write()
}

// Один коммит версии с фиксированной идентичностью/датой (SHA-идентично `git commit`).
fn commit_version(repo: &Repository, parent: Option<Oid>, v: &VersionData) -> Result<Oid, git2::Error> {
    let tree = repo.find_tree(build_tree(repo, v)?)?;
    let sig = Signature::new(AUTHOR_NAME, AUTHOR_EMAIL, &Time::new(v.ts, 0))?; // offset 0 → +0000
    let msg = commit_message(v);
    let parents: Vec<git2::Commit> = match parent {
        Some(oid) => vec![repo.find_commit(oid)?],
        None => vec![],
    };
    let refs: Vec<&git2::Commit> = parents.iter().collect();
    repo.commit(None, &sig, &sig, &msg, &tree, &refs) // update_ref=None: main выставим в конце
}

// Строит историю версий: коммиты + теги vN + refs/heads/main + HEAD→main. Возвращает tip.
fn build_history(repo: &Repository, versions: &[VersionData], mut parent: Option<Oid>) -> Result<Option<Oid>, git2::Error> {
    for v in versions {
        let oid = commit_version(repo, parent, v)?;
        let obj = repo.find_object(oid, Some(ObjectType::Commit))?;
        repo.tag_lightweight(&format!("v{}", v.version), &obj, true)?;
        parent = Some(oid);
    }
    if let Some(tip) = parent {
        repo.reference("refs/heads/main", tip, true, "setfork")?;
        let _ = repo.set_head("refs/heads/main");
    }
    Ok(parent)
}

/// Материализует историю версий в bare-репо (git2, детерминированные SHA) и возвращает путь.
/// ВЫЗЫВАЮЩИЙ обязан удалить каталог. Синхронно — вызывать через spawn_blocking.
pub fn materialize_repo(versions: &[VersionData]) -> io::Result<PathBuf> {
    if versions.is_empty() {
        return Err(io::Error::new(io::ErrorKind::NotFound, "no versions"));
    }
    let work = std::env::temp_dir().join(format!("setfork-git-{}", uuid::Uuid::new_v4()));
    let build = (|| -> Result<(), git2::Error> {
        let repo = Repository::init_bare(&work)?;
        build_history(&repo, versions, None)?;
        Ok(())
    })();
    match build {
        Ok(()) => Ok(work),
        Err(e) => {
            let _ = fs::remove_dir_all(&work);
            Err(git_io(e))
        }
    }
}

// pre-receive hook (порт store.ts PRE_RECEIVE): каждый пушнутый коммит обязан нести list.json.
const PRE_RECEIVE: &str = "#!/bin/sh\n# SetFork: каждый пушнутый коммит обязан содержать list.json в корне.\nwhile read old new ref; do\n  case \"$new\" in *0000000000000000000000000000000000000000) continue ;; esac\n  if ! git cat-file -e \"$new:list.json\" 2>/dev/null; then\n    echo \"SetFork: list.json is required at the repo root\" >&2\n    exit 1\n  fi\ndone\nexit 0\n";

fn install_hook(bare: &Path) -> io::Result<()> {
    let hooks = bare.join("hooks");
    fs::create_dir_all(&hooks)?;
    fs::write(hooks.join("pre-receive"), PRE_RECEIVE)?;
    // executable bit — только на unix; на Windows git-for-windows берёт хук через sh.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(hooks.join("pre-receive"), fs::Permissions::from_mode(0o755));
    }
    Ok(())
}

/// Бутстрап персистентного bare-репо из полной истории версий (git2) + pre-receive hook.
/// Больше нет temp-репо и `git clone --bare` — коммиты пишутся прямо в bare через git2.
pub fn bootstrap_bare(versions: &[VersionData], bare: &Path) -> io::Result<()> {
    if versions.is_empty() {
        return Err(io::Error::new(io::ErrorKind::NotFound, "no versions"));
    }
    if let Some(parent) = bare.parent() {
        fs::create_dir_all(parent)?;
    }
    (|| -> Result<(), git2::Error> {
        let repo = Repository::init_bare(bare)?;
        build_history(&repo, versions, None)?;
        Ok(())
    })()
    .map_err(git_io)?;
    install_hook(bare)
}

/// Дописывает недостающие веб-версии поверх текущего main (git2), сохраняя запушенные коммиты.
/// `versions` — только те, что добавить (version > have). Без worktree.
pub fn append_versions(bare: &Path, versions: &[VersionData]) -> io::Result<()> {
    if versions.is_empty() {
        return Ok(());
    }
    (|| -> Result<(), git2::Error> {
        let repo = Repository::open_bare(bare)?;
        let parent = repo.refname_to_id("refs/heads/main").ok();
        build_history(&repo, versions, parent)?;
        Ok(())
    })()
    .map_err(git_io)
}

/// Максимальный номер версии среди тегов v* (git2).
pub fn max_tag_version(bare: &Path) -> i32 {
    let repo = match Repository::open_bare(bare) {
        Ok(r) => r,
        Err(_) => return 0,
    };
    let names = match repo.tag_names(Some("v*")) {
        Ok(n) => n,
        Err(_) => return 0,
    };
    let mut max = 0i32;
    for name in names.iter().flatten() {
        if let Some(num) = name.strip_prefix('v') {
            if let Ok(n) = num.parse::<i32>() {
                max = max.max(n);
            }
        }
    }
    max
}

/// Материализует репо (git2) и возвращает bundle всех рефов.
/// `git bundle` — через шелл (libgit2 не умеет формат bundle). Синхронно — через spawn_blocking.
pub fn build_bundle(versions: &[VersionData]) -> io::Result<Vec<u8>> {
    let work = materialize_repo(versions)?;
    let work_s = work.to_string_lossy().to_string();
    let bundle_path = std::env::temp_dir().join(format!("setfork-{}.bundle", uuid::Uuid::new_v4()));
    let bundle_s = bundle_path.to_string_lossy().to_string();

    let result = (|| -> io::Result<Vec<u8>> {
        run_git(&["-C", &work_s, "bundle", "create", &bundle_s, "--all"])?;
        fs::read(&bundle_path)
    })();

    let _ = fs::remove_dir_all(&work);
    let _ = fs::remove_file(&bundle_path);
    result
}
