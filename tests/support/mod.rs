//! Общий support тест-бинарей: каждый использует своё подмножество хелперов.
#![allow(dead_code)]
//! Тестовая БД для интеграционных тестов: TEST_DATABASE_URL + одноразовая
//! схема со случайным именем (search_path пула) — параллельные тесты не
//! мешают друг другу, дроп не нужен (БД эфемерная, см. scripts/ci-local.sh).
//!
//! DDL ниже — СНИМОК того, что ядро реально ожидает от Postgres фронта
//! (только столбцы/enum'ы, которых касается Rust-код). Источник правды по
//! схеме — drizzle в setfork-frontend; разъезд снимка с реальной схемой
//! ловится golden-сверкой и прод-интеграцией, а этот снимок документирует
//! контракт ядра и делает тесты автономными.

/// Замок на GIT_DATA_DIR.
///
/// Переменная принадлежит ПРОЦЕССУ, а не тесту: параллельные тесты одного бинаря
/// перетирали бы её друг у друга, и репозиторий создавался бы не там, где его потом ищут —
/// падение выглядело бы как «репозиторий пропал». Держится до конца теста, поэтому пробы
/// безопасны и без `--test-threads=1`.
pub static GIT_DATA_DIR_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
use sqlx::postgres::{PgPool, PgPoolOptions};
use uuid::Uuid;

const DDL: &str = r#"
create type step_level as enum ('required','recommended','optional');
create type list_visibility as enum ('public','private');
create type list_status as enum ('draft','published');
create type template_origin as enum ('authored','forked','ai_draft');
-- Имя и состав ровно как в drizzle-схеме фронта (moderation_status, четыре значения):
-- снимок с именем list_moderation и без 'pending' разъехался с источником правды, а
-- ядро с 04.08 пишет это состояние вставкой и обязано попадать в НАСТОЯЩИЙ тип.
create type moderation_status as enum ('active','pending','flagged','hidden');
create type issue_status as enum ('open','closed');
create type suggestion_status as enum ('open','accepted','rejected');

create table users (
  id uuid primary key default gen_random_uuid(),
  handle text not null unique,
  avatar_url text
);

create table templates (
  id uuid primary key default gen_random_uuid(),
  owner_id uuid not null references users(id),
  slug text not null,
  title jsonb not null default '{}'::jsonb,
  "desc" jsonb not null default '{}'::jsonb,
  tags text[] not null default '{}',
  ordered boolean not null default true,
  status list_status not null default 'published',
  visibility list_visibility not null default 'public',
  moderation moderation_status not null default 'active',
  moderation_reason text,
  verified boolean not null default false,
  pinned boolean not null default false,
  origin template_origin not null default 'authored',
  forked_from_id uuid,
  -- Зеркало (Ф3): url + шифрованный токен + статус последнего пуша.
  mirror_url text,
  mirror_token text,
  mirror_synced_at timestamptz,
  mirror_error text,
  -- Ф2: неудач подряд; ведёт ядро, читает подметальщик ретраев во фронте.
  mirror_attempts integer not null default 0,
  -- Тип списка (ADR-0010); во фронт-схеме text nullable, БЕЗ enum (Ф2a).
  list_kind text,
  current_version integer not null default 1,
  stars_count integer not null default 0,
  forks_count integer not null default 0,
  runs_count integer not null default 0,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  unique (owner_id, slug)
);

create table template_versions (
  id uuid primary key default gen_random_uuid(),
  template_id uuid not null references templates(id),
  version integer not null,
  note text not null default '',
  author_id uuid references users(id),
  created_at timestamptz not null default now(),
  unique (template_id, version)
);

create table steps (
  id uuid primary key default gen_random_uuid(),
  version_id uuid not null references template_versions(id),
  n integer not null,
  -- Стабильная идентичность блока сквозь версии (nullable — старые строки).
  block_id uuid,
  "type" text not null default 'step',
  content jsonb not null default '{}'::jsonb,
  title jsonb not null default '{}'::jsonb,
  "desc" jsonb not null default '{}'::jsonb,
  command text not null default '',
  has_image boolean not null default false,
  image_key text,
  level step_level not null default 'required',
  why jsonb not null default '{}'::jsonb,
  section jsonb not null default '{}'::jsonb,
  subtasks jsonb not null default '[]'::jsonb,
  refs jsonb not null default '[]'::jsonb,
  -- «Здесь нужен человек»: место, где машина знать не может (цены, вкус, опыт).
  -- Зеркало фронт-схемы; расхождение тестовой копии с настоящей = ложно-зелёные тесты.
  needs_human boolean not null default false,
  needs_human_ask jsonb not null default '{}'::jsonb,
  -- Разрушительный пункт: команда необратима (сносит данные, тома, окружение).
  -- Пункт с пометкой не попадает в собранный скрипт исполняемым.
  danger boolean not null default false
);

create table stars (
  user_id uuid not null references users(id),
  template_id uuid not null references templates(id),
  created_at timestamptz not null default now(),
  primary key (user_id, template_id)
);

create table watches (
  user_id uuid not null references users(id),
  template_id uuid not null references templates(id),
  created_at timestamptz not null default now(),
  primary key (user_id, template_id)
);

create table issues (
  id uuid primary key default gen_random_uuid(),
  template_id uuid not null references templates(id),
  author_id uuid not null references users(id),
  number integer not null,
  title text not null,
  body text not null default '',
  labels jsonb not null default '[]'::jsonb,
  status issue_status not null default 'open',
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  closed_at timestamptz
);

create table issue_comments (
  id uuid primary key default gen_random_uuid(),
  issue_id uuid not null references issues(id),
  author_id uuid not null references users(id),
  body text not null,
  created_at timestamptz not null default now()
);

create table suggestions (
  id uuid primary key default gen_random_uuid(),
  template_id uuid not null references templates(id),
  author_id uuid not null references users(id),
  note text not null default '',
  -- Номер правки в рамках списка (#12) — как у задач.
  number integer,
  base_version integer not null,
  items jsonb not null default '[]'::jsonb,
  status suggestion_status not null default 'open',
  created_at timestamptz not null default now(),
  resolved_at timestamptz
);

create table suggestion_comments (
  id uuid primary key default gen_random_uuid(),
  suggestion_id uuid not null references suggestions(id),
  author_id uuid not null references users(id),
  body text not null,
  created_at timestamptz not null default now()
);
"#;

/// Пул на свежую случайную схему с применённым DDL-снимком.
/// Паникует с внятным сообщением, если TEST_DATABASE_URL не задан/недоступен.
pub async fn pool_with_schema() -> PgPool {
    // ПЕРВОЙ строкой, до любого чтения env. `ensure_git_data_dir` пишет переменную
    // окружения, а `set_var` в Rust 2024 небезопасен не из-за повторной записи (её
    // сторожит `Once`), а из-за ОДНОВРЕМЕННОГО ЧТЕНИЯ из другого потока: glibc может
    // переселить блок `environ`, и параллельный `getenv` прочитает освобождённую
    // память. Тесты внутри одного бинаря идут параллельно, и до этой правки картина
    // была ровно такой: `review_p1` — пять тестов, один зовёт `ensure_git_data_dir`,
    // остальные четыре в этот момент читают `TEST_DATABASE_URL` здесь же.
    //
    // Проведя запись через тот же `Once`, через который проходят ВСЕ тесты, мы
    // получаем happens-before: чтения начинаются только после того, как запись
    // завершена (линза 07 §7).
    ensure_git_data_dir();
    let url = std::env::var("TEST_DATABASE_URL").expect(
        "TEST_DATABASE_URL не задан — интеграционные тесты требуют Postgres \
         (bash scripts/ci-local.sh поднимет эфемерный)",
    );
    // Прогон против НАСТОЯЩЕЙ схемы фронта (линза 04 §4): снимок ниже — это то, что
    // ядро ОЖИДАЕТ от Postgres, и совпадение с источником правды (drizzle) он сам не
    // доказывает. С `TEST_USE_EXISTING_SCHEMA=1` тесты идут по схеме, накатанной
    // `drizzle-kit push` из фронта, и всё, что упало, — настоящий разъезд.
    // Схема при этом ОБЩАЯ, поэтому так гоняют разово и на одноразовой базе.
    if std::env::var("TEST_USE_EXISTING_SCHEMA").is_ok() {
        return PgPoolOptions::new()
            .max_connections(5)
            .connect(&url)
            .await
            .expect("пул на существующей схеме");
    }
    let schema = format!("t_{}", Uuid::new_v4().simple());

    let admin =
        PgPoolOptions::new().max_connections(1).connect(&url).await.expect("подключение к TEST_DATABASE_URL");
    // AssertSqlSafe: имя схемы — наш uuid, не пользовательский ввод.
    sqlx::query(sqlx::AssertSqlSafe(format!("create schema \"{schema}\"")))
        .execute(&admin)
        .await
        .expect("create schema");

    // search_path пула — наша схема: и таблицы, и enum-касты резолвятся в неё.
    let sep = if url.contains('?') { '&' } else { '?' };
    let url_sp = format!("{url}{sep}options=-csearch_path%3D{schema}");
    let pool =
        PgPoolOptions::new().max_connections(5).connect(&url_sp).await.expect("подключение тестового пула");
    sqlx::raw_sql(DDL).execute(&pool).await.expect("применение DDL-снимка");
    pool
}

/// GIT_DATA_DIR для тестов, зовущих git-first запись (ListWrite.add_version):
/// один временный корень на тест-бинарь. Env процесс-глобален, поэтому ставится
/// один раз; каталоги репо мелкие и живут в системном temp.
///
/// Зовётся из `pool_with_schema` ПЕРВОЙ строкой — этого достаточно, чтобы запись
/// произошла раньше любых чтений env в этом бинаре. Явные вызовы в тестах оставлены:
/// они безвредны (`Once`) и показывают в самом тесте, что он ходит в git.
pub fn ensure_git_data_dir() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let root = std::env::temp_dir().join(format!("setfork-gitdata-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).expect("create GIT_DATA_DIR");
        // SAFETY: единственная запись env в этом бинаре, под Once, до git-вызовов.
        unsafe { std::env::set_var("GIT_DATA_DIR", &root) };
    });
}

/// Собственный `GIT_DATA_DIR` для пробы: свой каталог, замок на время теста и уборка.
///
/// Заменил две одинаковые копии `git_data_dir()` в `lock_probe` и `merge_state_probe`.
/// Копий было две, и в них жила ловушка, которую видно только вместе: `ensure_git_data_dir`
/// пишет переменную ОДИН раз под `Once`, а `pool_with_schema` теперь зовёт его первой
/// строкой. Проба, поставившая свой каталог ДО первого обращения к пулу, получала бы его
/// затёртым на следующей же строке — и искала бы репозиторий там, где его нет.
///
/// Поэтому здесь `Once` сначала ОСУШАЕТСЯ, и только потом ставится свой каталог: дальше
/// вызовы `ensure_git_data_dir` из пула уже ничего не делают.
pub async fn own_git_data_dir(prefix: &str) -> OwnGitDataDir {
    ensure_git_data_dir();
    let lock = GIT_DATA_DIR_LOCK.lock().await;
    let path = std::env::temp_dir().join(format!("setfork-{prefix}-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&path).expect("mkdir GIT_DATA_DIR пробы");
    // SAFETY: замок держится до конца теста, а `Once` выше уже отработал — значит эта
    // запись последняя и никакой другой поток env в этот момент не пишет.
    unsafe { std::env::set_var("GIT_DATA_DIR", &path) };
    OwnGitDataDir { path, _lock: lock }
}

pub struct OwnGitDataDir {
    pub path: std::path::PathBuf,
    _lock: tokio::sync::MutexGuard<'static, ()>,
}

impl Drop for OwnGitDataDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Пользователь-фикстура; возвращает id.
pub async fn seed_user(pool: &PgPool, handle: &str) -> Uuid {
    sqlx::query_scalar("insert into users (handle) values ($1) returning id")
        .bind(handle)
        .fetch_one(pool)
        .await
        .expect("seed user")
}
