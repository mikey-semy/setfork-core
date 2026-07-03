use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

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

const IDENT: [&str; 8] = [
    "-c", "user.name=SetFork",
    "-c", "user.email=git@setfork.com",
    "-c", "commit.gpgsign=false",
    "-c", "core.autocrlf=false",
];

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

fn version_files(v: &VersionData) -> Vec<(String, String)> {
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

fn run_git(args: &[&str], dates: Option<i64>) -> io::Result<()> {
    let mut cmd = Command::new("git");
    cmd.args(args);
    if let Some(ts) = dates {
        let d = format!("{} +0000", ts);
        cmd.env("GIT_AUTHOR_DATE", &d).env("GIT_COMMITTER_DATE", &d);
    }
    let out = cmd.output()?;
    if !out.status.success() {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("git {:?} failed: {}", args, String::from_utf8_lossy(&out.stderr)),
        ));
    }
    Ok(())
}

fn reset_tree(dir: &Path) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_name() == ".git" {
            continue;
        }
        let p = entry.path();
        if p.is_dir() {
            fs::remove_dir_all(&p)?;
        } else {
            fs::remove_file(&p)?;
        }
    }
    Ok(())
}

/// Материализует историю версий в git-репо (шелл git, детерминированные SHA)
/// и возвращает путь к рабочему каталогу. Синхронно — вызывать через spawn_blocking.
/// ВЫЗЫВАЮЩИЙ обязан удалить каталог (`fs::remove_dir_all`) после использования.
pub fn materialize_repo(versions: &[VersionData]) -> io::Result<PathBuf> {
    if versions.is_empty() {
        return Err(io::Error::new(io::ErrorKind::NotFound, "no versions"));
    }
    let work = std::env::temp_dir().join(format!("setfork-git-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&work)?;
    let work_s = work.to_string_lossy().to_string();

    let build = (|| -> io::Result<()> {
        run_git(&["init", "-q", "-b", "main", &work_s], None)?;
        for v in versions {
            reset_tree(&work)?;
            for (path, content) in version_files(v) {
                let full = work.join(&path);
                if let Some(parent) = full.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&full, content)?;
            }
            let boilerplate = matches!(v.note.as_str(), "initial" | "edit" | "seeded");
            let msg = if !v.note.is_empty() && !boilerplate {
                format!("v{}: {}", v.version, v.note)
            } else {
                format!("v{}", v.version)
            };
            let mut add = IDENT.to_vec();
            add.extend(["-C", &work_s, "add", "-A"]);
            run_git(&add, None)?;
            let mut commit = IDENT.to_vec();
            commit.extend(["-C", &work_s, "commit", "-q", "--allow-empty", "-m", &msg]);
            run_git(&commit, Some(v.ts))?;
            let tag = format!("v{}", v.version);
            let mut tagcmd = IDENT.to_vec();
            tagcmd.extend(["-C", &work_s, "tag", "-f", &tag]);
            let _ = run_git(&tagcmd, None);
        }
        Ok(())
    })();

    match build {
        Ok(()) => Ok(work),
        Err(e) => {
            let _ = fs::remove_dir_all(&work);
            Err(e)
        }
    }
}

/// Материализует репо и возвращает bundle всех рефов (порт bundle.ts).
/// Синхронно — вызывать через spawn_blocking.
pub fn build_bundle(versions: &[VersionData]) -> io::Result<Vec<u8>> {
    let work = materialize_repo(versions)?;
    let work_s = work.to_string_lossy().to_string();
    let bundle_path = std::env::temp_dir().join(format!("setfork-{}.bundle", uuid::Uuid::new_v4()));
    let bundle_s = bundle_path.to_string_lossy().to_string();

    let result = (|| -> io::Result<Vec<u8>> {
        let mut bcmd = IDENT.to_vec();
        bcmd.extend(["-C", &work_s, "bundle", "create", &bundle_s, "--all"]);
        run_git(&bcmd, None)?;
        fs::read(&bundle_path)
    })();

    let _ = fs::remove_dir_all(&work);
    let _ = fs::remove_file(&bundle_path);
    result
}
