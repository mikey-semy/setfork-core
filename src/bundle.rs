use std::fs;
use std::io;
use std::path::Path;
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

/// README.md — человекочитаемый обзор (упрощённый; байт-точное выравнивание с TS — позже).
fn readme(v: &VersionData) -> String {
    let mut out = String::new();
    out.push_str(&format!("# {}\n\n", v.title));
    if !v.desc.is_empty() {
        out.push_str(&format!("{}\n\n", v.desc));
    }
    if !v.tags.is_empty() {
        let tags: Vec<String> = v.tags.iter().map(|t| format!("`{}`", t)).collect();
        out.push_str(&format!("{}\n\n", tags.join(" ")));
    }
    let kind = if v.ordered { "Ordered list" } else { "Unordered set" };
    out.push_str(&format!("> {} · v{} · {} items\n\n", kind, v.version, v.steps.len()));
    for (i, s) in v.steps.iter().enumerate() {
        let marker = if v.ordered { format!("{}.", i + 1) } else { "-".to_string() };
        let lvl = if s.level != "required" { format!(" _({})_", s.level) } else { String::new() };
        out.push_str(&format!("{} **{}**{}\n", marker, s.title, lvl));
        if !s.desc.is_empty() {
            out.push_str(&format!("   {}\n", s.desc.replace('\n', "\n   ")));
        }
        if !s.command.is_empty() {
            out.push_str(&format!("\n   ```sh\n   {}\n   ```\n", s.command));
        }
    }
    out
}

fn version_files(v: &VersionData) -> Vec<(String, String)> {
    vec![
        ("README.md".to_string(), readme(v)),
        ("list.json".to_string(), list_json(v)),
    ]
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
/// и возвращает bundle всех рефов. Синхронно — вызывать через spawn_blocking.
pub fn build_bundle(versions: &[VersionData]) -> io::Result<Vec<u8>> {
    if versions.is_empty() {
        return Err(io::Error::new(io::ErrorKind::NotFound, "no versions"));
    }
    let work = std::env::temp_dir().join(format!("setfork-git-{}", uuid::Uuid::new_v4()));
    let bundle_path = std::env::temp_dir().join(format!("setfork-{}.bundle", uuid::Uuid::new_v4()));
    fs::create_dir_all(&work)?;
    let work_s = work.to_string_lossy().to_string();

    let result = (|| -> io::Result<Vec<u8>> {
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
        let bundle_s = bundle_path.to_string_lossy().to_string();
        let mut bcmd = IDENT.to_vec();
        bcmd.extend(["-C", &work_s, "bundle", "create", &bundle_s, "--all"]);
        run_git(&bcmd, None)?;
        fs::read(&bundle_path)
    })();

    let _ = fs::remove_dir_all(&work);
    let _ = fs::remove_file(&bundle_path);
    result
}
