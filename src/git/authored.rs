//! Авторские файлы скилла в дереве версии (ADR-0028): `scripts/`, `references/`, `assets/`.
//!
//! Чтение для экспорта скилла. Эти файлы не генерирует никто — ни ядро, ни приложение, —
//! поэтому архив скилла не может собрать их из блоков и берёт отсюда, ровно теми байтами,
//! что покрыты SHA версии.
use git2::{Oid, Repository};

use super::serialize::AUTHORED_DIRS;

/// Авторский файл: путь от корня, байты, исполняемость (режим 100755).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthoredFile {
    pub path: String,
    pub content: Vec<u8>,
    pub executable: bool,
}

/// Авторские файлы дерева коммита `commit` — по порядку `AUTHORED_DIRS`, внутри каталога —
/// по порядку дерева.
///
/// Только обычные файлы и только один уровень: ровно то, что пускают правило путей и
/// лимиты на обеих дверях записи. Всё прочее пропускается, а не отдаётся «как есть», —
/// приём и выдача обязаны описывать одно и то же множество файлов.
pub fn authored_files(repo: &Repository, commit: Oid) -> Result<Vec<AuthoredFile>, git2::Error> {
    let tree = repo.find_commit(commit)?.tree()?;
    let mut out = Vec::new();
    for dir in AUTHORED_DIRS {
        let Some(entry) = tree.get_name(dir) else { continue };
        let Ok(sub) = entry.to_object(repo).and_then(|o| o.peel_to_tree()) else { continue };
        for e in sub.iter() {
            let mode = e.filemode();
            if !matches!(mode, 0o100644 | 0o100755) {
                continue;
            }
            let Ok(name) = e.name() else { continue };
            let blob = repo.find_blob(e.id())?;
            out.push(AuthoredFile {
                path: format!("{dir}/{name}"),
                content: blob.content().to_vec(),
                executable: mode == 0o100755,
            });
        }
    }
    Ok(out)
}

/// Коммит версии `version` (тег `vN`), а при `0` — вершина `main`. `None` — такого нет.
pub fn version_commit(repo: &Repository, version: i32) -> Option<Oid> {
    let refname = if version > 0 { format!("refs/tags/v{version}") } else { super::MAIN_REF.to_string() };
    repo.find_reference(&refname).ok()?.peel_to_commit().ok().map(|c| c.id())
}
