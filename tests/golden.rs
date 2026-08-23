//! Golden-фикстуры на ДЕТЕРМИНИРОВАННОМ датасете (фиксированные uuid/даты):
//! канонический domain-read JSON, list.json/README материализации и tip-SHA
//! bare-репо (фиксированный автор + ts ⇒ SHA воспроизводимы байт-в-байт).
//!
//! Ловит регрессии Rust-стороны. Паритет с TS дополнительно сверяется
//! скриптами фронта (scripts/golden-domain-read.ts) — тот путь кросс-репный
//! и остаётся ручным.
//!
//! Обновление фикстур — В ДВА ШАГА (см. `compare_or_update`):
//!   1) UPDATE_GOLDEN=1      TEST_DATABASE_URL=... cargo test --test golden -- --include-ignored
//!      — показывает, ЧТО изменилось, и падает;
//!   2) UPDATE_GOLDEN=accept TEST_DATABASE_URL=... cargo test --test golden -- --include-ignored
//!      — принимает показанное.
mod support;

use setfork_core::{db, git::bundle, services};
use sqlx::PgPool;

const OWNER_ID: &str = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
const LIST_ID: &str = "11111111-2222-3333-4444-555555555555";
const V1_ID: &str = "10000000-0000-0000-0000-000000000001";
const V2_ID: &str = "20000000-0000-0000-0000-000000000002";

async fn seed_fixed(pool: &PgPool) {
    // type/content засеваются ЯВНО ('step'/{}), а не NULL: в схеме фронта обе колонки
    // NOT NULL, и фикстура в форме NULL описывала строку, которой в настоящей базе
    // быть не может (линза 04 §1). На вывод это не влияет — 'step' и NULL значат для
    // канона одно и то же, — но фикстура снова описывает реальность.
    // created_at фиксированы: ts версии участвует в SHA коммита материализации.
    let sql = format!(
        r#"
insert into users (id, handle) values ('{OWNER_ID}', 'golden');
insert into templates (id, owner_id, slug, title, "desc", tags, ordered, current_version, created_at, updated_at)
values ('{LIST_ID}', '{OWNER_ID}', 'golden-list',
        '{{"en":"Golden List"}}', '{{"en":"Fixed dataset for golden tests"}}',
        array['redis','cache'], true, 2,
        '2026-01-02T03:04:05Z', '2026-01-03T04:05:06Z');
insert into template_versions (id, template_id, version, note, created_at) values
 ('{V1_ID}', '{LIST_ID}', 1, 'initial',    '2026-01-02T03:04:05Z'),
 ('{V2_ID}', '{LIST_ID}', 2, 'add config', '2026-01-02T10:20:30Z');
insert into steps (id, version_id, n, "type", content, title, "desc", command, level, why, section, subtasks, refs) values
 ('30000000-0000-0000-0000-000000000001', '{V1_ID}', 1, 'step', '{{}}', '{{"en":"Install Redis"}}', '{{"en":"Grab the binary"}}', 'apt install redis',
  'required', '{{"en":"speed"}}', '{{"en":"Setup"}}',
  '[{{"en":"check version"}}]', '[{{"label":{{"en":"docs"}},"url":"https://redis.io"}}]'),
 ('30000000-0000-0000-0000-000000000002', '{V2_ID}', 1, 'step', '{{}}', '{{"en":"Install Redis"}}', '{{"en":"Grab the binary"}}', 'apt install redis',
  'required', '{{"en":"speed"}}', '{{"en":"Setup"}}', '[]', '[]'),
 ('30000000-0000-0000-0000-000000000003', '{V2_ID}', 2, 'text', '{{"md":"Intro **context**"}}', '{{}}', '{{}}', '',
  'required', '{{}}', '{{}}', '[]', '[]'),
 ('30000000-0000-0000-0000-000000000004', '{V2_ID}', 3, 'step', '{{}}', '{{"en":"Configure"}}', '{{}}', 'redis-cli config set',
  'recommended', '{{}}', '{{"en":"Setup"}}', '[]', '[]');
"#
    );
    // AssertSqlSafe: датасет статический, подстановки — константы этого файла.
    sqlx::raw_sql(sqlx::AssertSqlSafe(sql)).execute(pool).await.expect("seed fixed dataset");
}

/// Показывает первые расхождения построчно — чтобы принимать было ЧТО смотреть.
fn diff_lines(expected: &str, actual: &str) -> String {
    let (mut out, mut shown) = (String::new(), 0);
    let (e, a): (Vec<_>, Vec<_>) = (expected.lines().collect(), actual.lines().collect());
    for i in 0..e.len().max(a.len()) {
        let (was, now) =
            (e.get(i).copied().unwrap_or("<нет строки>"), a.get(i).copied().unwrap_or("<нет строки>"));
        if was == now {
            continue;
        }
        out.push_str(&format!("  строка {}:\n    было:  {was}\n    стало: {now}\n", i + 1));
        shown += 1;
        if shown == 10 {
            out.push_str("  … остальные расхождения не показаны\n");
            break;
        }
    }
    if out.is_empty() {
        "  (различий по строкам нет — расходятся окончания строк или хвост файла)\n".into()
    } else {
        out
    }
}

/// Сравнивает с фикстурой (нормализуя CRLF) или обновляет её — В ДВА ШАГА.
///
/// Обновление намеренно НЕ делается одним движением. Golden-фикстуры — это байтовая
/// сверка с TS, то есть единственное место, где ловится разъезд двух реализаций
/// формата. Флаг, который молча переписывает эталон, позволяет узаконить сломанный
/// вывод одним нажатием — и тогда проверка перестаёт быть проверкой (линза 07 §3;
/// у скриншот-эталонов фронта процедура двухшаговая ровно поэтому).
///
/// Шаг 1: `UPDATE_GOLDEN=1` — показать, ЧТО изменилось, и упасть.
/// Шаг 2: `UPDATE_GOLDEN=accept` — принять показанное.
/// Расхождения копятся, а не роняют прогон на первом же.
///
/// Иначе показ обрывается на первой фикстуре, остальные человек не видит — и
/// следующий шаг («принять») запишет в том числе то, чего ему не показывали.
/// Тогда двухшаговость перестаёт быть двухшаговостью (P1 авто-ревью на #102).
static РАСХОЖДЕНИЯ: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

fn compare_or_update(rel: &str, actual: &str) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    let mode = std::env::var("UPDATE_GOLDEN").unwrap_or_default();
    let existing = std::fs::read_to_string(&path).ok();

    // Фикстуры ещё нет: сверять не с чем, показывать нечего — пишем сразу.
    if existing.is_none() && !mode.is_empty() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, actual).unwrap();
        eprintln!("golden: создан {rel}");
        return;
    }

    let expected = existing.unwrap_or_else(|| {
        panic!(
            "нет фикстуры {rel} — сгенерируй: UPDATE_GOLDEN=1 cargo test --test golden -- --include-ignored"
        )
    });
    let (e, a) = (expected.replace("\r\n", "\n"), actual.replace("\r\n", "\n"));
    if e == a {
        return; // совпало — принимать нечего, файл не трогаем даже в режиме accept
    }
    if mode == "accept" {
        std::fs::write(&path, actual).unwrap();
        eprintln!("golden: обновлён {rel}");
        return;
    }
    РАСХОЖДЕНИЯ
        .lock()
        .expect("копилка расхождений")
        .push(format!("golden-фикстура {rel} РАЗОШЛАСЬ. Что именно:\n{}", diff_lines(&e, &a)));
}

/// Зовётся В КОНЦЕ проверки: только здесь видно ВСЕ расхождения разом.
fn assert_golden_ok() {
    let собранное = std::mem::take(&mut *РАСХОЖДЕНИЯ.lock().expect("копилка расхождений"));
    if собранное.is_empty() {
        return;
    }
    panic!(
        "{}\nЭто либо починка формата, либо поломка — решает человек.\n\
         Принять ПОКАЗАННОЕ ВЫШЕ: UPDATE_GOLDEN=accept cargo test --test golden -- --include-ignored",
        собранное.join("\n")
    );
}

#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres)"]
async fn golden_domain_read_and_materialization() {
    let pool = support::pool_with_schema().await;
    seed_fixed(&pool).await;

    // 1. Канонический domain-read JSON (тот же код-пас, что CLI `domain-read`).
    let json = services::golden::golden_json(&pool, "golden", "golden-list").await.expect("golden_json");
    compare_or_update(
        "tests/fixtures/golden-domain-read.json",
        &format!("{}\n", serde_json::to_string_pretty(&json).unwrap()),
    );

    // 2. Материализация версий: list.json и README последней версии.
    let (id, cur) =
        db::resolve_list(&pool, "golden", "golden-list").await.expect("resolve").expect("список есть");
    assert_eq!(cur, 2);
    let versions = db::load_bundle_data(&pool, id).await.expect("load_bundle_data");
    assert_eq!(versions.len(), 2);
    let files = bundle::version_files(&versions[1]);
    let list_json = &files.iter().find(|(p, _)| p == "list.json").expect("list.json").1;
    let readme = &files.iter().find(|(p, _)| p == "README.md").expect("README").1;
    compare_or_update("tests/fixtures/golden-list.json", list_json);
    compare_or_update("tests/fixtures/golden-README.md", readme);

    // 3. Bare-репо: фиксированный автор и ts ⇒ детерминированный tip-SHA.
    let tmp = std::env::temp_dir().join(format!("setfork-golden-{}", uuid::Uuid::new_v4()));
    let bare = tmp.join("repo.git");
    bundle::bootstrap_bare(&versions, &bare).expect("bootstrap_bare");
    assert_eq!(bundle::max_tag_version(&bare), 2, "теги v1/v2 на месте");
    let repo = git2::Repository::open_bare(&bare).expect("open bare");
    let tip = repo.refname_to_id("refs/heads/main").expect("main tip").to_string();
    drop(repo);
    let _ = std::fs::remove_dir_all(&tmp);
    compare_or_update("tests/fixtures/golden-tip.txt", &format!("{tip}\n"));

    // ВСЕ расхождения — одним отказом и в конце: иначе человек увидит первое и
    // примет вслепую остальные.
    assert_golden_ok();
}
