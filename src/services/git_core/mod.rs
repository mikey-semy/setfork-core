//! GitCore — gRPC-сервис поверх git-подсистемы: smart-HTTP (clone/push),
//! ветки/теги/merge, bundle. Обслуживает proto/git.proto (setfork.git.v1).
use sqlx::postgres::PgPool;
use std::path::PathBuf;
use tonic::{Code, Request, Response, Status};

use crate::reason::{self, Reason};
use uuid::Uuid;

use super::util::{db_status, internal};
use crate::db;
use crate::git::bundle::VersionData;
use crate::git::update::{MainUpdateError, update_main};
use crate::git::{MAIN_REF, bundle, history, project, repo, smart_http, write};
use crate::pb::git_core_server::GitCore;
use crate::pb::{
    Branch, BranchOpResponse, BranchSnapshotRequest, BranchSnapshotResponse, BranchesResponse, BytesResponse,
    CanonIssue, CapabilitiesRequest, CapabilitiesResponse, Commit, CommitToBranchRequest,
    CommitToBranchResponse, CommitsResponse, CreateBranchRequest, CreateTagRequest, DeleteBranchRequest,
    InfoRefsRequest, ListCommitsRequest, MergeBranchRequest, MergeBranchResponse, MergeResolvedRequest,
    MergeStateRequest, MergeStateResponse, MirrorCheckResponse, MirrorPushResponse, ParseCanonRequest,
    ParseCanonResponse, PostRequest, ReceivePackResponse, RenderCanonRequest, RenderCanonResponse, RepoRef,
    Tag, TagsResponse, UpdateBranchRequest, UpdateBranchResponse,
};

/// GitCore: git-операции (smart-HTTP, ветки/теги/merge, bundle) поверх общего пула.
mod actor;
mod convert;
mod merge;
mod mirror;
mod names;

use actor::choose_actor;
use convert::{canon_list_json, list_content_from_parts, to_snapshot_pb};
pub use merge::with_coauthors;
use merge::{commit_resolved, merge_sig};
pub(crate) use mirror::{push_mirror_now, spawn_mirror};
use names::{is_version_tag, valid_branch, valid_tag};

pub struct GitCoreSvc {
    pub pool: PgPool,
}

impl GitCoreSvc {
    // Резолв списка + загрузка всей истории версий (общее для всех RPC;
    // pub — используется golden-CLI в бинаре, см. main.rs).
    pub async fn load(&self, owner: &str, slug: &str) -> Result<Vec<VersionData>, Status> {
        let (id, _v) = db::resolve_list(&self.pool, owner, slug)
            .await
            .map_err(db_status)?
            .ok_or_else(|| reason::status(Code::NotFound, Reason::NotFound, "list not found"))?;
        db::load_bundle_data(&self.pool, id).await.map_err(db_status)
    }

    // Общая реализация bundle: загрузка версий → материализация → git bundle.
    // pub — используется golden-CLI в бинаре (main.rs).
    pub async fn build(&self, owner: &str, slug: &str) -> Result<Vec<u8>, Status> {
        let versions = self.load(owner, slug).await?;
        tokio::task::spawn_blocking(move || bundle::build_bundle(&versions))
            .await
            .map_err(internal)?
            .map_err(internal)
    }

    // Персистентный bare-репо (bootstrap/append под локом) — общий вход read/write RPC.
    async fn ensure(&self, owner: &str, slug: &str) -> Result<(PathBuf, Uuid), Status> {
        repo::ensure_repo(&self.pool, owner, slug)
            .await
            .map_err(db_status)?
            .ok_or_else(|| reason::status(Code::NotFound, Reason::NotFound, "list not found"))
    }

    /// Только идентичность списка — БЕЗ создания репозитория на диске.
    /// Для RPC, которые ничего не пишут в git: заводить дерево ради чтения текста
    /// значит оставлять следы там, где вызывающий об этом не просил.
    async fn list_id(&self, repo: Option<RepoRef>) -> Result<Uuid, Status> {
        let repo = repo.ok_or_else(|| Status::invalid_argument("repo required"))?;
        db::resolve_list(&self.pool, &repo.owner, &repo.slug)
            .await
            .map_err(db_status)?
            .map(|(id, _ver)| id)
            .ok_or_else(|| reason::status(Code::NotFound, Reason::NotFound, "list not found"))
    }
}

// Открывает bare-репо в spawn_blocking (git2-объекты не Send) и выполняет `f`;
// JoinError и ошибка открытия схлопываются в один internal-Status.
async fn with_repo<T, F>(bare: PathBuf, f: F) -> Result<T, Status>
where
    T: Send + 'static,
    F: FnOnce(&git2::Repository) -> Result<T, Status> + Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let repo = git2::Repository::open_bare(&bare).map_err(internal)?;
        f(&repo)
    })
    .await
    .map_err(internal)?
}

/// Отказ единой точки обновления main → gRPC-статус.
///
/// MissingListJson — единственный «пользовательский» случай (дерево слияния без
/// канона); Stale — конкурентная запись (под репо-локом почти невозможна, но
/// CAS честный); NonFastForward под локом означает сломанный инвариант кода.
fn main_status(e: MainUpdateError) -> Status {
    match e {
        MainUpdateError::MissingListJson => reason::status(
            Code::FailedPrecondition,
            Reason::MissingListJson,
            "list.json is required at the repo root",
        ),
        // Как и MissingListJson — «пользовательский» случай: дерево собрано не по
        // формату. Текст несёт сам путь, иначе отказ нечего показать человеку.
        // По-английски — как ВСЕ остальные Status в ядре: их читает фронт и
        // переводит сам; язык пользователя ядру неизвестен.
        MainUpdateError::ForeignPath(p) => reason::status(
            Code::FailedPrecondition,
            Reason::ForeignPath,
            format!(
                "only README.md, list.json and .gitattributes are allowed in the list tree; foreign path: {p}"
            ),
        ),
        MainUpdateError::Stale => reason::status(Code::Aborted, Reason::Stale, "main moved concurrently"),
        MainUpdateError::NonFastForward => {
            Status::internal("non-fast-forward update of main (invariant breach)")
        }
        MainUpdateError::Git(e) => Status::internal(e),
    }
}

/// Текущий oid main (или None, если ветки ещё нет) — для проверки «push сдвинул main».
fn main_oid(bare: &std::path::Path) -> Option<String> {
    git2::Repository::open_bare(bare).ok()?.refname_to_id(MAIN_REF).ok().map(|o| o.to_string())
}

// Пустая proto-строка → None (proto3 не отличает '' от отсутствия поля).
fn opt(s: &str) -> Option<&str> {
    (!s.is_empty()).then_some(s)
}

/// Проекция нового main-tip → версия БД, с одним повтором на транзиентный сбой.
///
/// Порядок канона: git уже записан и НЕ откатывается — БД догоняет. Поэтому сбой
/// проекции не отменяет git-операцию, но обязан быть громким: метрика + ERROR-лог;
/// восстановление — штатный rebuild проекции `reproject <owner> <slug>` (CLI).
/// «Спроецировать нечего» (битый/пустой list.json) — тот же исход, что и сбой:
/// git принят, версии нет; без метрики случай проходил мимо алерта (линза 02, F3).
async fn project_main_or_log(
    pool: &PgPool,
    id: Uuid,
    bare: &std::path::Path,
    op: &'static str,
    owner: &str,
    slug: &str,
) -> i32 {
    let outcome = match project::project_pushed_commit(pool, id, bare).await {
        Err(e) => {
            tracing::warn!(owner, slug, %id, error = %e, op, "projection failed, retrying in 200ms");
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            project::project_pushed_commit(pool, id, bare).await
        }
        ok => ok,
    };
    match outcome {
        Ok(Some(v)) => v,
        Ok(None) => {
            metrics::counter!("projection_failures_total", "op" => format!("{op}_empty")).increment(1);
            tracing::warn!(
                owner, slug, %id, op,
                "projection produced nothing (list.json unparsed or without steps): version NOT created"
            );
            0
        }
        Err(e) => {
            metrics::counter!("projection_failures_total", "op" => op).increment(1);
            tracing::error!(
                owner, slug, %id, error = %e, op,
                "projection FAILED: git accepted, version NOT created; recovery: reproject"
            );
            0
        }
    }
}

#[tonic::async_trait]
impl GitCore for GitCoreSvc {
    /// Что умеет это ядро (Ф5). Подробности решения — в комментарии к
    /// `CapabilitiesResponse` в proto.
    ///
    /// Признак — константа, а не настройка: он описывает КОД, а не конфигурацию.
    /// Настройкой он был бы бесполезен ровно там, где нужен: оператор, забывший
    /// выставить её после выката, получил бы ту самую пару «новый фронт, ядро без
    /// правила», от которой признак и защищает.
    async fn get_capabilities(
        &self,
        _req: Request<CapabilitiesRequest>,
    ) -> Result<Response<CapabilitiesResponse>, Status> {
        Ok(Response::new(CapabilitiesResponse { enforces_push_roles: true }))
    }

    async fn info_refs_upload_pack(
        &self,
        req: Request<InfoRefsRequest>,
    ) -> Result<Response<BytesResponse>, Status> {
        let InfoRefsRequest { repo, git_protocol } = req.into_inner();
        let repo = repo.ok_or_else(|| Status::invalid_argument("repo required"))?;
        let (bare, _id) = self.ensure(&repo.owner, &repo.slug).await?;
        let data =
            tokio::task::spawn_blocking(move || smart_http::upload_pack_advertise(&bare, opt(&git_protocol)))
                .await
                .map_err(internal)?
                .map_err(internal)?;
        Ok(Response::new(BytesResponse { data }))
    }
    async fn info_refs_receive_pack(
        &self,
        req: Request<InfoRefsRequest>,
    ) -> Result<Response<BytesResponse>, Status> {
        let InfoRefsRequest { repo, git_protocol } = req.into_inner();
        let repo = repo.ok_or_else(|| Status::invalid_argument("repo required"))?;
        let (bare, _id) = self.ensure(&repo.owner, &repo.slug).await?;
        let data = tokio::task::spawn_blocking(move || {
            smart_http::receive_pack_advertise(&bare, opt(&git_protocol))
        })
        .await
        .map_err(internal)?
        .map_err(internal)?;
        Ok(Response::new(BytesResponse { data }))
    }
    async fn upload_pack(&self, req: Request<PostRequest>) -> Result<Response<BytesResponse>, Status> {
        let PostRequest { repo, body, git_protocol, .. } = req.into_inner();
        let repo = repo.ok_or_else(|| Status::invalid_argument("repo required"))?;
        let (bare, _id) = self.ensure(&repo.owner, &repo.slug).await?;
        let data = tokio::task::spawn_blocking(move || {
            smart_http::upload_pack_rpc(&bare, &body, opt(&git_protocol))
        })
        .await
        .map_err(internal)?
        .map_err(internal)?;
        Ok(Response::new(BytesResponse { data }))
    }
    async fn receive_pack(&self, req: Request<PostRequest>) -> Result<Response<ReceivePackResponse>, Status> {
        // `actor_handle` не читаем СОЗНАТЕЛЬНО: ник в логике не участвует (Ф5), поле
        // переходное и живёт ради старого ядра в окно выкатки — см. proto.
        let PostRequest { repo, body, git_protocol, lang, actor_handle, actor_id, actor_role } =
            req.into_inner();
        let repo = repo.ok_or_else(|| Status::invalid_argument("repo required"))?;
        // Предусловие записи (ADR-0015): спрашиваем приложение ДО любой работы.
        crate::gate::ensure_writable(&repo.owner, &repo.slug).await?;
        let (bare, id) = self.ensure(&repo.owner, &repo.slug).await?;
        // Критическая секция: receive-pack + проекция под одним локом репо
        // (ленивый append не вклинивается между приёмом и проекцией).
        let _guard = repo::repo_guard(&self.pool, id).await.map_err(db_status)?;
        // Хук ставится ЗАНОВО, уже под локом. `ensure` выше его тоже ставит, но
        // свой лок отпускает — и в зазор между ними чужой процесс успевает
        // переписать общий файл хука на диске. Так бывает, когда рядом работает
        // ядро ДРУГОЙ версии (общий GIT_DATA_DIR в момент выкатки): её хук правила
        // ролей не знает, а это ядро тем временем отвечает `enforces_push_roles:
        // true` — то есть обещает защиту, которой на диске уже нет (авто-ревью
        // core#80, P1). Под локом зазора не остаётся: чтобы переписать хук, чужой
        // процесс обязан взять тот же advisory-лок, а его держим мы — до конца
        // приёма пака. Запись идемпотентна и на фоне пака ничего не стоит.
        let bare_hook = bare.clone();
        tokio::task::spawn_blocking(move || bundle::install_hook(&bare_hook))
            .await
            .map_err(internal)?
            .map_err(internal)?;
        // Порог размера репо (Ф0). Per-push потолок (receive.maxInputSize) не мешает
        // вырастить репозиторий серией мелких пушей, а квоты размера у git нет вовсе —
        // это уровень приложения (у GitLab так же: «Repository size limit» —
        // настройка приложения, пуш при превышении отклоняется). Проверяем ДО приёма:
        // принять и отказать потом значило бы оставить объекты на диске ровно в том
        // случае, ради которого порог и вводился.
        let limit = crate::config::repo_limit_bytes();
        if limit > 0 {
            let bare_size = bare.clone();
            let size = tokio::task::spawn_blocking(move || bundle::repo_size_bytes(&bare_size))
                .await
                .map_err(internal)?;
            metrics::gauge!("repo_bytes").set(size as f64);
            if size > limit {
                // По-английски — как все Status в ядре (перевод за фронтом).
                return Err(reason::status(
                    Code::ResourceExhausted,
                    Reason::RepoTooLarge,
                    format!(
                        "list repository is {} MB, over the {} MB limit; pushes are stopped",
                        size / (1024 * 1024),
                        limit / (1024 * 1024)
                    ),
                ));
            }
        }
        let bare_recv = bare.clone();
        // Внутри одного spawn_blocking: oid main до и после приёма пака — чтобы
        // проецировать версию ТОЛЬКО когда push реально сдвинул main. Пуш в
        // ветку-черновик main не двигает → иначе плодились бы дубли версий.
        // ПЕРЕХОДНОЕ (снять вместе с полем `actor_handle`): фронт старше Ф5 шлёт
        // только ник. Без запасного пути такое ядро отвергало бы КАЖДЫЙ магический
        // пуш до выката фронта — то есть при обратном порядке выкатки функция
        // ложится ровно так же, как ложилась бы у старого ядра без переходного
        // поля (авто-ревью fe#662). Ник здесь работает как имя ветки, и это
        // сегодняшнее прод-поведение: новое ничего не ломает, а старое доживает.
        //
        let actor = choose_actor(&actor_id, &actor_role, &actor_handle);
        // Ф5: роль едет в хук как есть.
        //
        // Пустая означает «фронт старше Ф5»: он ролей не шлёт — и посторонних не
        // впускает, поэтому правило пространства имён к нему неприменимо. Это
        // нормальное состояние ОКНА ВЫКАТКИ, но оно не должно быть тихим: пока
        // строка есть в логах, ограничение для посторонних фактически не
        // работает, и включать им доступ во фронте рано.
        if actor_role.is_empty() {
            tracing::warn!(
                owner = %repo.owner,
                slug = %repo.slug,
                "push without a role: frontend predates phase 5, contributor namespace rule is not applied"
            );
        }
        let role = actor_role.clone();
        let (data, moved, magic) = tokio::task::spawn_blocking(
            move || -> std::io::Result<(Vec<u8>, bool, Vec<crate::git::magic::MagicPush>)> {
                let before = main_oid(&bare_recv);
                // Снимок магических рефов ДО приёма: без него чужой брошенный
                // refs/for/* присвоился бы текущему пушащему (авто-ревью, P1).
                let magic_before = match git2::Repository::open_bare(&bare_recv) {
                    Ok(r) => crate::git::magic::snapshot(&r).unwrap_or_default(),
                    Err(e) => return Err(std::io::Error::other(e.to_string())),
                };
                let data = smart_http::receive_pack_rpc(
                    &bare_recv,
                    &body,
                    opt(&git_protocol),
                    opt(&lang),
                    opt(&actor),
                    opt(&role),
                )?;
                let after = main_oid(&bare_recv);
                // Ф4: магические рефы разбираем ВНУТРИ той же критической секции,
                // что и приём — между ними не должно вклиниться чужое чтение
                // рефов, иначе кто-то увидит refs/for/* как настоящую ветку.
                let magic = match git2::Repository::open_bare(&bare_recv) {
                    Ok(r) => crate::git::magic::take_magic_pushes(&r, &actor, &magic_before)
                        .map_err(|e| std::io::Error::other(e.to_string()))?,
                    Err(e) => return Err(std::io::Error::other(e.to_string())),
                };
                Ok((data, after.is_some() && after != before, magic))
            },
        )
        .await
        .map_err(internal)?
        .map_err(internal)?;
        // Проекция list.json нового main tip → новая версия (0 = не спроецировано).
        // Сбой проекции НЕ отменяет push (git-объекты целы) — тихая потеря версии
        // худший исход (аудит 2026-07-20, P0-1); громкость — в project_main_or_log.
        let new_version = if moved {
            let v = project_main_or_log(&self.pool, id, &bare, "push", &repo.owner, &repo.slug).await;
            spawn_mirror(self.pool.clone(), id, bare.clone()); // Ф3: зеркало догоняет истину
            v
        } else {
            0
        };
        Ok(Response::new(ReceivePackResponse {
            data,
            new_version,
            magic: magic
                .into_iter()
                .map(|m| crate::pb::MagicPush { base: m.base, branch: m.branch, tip_sha: m.tip_sha })
                .collect(),
        }))
    }
    async fn create_bundle(&self, req: Request<RepoRef>) -> Result<Response<BytesResponse>, Status> {
        let RepoRef { owner, slug } = req.into_inner();
        // Персистентный репо (как TS bundleRepo) — bundle включает запушенные коммиты.
        let data = repo::bundle_repo(&self.pool, &owner, &slug)
            .await
            .map_err(db_status)?
            .ok_or_else(|| reason::status(Code::NotFound, Reason::NotFound, "list not found"))?;
        Ok(Response::new(BytesResponse { data }))
    }

    async fn list_branches(&self, req: Request<RepoRef>) -> Result<Response<BranchesResponse>, Status> {
        let RepoRef { owner, slug } = req.into_inner();
        let (bare, _id) = self.ensure(&owner, &slug).await?;
        // git2-объекты не Send → всё в with_repo (spawn_blocking), наружу только owned-данные.
        let branches = with_repo(bare, |repo| {
            let main_tip = repo.refname_to_id(MAIN_REF).map_err(internal)?;
            let mut out: Vec<Branch> = Vec::new();
            for b in repo.branches(Some(git2::BranchType::Local)).map_err(internal)? {
                let (branch, _) = b.map_err(internal)?;
                let name = branch.name().ok().flatten().unwrap_or("").to_string();
                if name.is_empty() {
                    continue;
                }
                let tip = match branch.get().target() {
                    Some(t) => t,
                    None => continue,
                };
                let (ahead, behind) = if name == "main" {
                    (0, 0)
                } else {
                    repo.graph_ahead_behind(tip, main_tip).unwrap_or((0, 0))
                };
                out.push(Branch {
                    name: name.clone(),
                    tip_sha: tip.to_string(),
                    is_default: name == "main",
                    ahead: ahead as i32,
                    behind: behind as i32,
                });
            }
            // main первым, остальные по имени.
            out.sort_by(|a, b| b.is_default.cmp(&a.is_default).then(a.name.cmp(&b.name)));
            Ok(out)
        })
        .await?;
        Ok(Response::new(BranchesResponse { branches }))
    }

    async fn get_branch_snapshot(
        &self,
        req: Request<BranchSnapshotRequest>,
    ) -> Result<Response<BranchSnapshotResponse>, Status> {
        let BranchSnapshotRequest { repo, branch } = req.into_inner();
        let repo = repo.ok_or_else(|| Status::invalid_argument("repo required"))?;
        if !valid_branch(&branch) {
            return Err(reason::status(Code::InvalidArgument, Reason::BadName, "bad branch name"));
        }
        let (bare, _id) = self.ensure(&repo.owner, &repo.slug).await?;
        let refname = format!("refs/heads/{branch}");
        let snap = tokio::task::spawn_blocking(move || project::branch_snapshot(&bare, &refname))
            .await
            .map_err(internal)?;
        let Some(sn) = snap else {
            return Ok(Response::new(BranchSnapshotResponse { found: false, ..Default::default() }));
        };
        Ok(Response::new(to_snapshot_pb(sn)))
    }

    async fn create_branch(
        &self,
        req: Request<CreateBranchRequest>,
    ) -> Result<Response<BranchOpResponse>, Status> {
        let CreateBranchRequest { repo, name, from } = req.into_inner();
        let repo = repo.ok_or_else(|| Status::invalid_argument("repo required"))?;
        // Предусловие записи (ADR-0015): спрашиваем приложение ДО любой работы.
        crate::gate::ensure_writable(&repo.owner, &repo.slug).await?;
        let from = if from.is_empty() { "main".to_string() } else { from };
        if !valid_branch(&name) || !valid_branch(&from) {
            return Err(reason::status(Code::InvalidArgument, Reason::BadName, "bad branch name"));
        }
        let (bare, _id) = self.ensure(&repo.owner, &repo.slug).await?;
        let tip = with_repo(bare, move |repo| {
            let base = repo
                .refname_to_id(&format!("refs/heads/{from}"))
                .map_err(|_| reason::status(Code::NotFound, Reason::NotFound, "base branch not found"))?;
            let commit = repo.find_commit(base).map_err(internal)?;
            // force=false: существующая ветка → ошибка (already_exists наружу).
            // .map(|_| ()) сразу дропает Branch<'_> (заимствует repo).
            match repo.branch(&name, &commit, false).map(|_| ()) {
                Ok(()) => Ok(base.to_string()),
                Err(e) if e.code() == git2::ErrorCode::Exists => {
                    Err(reason::status(Code::AlreadyExists, Reason::Exists, "branch exists"))
                }
                Err(e) => Err(internal(e)),
            }
        })
        .await?;
        Ok(Response::new(BranchOpResponse { tip_sha: tip }))
    }

    async fn delete_branch(
        &self,
        req: Request<DeleteBranchRequest>,
    ) -> Result<Response<BranchOpResponse>, Status> {
        let DeleteBranchRequest { repo, name } = req.into_inner();
        let repo = repo.ok_or_else(|| Status::invalid_argument("repo required"))?;
        // Предусловие записи (ADR-0015): спрашиваем приложение ДО любой работы.
        crate::gate::ensure_writable(&repo.owner, &repo.slug).await?;
        if !valid_branch(&name) {
            return Err(reason::status(Code::InvalidArgument, Reason::BadName, "bad branch name"));
        }
        if name == "main" {
            return Err(reason::status(Code::FailedPrecondition, Reason::Protected, "main is protected"));
        }
        let (bare, _id) = self.ensure(&repo.owner, &repo.slug).await?;
        with_repo(bare, move |repo| {
            let mut branch = repo
                .find_branch(&name, git2::BranchType::Local)
                .map_err(|_| reason::status(Code::NotFound, Reason::NotFound, "branch not found"))?;
            branch.delete().map_err(internal)
        })
        .await?;
        Ok(Response::new(BranchOpResponse { tip_sha: String::new() }))
    }

    async fn merge_branch(
        &self,
        req: Request<MergeBranchRequest>,
    ) -> Result<Response<MergeBranchResponse>, Status> {
        let MergeBranchRequest { repo, name, mode, message } = req.into_inner();
        let squash = mode == "squash";
        let repo = repo.ok_or_else(|| Status::invalid_argument("repo required"))?;
        // Предусловие записи (ADR-0015): спрашиваем приложение ДО любой работы.
        crate::gate::ensure_writable(&repo.owner, &repo.slug).await?;
        if !valid_branch(&name) || name == "main" {
            return Err(reason::status(Code::InvalidArgument, Reason::BadName, "bad branch name"));
        }
        let (bare, id) = self.ensure(&repo.owner, &repo.slug).await?;
        // Merge двигает main → критическая секция с проекцией (как receive_pack).
        let _guard = repo::repo_guard(&self.pool, id).await.map_err(db_status)?;
        let (tip, ff) = with_repo(bare.clone(), move |repo| {
            // Упаковка — на ОБЩЕМ пути успеха, а не у каждого выхода. Выходов здесь три
            // (squash, fast-forward, merge-коммит), и первая версия правки поставила gc
            // только у последнего: squash уходит раньше и копил бы объекты дальше
            // (замечание авто-ревью на #106). Один вызов на все ветки не даст этому
            // повториться при четвёртом режиме слияния.
            let outcome = (|| -> Result<(String, bool), Status> {
                let branch_tip = repo
                    .refname_to_id(&format!("refs/heads/{name}"))
                    .map_err(|_| reason::status(Code::NotFound, Reason::NotFound, "branch not found"))?;
                let main_tip = repo.refname_to_id(MAIN_REF).map_err(internal)?;
                let (ahead, _behind) = repo.graph_ahead_behind(branch_tip, main_tip).map_err(internal)?;
                if ahead == 0 {
                    return Err(reason::status(
                        Code::FailedPrecondition,
                        Reason::NothingToMerge,
                        "branch is not ahead of base",
                    ));
                }
                // SQUASH: один коммит с ОДНИМ родителем (main). История ветки в main
                // не уезжает — это и есть смысл режима. Вклад авторов не теряется:
                // он переносится трейлерами Co-authored-by, как это делает GitHub.
                //
                // Проверка идёт ДО fast-forward: при squash даже перематываемую ветку
                // сплющиваем, иначе выбор режима работал бы через раз — в зависимости
                // от того, ушёл ли main вперёд.
                if squash {
                    let ours = repo.find_commit(main_tip).map_err(internal)?;
                    let theirs = repo.find_commit(branch_tip).map_err(internal)?;
                    // Дерево берём merge-ом, а не деревом ветки: main мог уйти вперёд,
                    // и дерево ветки откатило бы чужие изменения.
                    let mut idx = repo.merge_commits(&ours, &theirs, None).map_err(internal)?;
                    if idx.has_conflicts() {
                        return Err(reason::status(
                            Code::FailedPrecondition,
                            Reason::Conflict,
                            "merge does not apply cleanly",
                        ));
                    }
                    let tree_id = idx.write_tree_to(repo).map_err(internal)?;
                    let tree = repo.find_tree(tree_id).map_err(internal)?;
                    let title = if message.trim().is_empty() {
                        format!("Squashed branch '{name}'")
                    } else {
                        message.trim().to_string()
                    };
                    let msg = with_coauthors(repo, branch_tip, main_tip, &title);
                    let sig = merge_sig()?;
                    let squashed = repo.commit(None, &sig, &sig, &msg, &tree, &[&ours]).map_err(internal)?;
                    update_main(repo, squashed, Some(main_tip), &format!("merge {name}: squash"))
                        .map_err(main_status)?;
                    return Ok((squashed.to_string(), false));
                }
                // main — предок ветки → fast-forward: двигаем ref (через единую точку).
                if repo.graph_descendant_of(branch_tip, main_tip).unwrap_or(false) {
                    update_main(repo, branch_tip, Some(main_tip), &format!("merge {name}: fast-forward"))
                        .map_err(main_status)?;
                    return Ok((branch_tip.to_string(), true));
                }
                // Расхождение → merge-commit; конфликт индекса = failed_precondition.
                let ours = repo.find_commit(main_tip).map_err(internal)?;
                let theirs = repo.find_commit(branch_tip).map_err(internal)?;
                let mut idx = repo.merge_commits(&ours, &theirs, None).map_err(internal)?;
                if idx.has_conflicts() {
                    return Err(reason::status(
                        Code::FailedPrecondition,
                        Reason::Conflict,
                        "merge does not apply cleanly",
                    ));
                }
                let tree_id = idx.write_tree_to(repo).map_err(internal)?;
                let tree = repo.find_tree(tree_id).map_err(internal)?;
                let sig = merge_sig()?;
                let msg = format!("Merge branch '{name}'");
                let merged =
                    repo.commit(None, &sig, &sig, &msg, &tree, &[&ours, &theirs]).map_err(internal)?;
                update_main(repo, merged, Some(main_tip), &format!("merge {name}")).map_err(main_status)?;
                Ok((merged.to_string(), false))
            })();
            if outcome.is_ok() {
                crate::git::bundle::gc_auto(repo.path());
            }
            outcome
        })
        .await?;
        // main сдвинулся → проекция новой версии (0 = list.json не изменился).
        // Сбой проекции не отменяет merge, но громок — см. project_main_or_log.
        let new_version = project_main_or_log(&self.pool, id, &bare, "merge", &repo.owner, &repo.slug).await;
        spawn_mirror(self.pool.clone(), id, bare.clone()); // Ф3
        Ok(Response::new(MergeBranchResponse { tip_sha: tip, new_version, fast_forward: ff }))
    }

    async fn get_merge_state(
        &self,
        req: Request<MergeStateRequest>,
    ) -> Result<Response<MergeStateResponse>, Status> {
        let MergeStateRequest { repo, branch } = req.into_inner();
        let repo = repo.ok_or_else(|| Status::invalid_argument("repo required"))?;
        // Гейта записи здесь НЕТ намеренно: это чтение — три материализации для
        // сравнения. Смотреть на замороженный список можно, менять нельзя.
        if !valid_branch(&branch) || branch == "main" {
            return Err(reason::status(Code::InvalidArgument, Reason::BadName, "bad branch name"));
        }
        let (bare, _id) = self.ensure(&repo.owner, &repo.slug).await?;
        let state = tokio::task::spawn_blocking(move || -> Result<Option<(String, project::BranchSnapshotData, project::BranchSnapshotData, project::BranchSnapshotData)>, String> {
            let repo = git2::Repository::open_bare(&bare).map_err(|e| e.to_string())?;
            let Ok(branch_tip) = repo.refname_to_id(&format!("refs/heads/{branch}")) else {
                return Ok(None);
            };
            let main_tip = repo.refname_to_id(MAIN_REF).map_err(|e| e.to_string())?;
            let Ok(base_oid) = repo.merge_base(main_tip, branch_tip) else {
                return Ok(None);
            };
            drop(repo); // commit_snapshot открывает репо сам
            let base = project::commit_snapshot(&bare, &base_oid.to_string());
            let ours = project::branch_snapshot(&bare, MAIN_REF);
            let theirs = project::branch_snapshot(&bare, &format!("refs/heads/{branch}"));
            match (base, ours, theirs) {
                (Some(b), Some(o), Some(t)) => Ok(Some((base_oid.to_string(), b, o, t))),
                _ => Ok(None),
            }
        })
        .await
        .map_err(internal)?
        .map_err(Status::internal)?;
        let Some((base_sha, b, o, t)) = state else {
            return Ok(Response::new(MergeStateResponse { found: false, ..Default::default() }));
        };
        Ok(Response::new(MergeStateResponse {
            found: true,
            merge_base_sha: base_sha,
            base: Some(to_snapshot_pb(b)),
            ours: Some(to_snapshot_pb(o)),
            theirs: Some(to_snapshot_pb(t)),
        }))
    }

    async fn merge_resolved(
        &self,
        req: Request<MergeResolvedRequest>,
    ) -> Result<Response<MergeBranchResponse>, Status> {
        let MergeResolvedRequest { repo, branch, content, mode, message } = req.into_inner();
        let repo = repo.ok_or_else(|| Status::invalid_argument("repo required"))?;
        // Предусловие записи (ADR-0015): спрашиваем приложение ДО любой работы.
        crate::gate::ensure_writable(&repo.owner, &repo.slug).await?;
        if !valid_branch(&branch) || branch == "main" {
            return Err(reason::status(Code::InvalidArgument, Reason::BadName, "bad branch name"));
        }
        let (bare, id) = self.ensure(&repo.owner, &repo.slug).await?;
        // Канон собирает ядро: содержимое — из запроса, kind и надстройки блоков
        // (картинка/пометка, по идентичности) — из БД (Ф2a).
        let kind = db::load_list_kind(&self.pool, id).await.map_err(db_status)?;
        let carry = db::current_marks(&self.pool, id).await.map_err(db_status)?;
        let list_json = canon_list_json(content, kind, &carry)?;
        let _guard = repo::repo_guard(&self.pool, id).await.map_err(db_status)?;
        let tip = with_repo(bare.clone(), move |repo| {
            commit_resolved(repo, &branch, &list_json, mode == "squash", &message)
        })
        .await?;
        let new_version =
            project_main_or_log(&self.pool, id, &bare, "merge_resolved", &repo.owner, &repo.slug).await;
        spawn_mirror(self.pool.clone(), id, bare.clone()); // Ф3
        Ok(Response::new(MergeBranchResponse { tip_sha: tip, new_version, fast_forward: false }))
    }

    async fn create_tag(&self, req: Request<CreateTagRequest>) -> Result<Response<BranchOpResponse>, Status> {
        let CreateTagRequest { repo, name, version } = req.into_inner();
        let repo = repo.ok_or_else(|| Status::invalid_argument("repo required"))?;
        // Предусловие записи (ADR-0015): спрашиваем приложение ДО любой работы.
        crate::gate::ensure_writable(&repo.owner, &repo.slug).await?;
        if !valid_tag(&name) {
            return Err(reason::status(Code::InvalidArgument, Reason::BadName, "bad tag name"));
        }
        // Отдельным кодом от «плохого имени»: имя корректно, но принадлежит версиям.
        // Клиенту нужно показать РАЗНЫЕ подсказки, поэтому и сообщения разные.
        if is_version_tag(&name) {
            return Err(reason::status(Code::InvalidArgument, Reason::ReservedTagName, "reserved tag name"));
        }
        let (bare, _id) = self.ensure(&repo.owner, &repo.slug).await?;
        let sha = with_repo(bare, move |repo| {
            // Коммит версии: у каждой версии уже есть лёгкий тег vN.
            let target = repo
                .refname_to_id(&format!("refs/tags/v{version}"))
                .map_err(|_| Status::not_found("version not found"))?;
            let obj = repo.find_object(target, None).map_err(internal)?;
            // force=true: повторный релиз с тем же именем перевесит тег.
            repo.tag_lightweight(&name, &obj, true).map_err(internal)?;
            Ok(target.to_string())
        })
        .await?;
        // Ф3: релизный тег — тоже контент зеркала (refspec тянет все теги).
        let (bare_m, id_m) = self.ensure(&repo.owner, &repo.slug).await?;
        spawn_mirror(self.pool.clone(), id_m, bare_m);
        Ok(Response::new(BranchOpResponse { tip_sha: sha }))
    }

    async fn list_tags(&self, req: Request<RepoRef>) -> Result<Response<TagsResponse>, Status> {
        let RepoRef { owner, slug } = req.into_inner();
        let (bare, _id) = self.ensure(&owner, &slug).await?;
        let tags = with_repo(bare, |repo| {
            let mut out: Vec<Tag> = Vec::new();
            repo.tag_foreach(|oid, name_bytes| {
                if let Ok(name) = std::str::from_utf8(name_bytes) {
                    let name = name.strip_prefix("refs/tags/").unwrap_or(name).to_string();
                    // peel до коммита (лёгкий тег указывает прямо на коммит).
                    let sha = repo
                        .find_object(oid, None)
                        .and_then(|o| o.peel_to_commit())
                        .map(|c| c.id().to_string())
                        .unwrap_or_else(|_| oid.to_string());
                    out.push(Tag { name, target_sha: sha });
                }
                true
            })
            .map_err(internal)?;
            out.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(out)
        })
        .await?;
        Ok(Response::new(TagsResponse { tags }))
    }

    /// Обновить ветку из main — обратное слияние. main НЕ двигается, поэтому
    /// проекции версии нет: ветка это черновик, версии рождаются только из main.
    async fn update_branch(
        &self,
        req: Request<UpdateBranchRequest>,
    ) -> Result<Response<UpdateBranchResponse>, Status> {
        let UpdateBranchRequest { repo, name } = req.into_inner();
        let repo = repo.ok_or_else(|| Status::invalid_argument("repo required"))?;
        // Предусловие записи (ADR-0015): спрашиваем приложение ДО любой работы.
        crate::gate::ensure_writable(&repo.owner, &repo.slug).await?;
        if !valid_branch(&name) || name == "main" {
            return Err(reason::status(Code::InvalidArgument, Reason::BadName, "bad branch name"));
        }
        let (bare, id) = self.ensure(&repo.owner, &repo.slug).await?;
        // Под тем же локом, что merge/push: tip ветки нельзя читать до захвата —
        // конкурентный пуш иначе потерялся бы (та же причина, что в merge_branch).
        let _guard = repo::repo_guard(&self.pool, id).await.map_err(db_status)?;
        let (tip, ff) = with_repo(bare, move |repo| {
            let branch_ref = format!("refs/heads/{name}");
            let branch_tip = repo
                .refname_to_id(&branch_ref)
                .map_err(|_| reason::status(Code::NotFound, Reason::NotFound, "branch not found"))?;
            let main_tip = repo.refname_to_id(MAIN_REF).map_err(internal)?;
            // Ветка уже содержит main → обновлять нечего.
            if repo.graph_descendant_of(branch_tip, main_tip).unwrap_or(false) || branch_tip == main_tip {
                return Err(reason::status(
                    Code::FailedPrecondition,
                    Reason::NothingToMerge,
                    "branch is not ahead of base",
                ));
            }
            // Ветка — предок main (в ней нет своих коммитов) → просто двигаем ref.
            if repo.graph_descendant_of(main_tip, branch_tip).unwrap_or(false) {
                repo.reference(
                    &branch_ref,
                    main_tip,
                    true,
                    &format!("update {name} from main: fast-forward"),
                )
                .map_err(internal)?;
                return Ok((main_tip.to_string(), true));
            }
            // Расхождение → merge-commit В ВЕТКЕ: ours = ветка, theirs = main
            // (порядок обратный merge_branch — сливаем main в ветку, а не наоборот).
            let ours = repo.find_commit(branch_tip).map_err(internal)?;
            let theirs = repo.find_commit(main_tip).map_err(internal)?;
            let mut idx = repo.merge_commits(&ours, &theirs, None).map_err(internal)?;
            if idx.has_conflicts() {
                return Err(reason::status(
                    Code::FailedPrecondition,
                    Reason::Conflict,
                    "merge does not apply cleanly",
                ));
            }
            let tree_id = idx.write_tree_to(repo).map_err(internal)?;
            let tree = repo.find_tree(tree_id).map_err(internal)?;
            let sig = merge_sig()?;
            let merged = repo
                .commit(Some(&branch_ref), &sig, &sig, "Merge branch 'main'", &tree, &[&ours, &theirs])
                .map_err(internal)?;
            Ok((merged.to_string(), false))
        })
        .await?;
        Ok(Response::new(UpdateBranchResponse { tip_sha: tip, fast_forward: ff }))
    }

    /// Записать list.json в ветку одним коммитом («предложенные правки»).
    ///
    /// Отличие от merge_resolved: пишем в ВЕТКУ, main не двигается, родитель один
    /// — а значит и проекции версии здесь нет (версии рождаются только из main).
    async fn commit_to_branch(
        &self,
        req: Request<CommitToBranchRequest>,
    ) -> Result<Response<CommitToBranchResponse>, Status> {
        let CommitToBranchRequest { repo, branch, message, expected_tip, author_name, author_email, content } =
            req.into_inner();
        let repo = repo.ok_or_else(|| Status::invalid_argument("repo required"))?;
        // Предусловие записи (ADR-0015): спрашиваем приложение ДО любой работы.
        crate::gate::ensure_writable(&repo.owner, &repo.slug).await?;
        if !valid_branch(&branch) || branch == "main" {
            return Err(reason::status(Code::InvalidArgument, Reason::BadName, "bad branch name"));
        }
        let (bare, id) = self.ensure(&repo.owner, &repo.slug).await?;
        // Канон собирает ядро: содержимое — из запроса, kind и надстройки блоков
        // (картинка/пометка, по идентичности) — из БД (Ф2a).
        let kind = db::load_list_kind(&self.pool, id).await.map_err(db_status)?;
        let carry = db::current_marks(&self.pool, id).await.map_err(db_status)?;
        let list_json = canon_list_json(content, kind, &carry)?;
        // Тот же лок, что у merge/push: tip читаем ПОСЛЕ захвата, иначе
        // конкурентный пуш в ветку потерялся бы.
        let _guard = repo::repo_guard(&self.pool, id).await.map_err(db_status)?;
        let (tip, changed) = with_repo(bare, move |repo| {
            let author = opt(&author_name).zip(opt(&author_email));
            match write::commit_list_json(repo, &branch, &list_json, &message, &expected_tip, author) {
                Ok(write::WriteOutcome::Committed(sha)) => Ok((sha, true)),
                Ok(write::WriteOutcome::Unchanged(sha)) => Ok((sha, false)),
                Err(write::WriteError::NotFound) => {
                    Err(reason::status(Code::NotFound, Reason::NotFound, "branch not found"))
                }
                Err(write::WriteError::Stale) => Err(reason::status(
                    Code::FailedPrecondition,
                    Reason::Stale,
                    "branch tip moved since it was read",
                )),
                Err(write::WriteError::Git(e)) => Err(Status::internal(e)),
            }
        })
        .await?;
        Ok(Response::new(CommitToBranchResponse { tip_sha: tip, changed }))
    }

    /// Ф4, чтение: канон текстом — ровно то, что уехало бы в коммит.
    ///
    /// Собирается ТЕМ ЖЕ `canon_list_json`, что и запись в ветку: показать человеку
    /// один текст, а закоммитить другой — худшее, что может сделать редактор кода.
    /// Репозиторий здесь не создаётся: чтение не заводит на диске ничего нового.
    async fn render_canon(
        &self,
        req: Request<RenderCanonRequest>,
    ) -> Result<Response<RenderCanonResponse>, Status> {
        let RenderCanonRequest { repo, content } = req.into_inner();
        let id = self.list_id(repo).await?;
        let kind = db::load_list_kind(&self.pool, id).await.map_err(db_status)?;
        let carry = db::current_marks(&self.pool, id).await.map_err(db_status)?;
        let canon = canon_list_json(content, kind, &carry)?;
        let canon = String::from_utf8(canon).map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(RenderCanonResponse { canon }))
    }

    /// Ф4, запись: строгий разбор отредактированного текста.
    ///
    /// Придирки — ЛЕГИТИМНЫЙ ответ, а не gRPC-ошибка: это разбор пользовательского
    /// ввода, и вызывающему нужен весь список сразу, чтобы подсветить места в буфере.
    /// Ошибкой отвечаем только на то, что сломано у нас (нет списка, битая БД).
    async fn parse_canon(
        &self,
        req: Request<ParseCanonRequest>,
    ) -> Result<Response<ParseCanonResponse>, Status> {
        let ParseCanonRequest { repo, canon } = req.into_inner();
        // Список резолвим и здесь: разбор текста от имени несуществующего списка —
        // ошибка вызывающего, и узнать о ней лучше до правки, а не при сохранении.
        self.list_id(repo).await?;
        match crate::git::canon::parse_canon(&canon) {
            Ok(parts) => Ok(Response::new(ParseCanonResponse {
                issues: Vec::new(),
                content: Some(list_content_from_parts(parts)),
            })),
            Err(issues) => Ok(Response::new(ParseCanonResponse {
                issues: issues
                    .into_iter()
                    .map(|i| CanonIssue {
                        path: i.path,
                        code: i.code.as_str().to_string(),
                        message: i.message,
                        line: i.line,
                        column: i.column,
                    })
                    .collect(),
                content: None,
            })),
        }
    }

    /// Ф3: пуш зеркала по запросу (кнопка «Синхронизировать», после сохранения
    /// настроек). Исход в теле ответа — текст ошибки показывается владельцу.
    async fn mirror_push(&self, req: Request<RepoRef>) -> Result<Response<MirrorPushResponse>, Status> {
        let RepoRef { owner, slug } = req.into_inner();
        let (bare, id) = self.ensure(&owner, &slug).await?;
        match push_mirror_now(&self.pool, id, &bare).await {
            Ok(()) => Ok(Response::new(MirrorPushResponse { ok: true, error: String::new() })),
            Err(e) => Ok(Response::new(MirrorPushResponse { ok: false, error: e })),
        }
    }

    /// Ф2: проверка доступа к зеркалу БЕЗ пуша — кнопка «Проверить доступ».
    ///
    /// Статус зеркала здесь НЕ трогаем, в отличие от пуша: проверка ничего не
    /// меняет на фордже и не имеет права выдавать себя за попытку синхронизации.
    /// Иначе «проверил и ушёл» отодвигало бы настоящий повтор (подметальщик
    /// считает паузу от времени последней попытки) и сбивало счётчик неудач.
    async fn mirror_check(&self, req: Request<RepoRef>) -> Result<Response<MirrorCheckResponse>, Status> {
        let RepoRef { owner, slug } = req.into_inner();
        let (bare, id) = self.ensure(&owner, &slug).await?;
        // Исход — в теле, как у пуша: «токен не подошёл» это legitimate ответ
        // владельцу, а не сбой RPC.
        let body = |error: Option<String>| {
            Ok(Response::new(match error {
                None => MirrorCheckResponse { ok: true, error: String::new() },
                Some(e) => MirrorCheckResponse { ok: false, error: e },
            }))
        };
        let Some((url, token_enc)) = db::load_mirror(&self.pool, id).await.map_err(db_status)? else {
            return body(Some("зеркало не настроено".to_string()));
        };
        let Some(secret) = crate::git::mirror::mirror_secret() else {
            return body(Some("SETFORK_MIRROR_SECRET не задан на сервере".to_string()));
        };
        let Some(token) = crate::git::mirror::decrypt_token(&token_enc, secret) else {
            return body(Some(
                "токен зеркала не расшифровался (секрет сменён?) — сохраните заново".to_string(),
            ));
        };
        body(crate::git::mirror::mirror_check(&bare, &url, &token).await.err())
    }

    /// Коммиты рефа (свежие первыми). `not_in` скрывает достижимое из базы —
    /// так вкладка «Коммиты» показывает ровно вклад ветки, а не всю историю.
    async fn list_commits(
        &self,
        req: Request<ListCommitsRequest>,
    ) -> Result<Response<CommitsResponse>, Status> {
        let ListCommitsRequest { repo, rev, not_in, limit } = req.into_inner();
        let RepoRef { owner, slug } = repo.ok_or_else(|| Status::invalid_argument("repo required"))?;
        if rev.is_empty() {
            return Err(Status::invalid_argument("rev required"));
        }
        let (bare, _id) = self.ensure(&owner, &slug).await?;
        let take = if limit <= 0 { 100 } else { limit.min(500) } as usize;
        let found =
            with_repo(bare, move |repo| history::commits(repo, &rev, &not_in, take).map_err(internal))
                .await?;
        // Несуществующий реф — не ошибка: ветку могли удалить, UI покажет пусто.
        let commits = found
            .iter()
            .flatten()
            .map(|c| Commit {
                sha: c.sha.clone(),
                message: c.message.clone(),
                author_name: c.author_name.clone(),
                author_email: c.author_email.clone(),
                at_unix: c.at_unix,
                parents: c.parents,
            })
            .collect();
        Ok(Response::new(CommitsResponse { found: found.is_some(), commits }))
    }
}
