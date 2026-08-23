//! Ф0 трека git-surface: границы приёма объявлены явно.
//!
//! Проверяем ОБА пути записи, потому что правила у них разные по механике:
//! `pre-receive` ловит настоящий `git push`, `update_main` — программную запись
//! через git2, которая идёт мимо хука. Плюс эмпирическая проверка родного
//! `receive.maxInputSize`: код на него опирается, значит поведение должно быть
//! доказано, а не взято из документации на веру.
//!
//! Тесты не трогают БД — только git, поэтому идут в обычном `cargo test`.
use std::path::{Path, PathBuf};
use std::process::Command;

use setfork_core::git::bundle::install_hook;
use setfork_core::git::serialize::tree_path_allowed;
use setfork_core::git::update::{MainUpdateError, update_main};

/// Каталог-однодневка: удаляется на Drop (в т.ч. при panic внутри теста).
struct Tmp(PathBuf);
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn tmp(prefix: &str) -> Tmp {
    let p = std::env::temp_dir().join(format!("setfork-{prefix}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&p).expect("create tmp");
    Tmp(p)
}

/// Git от лица ВЛАДЕЛЬЦА списка: подготовка репозитория и пуши в main.
///
/// Ф5 сделала роль значимой: её отсутствие хук трактует как «посторонний» и в
/// main не пускает. Это и есть безопасный дефолт, но тест, изображающий
/// владельца, обязан теперь сказать об этом вслух — раньше хватало умолчания.
fn git(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .current_dir(dir)
        .env("SETFORK_ROLE", "owner")
        .args(args)
        .output()
        .expect("git запустился")
}

fn git_ok(dir: &Path, args: &[&str]) {
    let out = git(dir, args);
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

/// Bare с установленными правилами + рабочая копия с одной версией в main.
fn repo_pair(root: &Path) -> (PathBuf, PathBuf) {
    let bare = root.join("bare.git");
    git_ok(root, &["init", "-q", "--bare", "--initial-branch=main", bare.to_str().unwrap()]);
    install_hook(&bare).expect("хук установлен");

    let work = root.join("work");
    std::fs::create_dir_all(&work).expect("mkdir work");
    git_ok(root, &["init", "-q", "--initial-branch=main", work.to_str().unwrap()]);
    git_ok(&work, &["config", "user.email", "t@example.com"]);
    git_ok(&work, &["config", "user.name", "Тест"]);
    git_ok(&work, &["remote", "add", "origin", bare.to_str().unwrap()]);
    std::fs::write(work.join("list.json"), b"{\"title\":\"t\",\"steps\":[]}").expect("list.json");
    std::fs::write(work.join("README.md"), b"# t\n").expect("README");
    git_ok(&work, &["add", "-A"]);
    git_ok(&work, &["commit", "-q", "-m", "v1"]);
    git_ok(&work, &["push", "-q", "origin", "main"]);
    (bare, work)
}

#[test]
fn хук_отвергает_посторонний_файл_и_называет_его() {
    let root = tmp("tree-hook");
    let (_bare, work) = repo_pair(&root.0);

    std::fs::create_dir_all(work.join("assets")).expect("mkdir");
    std::fs::write(work.join("assets/big.bin"), vec![0u8; 1024]).expect("blob");
    git_ok(&work, &["add", "-A"]);
    git_ok(&work, &["commit", "-q", "-m", "мусор"]);

    let out = git(&work, &["push", "origin", "main"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "пуш с лишним файлом обязан быть отвергнут: {err}");
    assert!(err.contains("assets/big.bin"), "отказ обязан НАЗВАТЬ путь, а не только факт: {err}");
}

// Мусор в промежуточном коммите — главный смысл проверки по всем новым объектам,
// а не по дереву вершины: чистый tip оставлял бы блоб достижимым из истории, и
// зеркало вывезло бы его на чужую форджу.
#[test]
fn хук_видит_мусор_в_промежуточном_коммите() {
    let root = tmp("tree-mid");
    let (_bare, work) = repo_pair(&root.0);

    std::fs::write(work.join("secret.env"), b"TOKEN=1").expect("blob");
    git_ok(&work, &["add", "-A"]);
    git_ok(&work, &["commit", "-q", "-m", "промежуточный с мусором"]);
    std::fs::remove_file(work.join("secret.env")).expect("rm");
    git_ok(&work, &["add", "-A"]);
    git_ok(&work, &["commit", "-q", "-m", "вершина чистая"]);

    let out = git(&work, &["push", "origin", "main"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "чистая вершина не должна пропускать мусор из истории: {err}");
    assert!(err.contains("secret.env"), "{err}");
}

// Легаси: у репо, в которые не писали после Ф2b, steps/*.md всё ещё в дереве.
// Жёсткий отказ ломал бы пуш из такого клона на ровном месте.
#[test]
fn хук_пропускает_легаси_steps() {
    let root = tmp("tree-legacy");
    let (_bare, work) = repo_pair(&root.0);

    std::fs::create_dir_all(work.join("steps")).expect("mkdir");
    std::fs::write(work.join("steps/01-first.md"), b"# First\n").expect("step");
    git_ok(&work, &["add", "-A"]);
    git_ok(&work, &["commit", "-q", "-m", "легаси-дерево"]);

    let out = git(&work, &["push", "origin", "main"]);
    assert!(
        out.status.success(),
        "легаси steps/ обязаны проходить: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// Регрессия P1 авто-ревью core#70. Первая версия хука перечисляла ОБЪЕКТЫ
// (`rev-list --objects`), а он печатает каждый OID один раз: лишний файл с теми
// же байтами, что у разрешённого, не появлялся в выводе вовсе и проезжал.
// Проверено вживую: у одинаковых README.md и evil выводился только README.md.
#[test]
fn хук_ловит_лишний_файл_с_содержимым_разрешённого() {
    let root = tmp("tree-dedup");
    let (_bare, work) = repo_pair(&root.0);

    let same = std::fs::read(work.join("README.md")).expect("читаем README");
    std::fs::write(work.join("evil"), &same).expect("тот же блоб под другим именем");
    git_ok(&work, &["add", "-A"]);
    git_ok(&work, &["commit", "-q", "-m", "дубль блоба"]);

    let out = git(&work, &["push", "origin", "main"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "дубль блоба под чужим именем обязан быть отвергнут: {err}");
    assert!(err.contains("evil"), "{err}");
}

// Регрессия P2 авто-ревью: `steps` разрешался как ИМЯ, поэтому обычный файл с
// таким именем в корне проходил обе проверки и оставался в дереве незамеченным.
#[test]
fn обычный_файл_с_именем_steps_отвергается() {
    assert!(!tree_path_allowed("steps"), "каталог судится по содержимому, файл — сам по себе");

    let root = tmp("tree-stepsfile");
    let (_bare, work) = repo_pair(&root.0);
    std::fs::write(work.join("steps"), "не каталог, а обычный файл\n").expect("файл steps");
    git_ok(&work, &["add", "-A"]);
    git_ok(&work, &["commit", "-q", "-m", "файл steps"]);

    let out = git(&work, &["push", "origin", "main"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "файл steps в корне обязан быть отвергнут: {err}");
}

// Имена версий принадлежат серверу — и на ОБЕИХ дверях. На веб-двери правило
// стояло с самого начала (`ReservedTagName`), а через git владелец мог поставить
// `v99` руками: старший тег становился 99 при текущей версии 1, выравнивание
// объявляло Conflict, и запись в список останавливалась до оператора
// (05-F2, замер соседней сессии).
#[test]
fn тег_с_именем_версии_не_принимается_пушем() {
    let root = tmp("tree-vtag");
    let (_bare, work) = repo_pair(&root.0);
    std::fs::write(work.join("list.json"), "{}").ok(); // содержимое не важно: судим ИМЯ рефа
    git_ok(&work, &["tag", "v99"]);
    git_ok(&work, &["tag", "v1.0"]);
    git_ok(&work, &["tag", "release-1"]);

    let out = git(&work, &["push", "origin", "refs/tags/v99"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "имя версии обязано быть отвергнуто: {err}");
    assert!(err.contains("v99"), "отказ обязан назвать сам тег: {err}");

    // Человеческие имена остаются доступными — правило про «v и только цифры».
    for name in ["v1.0", "release-1"] {
        let out = git(&work, &["push", "origin", &format!("refs/tags/{name}")]);
        assert!(out.status.success(), "тег {name} — не имя версии: {}", String::from_utf8_lossy(&out.stderr));
    }
}

// Линза 02 §7 нашла ДВА расхождения между Rust-правилом и его шелльным близнецом
// в хуке. Оба здесь и закреплены: правило одно, реализации две, и разъезжаться
// им нельзя ни в какую сторону.
#[test]
fn правило_путей_совпадает_с_хуком_на_граничных_именах() {
    // F7: пустое имя. Шелльное `[^/]+` его отвергает, Rust — пропускал.
    assert!(!tree_path_allowed("steps/.md"), "пустое имя файла — не путь канона");
    assert!(!tree_path_allowed("steps/"), "каталог сам по себе не разрешён");
    assert!(!tree_path_allowed("steps/a/b.md"), "вложенность запрещена обеими реализациями");
    // Разрешённое остаётся разрешённым, включая не-ASCII (см. пробу пуша ниже).
    assert!(tree_path_allowed("steps/шаг.md"), "кириллица в имени законна");
    assert!(tree_path_allowed("steps/01-first.md"));
    assert!(
        tree_path_allowed("README.md")
            && tree_path_allowed("list.json")
            && tree_path_allowed(".gitattributes")
    );
}

// F6: хук судил по ЭКРАНИРОВАННОМУ пути — `ls-tree` заключает не-ASCII имена в
// кавычки, и якорное правило по ним не совпадало. Законный файл отвергался из-за
// формы вывода, а не из-за содержания. Проверяем настоящим пушем.
#[test]
fn имя_с_кириллицей_проходит_хук() {
    let root = tmp("tree-utf8");
    let (_bare, work) = repo_pair(&root.0);
    std::fs::create_dir_all(work.join("steps")).expect("mkdir steps");
    std::fs::write(work.join("steps").join("шаг.md"), "# шаг\n").expect("файл с кириллицей");
    git_ok(&work, &["add", "-A"]);
    git_ok(&work, &["commit", "-q", "-m", "шаг с кириллицей"]);

    let out = git(&work, &["push", "origin", "main"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "законное имя не должно отвергаться из-за кавычек в выводе ls-tree: {err}");
}

// Регрессия P2 авто-ревью: гитлинк libgit2 отдаёт как ObjectType::Commit, и
// «судим только блобы» пропускало подмодуль мимо правила целиком.
#[test]
fn update_main_отвергает_гитлинк() {
    let root = tmp("tree-gitlink");
    let bare = root.0.join("gl.git");
    let repo = git2::Repository::init_bare(&bare).expect("init bare");
    let sig = git2::Signature::new("Тест", "t@example.com", &git2::Time::new(1_700_000_000, 0)).expect("sig");

    let mut b = repo.treebuilder(None).expect("treebuilder");
    let blob = repo.blob(b"{}").expect("blob");
    b.insert("list.json", blob, 0o100644).expect("list.json");
    // Гитлинк: режим 160000, цель — произвольный sha (объекта у нас нет и не надо).
    let target = git2::Oid::from_str("0123456789abcdef0123456789abcdef01234567").expect("oid");
    b.insert("vendor", target, 0o160000).expect("gitlink");
    let tree = repo.find_tree(b.write().expect("write tree")).expect("tree");
    let tip = repo.commit(None, &sig, &sig, "с подмодулем", &tree, &[]).expect("commit");

    match update_main(&repo, tip, None, "тест") {
        Err(MainUpdateError::ForeignPath(p)) => assert_eq!(p, "vendor"),
        other => panic!("подмодуль обязан быть отвергнут, получено {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&bare);
}

#[test]
fn update_main_отвергает_посторонний_путь() {
    let root = tmp("tree-git2");
    let bare = root.0.join("g2.git");
    let repo = git2::Repository::init_bare(&bare).expect("init bare");

    let sig = git2::Signature::new("Тест", "t@example.com", &git2::Time::new(1_700_000_000, 0)).expect("sig");
    // parent обязателен для второго и далее коммитов: update_main пускает только
    // fast-forward, и родительский коммит здесь — не декорация, а условие проверки
    // именно состава дерева, а не родства.
    let mk = |paths: &[(&str, &str)], parent: Option<git2::Oid>| -> git2::Oid {
        let mut root_b = repo.treebuilder(None).expect("treebuilder");
        // Вложенные пути собираем поддеревом — так же, как их принёс бы пуш.
        let mut nested: Option<(String, git2::Oid)> = None;
        for (p, content) in paths {
            let blob = repo.blob(content.as_bytes()).expect("blob");
            match p.split_once('/') {
                None => root_b.insert(*p, blob, 0o100644).map(|_| ()).expect("insert"),
                Some((dir, name)) => {
                    let mut sub = repo.treebuilder(None).expect("sub");
                    sub.insert(name, blob, 0o100644).expect("insert sub");
                    nested = Some((dir.to_string(), sub.write().expect("write sub")));
                }
            }
        }
        if let Some((dir, oid)) = nested {
            root_b.insert(&dir, oid, 0o040000).expect("insert dir");
        }
        let tree = repo.find_tree(root_b.write().expect("write tree")).expect("tree");
        let parents: Vec<git2::Commit> =
            parent.into_iter().map(|p| repo.find_commit(p).expect("parent")).collect();
        let refs: Vec<&git2::Commit> = parents.iter().collect();
        repo.commit(None, &sig, &sig, "t", &tree, &refs).expect("commit")
    };

    let clean = mk(&[("list.json", "{}"), ("README.md", "# t")], None);
    assert_eq!(update_main(&repo, clean, None, "тест"), Ok(()), "чистое дерево проходит");

    let dirty = mk(&[("list.json", "{}"), ("assets/x.png", "\u{0}")], Some(clean));
    match update_main(&repo, dirty, Some(clean), "тест") {
        Err(MainUpdateError::ForeignPath(p)) => assert_eq!(p, "assets/x.png"),
        other => panic!("ожидался ForeignPath, получено {other:?}"),
    }

    // Легаси-исключение действует и здесь — правило одно на оба пути записи.
    let legacy = mk(&[("list.json", "{}"), ("steps/01-a.md", "# a")], Some(clean));
    assert_eq!(update_main(&repo, legacy, Some(clean), "тест"), Ok(()), "steps/ — легаси, проходит");

    let _ = std::fs::remove_dir_all(&bare);
}

#[test]
fn allowlist_путей_совпадает_с_форматом_версии() {
    for ok in ["README.md", "list.json", ".gitattributes", "steps/01-a.md"] {
        assert!(tree_path_allowed(ok), "{ok}");
    }
    for bad in [
        "steps", // ФАЙЛ с таким именем; каталог сюда не попадает — он не лист
        "assets/x.png",
        "steps/nested/deep.md", // подкаталог внутри steps — не легаси-форма
        "steps/x.bin",
        ".github/workflows/ci.yml",
        "readme.md", // регистр значим: дерево собирает ядро
    ] {
        assert!(!tree_path_allowed(bad), "{bad}");
    }
}

// Опора кода — родной механизм git: install_hook ставит receive.maxInputSize
// вместо самодельной проверки размера, потому что git отказывает ДО распаковки
// пака. Проверяем ПОВЕДЕНИЕМ, а не доверием к документации.
//
// Данные обязаны быть несжимаемыми: первая версия теста брала «шум» из
// 26-символьного цикла, он ужимался в пак меньше лимита, и тест сообщал, что
// механизм не работает, — хотя не работал сам тест. Здесь LCG даёт настоящую
// энтропию.
#[test]
fn receive_max_input_size_действительно_отказывает() {
    let root = tmp("pack-limit");
    let (bare, work) = repo_pair(&root.0);
    git_ok(&bare, &["config", "receive.maxInputSize", "512"]);
    let readback = git(&bare, &["config", "receive.maxInputSize"]);
    assert_eq!(String::from_utf8_lossy(&readback.stdout).trim(), "512", "конфиг записался");

    let mut seed: u64 = 0x2545_F491_4F6C_DD1D;
    let noise: String = (0..8192)
        .map(|_| {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            // Печатаемый ASCII из старших битов — их период куда длиннее младших.
            char::from(b'!' + ((seed >> 33) % 90) as u8)
        })
        .collect();
    std::fs::write(work.join("list.json"), format!("{{\"n\":\"{noise}\"}}")).expect("list.json");
    git_ok(&work, &["add", "-A"]);
    git_ok(&work, &["commit", "-q", "-m", "большой"]);

    let out = git(&work, &["push", "origin", "main"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "пак больше лимита обязан быть отвергнут: {err}");
    // Сообщение git при этом невнятное («unpacker error») — человеческий отказ
    // по размеру даёт фронт, проверяя Content-Length до чтения тела.
}

// install_hook обязан выставлять потолок пака сам: правило должно доезжать и до
// репозиториев, созданных раньше, — install_hook зовётся при каждом обращении.
#[test]
fn install_hook_выставляет_потолок_пака() {
    let root = tmp("pack-cfg");
    let bare = root.0.join("cfg.git");
    git_ok(&root.0, &["init", "-q", "--bare", bare.to_str().unwrap()]);
    install_hook(&bare).expect("хук");
    let v = git(&bare, &["config", "receive.maxInputSize"]);
    let got: u64 = String::from_utf8_lossy(&v.stdout).trim().parse().expect("число");
    assert_eq!(got, 16 * 1024 * 1024, "дефолт 16 МБ");
}

/// И2: язык отказа выбирается переменной окружения, которую ядро выставляет
/// процессу `receive-pack` — хук её наследует.
///
/// Проверяем ПОВЕДЕНИЕМ через настоящий push: сам факт, что оба набора лежат в
/// скрипте, ещё не значит, что выбор работает — ошибиться можно и в шаблоне
/// `case`, и в порядке ветвей.
#[test]
fn язык_отказа_переключается_окружением() {
    let root = tmp("tree-lang");
    let (_bare, work) = repo_pair(&root.0);
    std::fs::write(work.join("assets.bin"), b"x").expect("blob");
    git_ok(&work, &["add", "-A"]);
    git_ok(&work, &["commit", "-q", "-m", "мусор"]);

    // Английский по умолчанию: переменной нет вовсе (решение владельца 31.07 —
    // git-инструментарий англоязычен, незнакомый язык читается как поломка).
    let out = git(&work, &["push", "origin", "main"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("only README.md"), "по умолчанию английский: {err}");

    // Русский — по явному сигналу.
    let ru = Command::new("git")
        .env("SETFORK_ROLE", "owner")
        .current_dir(&work)
        .env("SETFORK_LANG", "ru")
        .args(["push", "origin", "main"])
        .output()
        .expect("git");
    let ru_err = String::from_utf8_lossy(&ru.stderr);
    assert!(ru_err.contains("разрешены только"), "русский по SETFORK_LANG: {ru_err}");

    // Незнакомый язык — английский, а не пусто: молчаливый отказ читается как сбой.
    let xx = Command::new("git")
        .env("SETFORK_ROLE", "owner")
        .current_dir(&work)
        .env("SETFORK_LANG", "xx")
        .args(["push", "origin", "main"])
        .output()
        .expect("git");
    let xx_err = String::from_utf8_lossy(&xx.stderr);
    assert!(xx_err.contains("only README.md"), "неизвестный язык → английский: {xx_err}");

    // Путь называется в любом языке — это данные, а не перевод.
    assert!(ru_err.contains("assets.bin") && err.contains("assets.bin"), "путь потерялся");
}

/// Ф4: `git push origin HEAD:refs/for/main` — предъявление правки, а не ветка.
///
/// Хук здесь решает ДО приёма (отказать задним числом нельзя), поэтому проверяем
/// его настоящим пушем: без ника — отказ с объяснением, чужая база — отказ,
/// нормальный случай — подсказка человеку в выводе.
#[test]
fn магический_реф_проверяется_хуком() {
    let root = tmp("magic-hook");
    let (_bare, work) = repo_pair(&root.0);

    // Без SETFORK_ACTOR: ядро не знает, в чью ветку класть коммиты.
    let anon = Command::new("git")
        .env("SETFORK_ROLE", "owner")
        .current_dir(&work)
        .args(["push", "origin", "main:refs/for/main"])
        .output()
        .expect("git");
    let anon_err = String::from_utf8_lossy(&anon.stderr);
    assert!(!anon.status.success(), "без пользователя магический реф не принимается: {anon_err}");
    assert!(anon_err.contains("authenticated push"), "отказ объясняет причину: {anon_err}");

    // Чужая база: поддерживается только main.
    let bad = Command::new("git")
        .env("SETFORK_ROLE", "owner")
        .current_dir(&work)
        .env("SETFORK_ACTOR", "mike")
        .args(["push", "origin", "main:refs/for/other"])
        .output()
        .expect("git");
    let bad_err = String::from_utf8_lossy(&bad.stderr);
    assert!(!bad.status.success(), "чужая база отвергается: {bad_err}");
    assert!(bad_err.contains("only refs/for/main"), "отказ называет правило: {bad_err}");

    // Нормальный случай: пуш проходит и человек видит, что дальше.
    let ok = Command::new("git")
        .env("SETFORK_ROLE", "owner")
        .current_dir(&work)
        .env("SETFORK_ACTOR", "mike")
        .args(["push", "origin", "main:refs/for/main"])
        .output()
        .expect("git");
    let ok_err = String::from_utf8_lossy(&ok.stderr);
    assert!(ok.status.success(), "нормальный магический пуш проходит: {ok_err}");
    assert!(ok_err.contains("change accepted"), "человек видит подсказку: {ok_err}");
}
