//! Материализация репо из истории версий (git2, шелл `git` для bundle/gc):
//! фиксированные автор/даты дают детерминированные SHA — golden-сверка
//! сравнивает их с inproc-реализацией фронта. Генерация файлов версии
//! (list.json/README/steps) — в соседнем serialize; типы ре-экспортируются
//! отсюда, чтобы исторические пути `bundle::VersionData` продолжали работать.
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use git2::{ObjectType, Oid, Repository, Signature, Time};

use super::MAIN_REF;
use super::serialize::commit_message;
pub use super::serialize::{SerStep, StepRef, VersionData, version_files};
use super::update::{AuthoredInput, MainUpdateError, update_main};

// Идентичность коммитов — ОДИНАКОВО с TS (store.ts/bundle.ts) для детерминированных SHA.
// Переиспользуются merge-коммитами в services::git_core.
pub const AUTHOR_NAME: &str = "SetFork";
pub const AUTHOR_EMAIL: &str = "git@setfork.com";

fn run_git(args: &[&str]) -> io::Result<()> {
    let out = Command::new("git").args(args).output()?;
    if !out.status.success() {
        return Err(io::Error::other(format!(
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(())
}

fn upd_io(e: MainUpdateError) -> io::Error {
    io::Error::other(e.to_string())
}

// Дерево версии из version_files (Ф2b: README.md, list.json, .gitattributes —
// плоский корень, steps/ из формата удалены). treebuilder.write() канонично
// сортирует записи — как git, поэтому SHA дерева совпадает.
//
// ⚠️ АВТОРСКИЕ КАТАЛОГИ (`AUTHORED_DIRS`: `scripts/`, `references/`, `assets/`) ПЕРЕНОСЯТСЯ ИЗ РОДИТЕЛЯ. Их не
// генерирует ядро — они приходят пушем, — а дерево здесь собирается с нуля. Без
// переноса первая же правка с сайта молча стирала бы скрипты, пришедшие пушем:
// версия выглядела бы законной, а файлов в ней уже не было. Переносим каталог
// целиком (тот же объект дерева), поэтому его байты и SHA не меняются.
//
// У списка без авторских файлов дерево прежнее до байта — SHA версий, собранных
// раньше, и сверка с фронтом не сдвигаются.
/// Откуда в новом коммите авторские каталоги.
///
/// `Carry` — перенос из родителя: так работает любая запись, которая про файлы не знает
/// (правка шагов на сайте, досыпка, выравнивание). `Replace` — набор пришёл вместе с
/// записью (ADR-0028, `publish_skill`): каталоги собираются из него, а родительские
/// отбрасываются целиком — пустой набор убирает все файлы.
#[derive(Clone, Copy)]
pub enum Authored<'a> {
    Carry,
    Replace(&'a [AuthoredInput]),
}

fn build_tree(
    repo: &Repository,
    v: &VersionData,
    parent_tree: Option<&git2::Tree<'_>>,
    authored: Authored<'_>,
) -> Result<Oid, git2::Error> {
    let mut root = repo.treebuilder(None)?;
    for (path, content) in version_files(v) {
        let blob = repo.blob(content.as_bytes())?;
        root.insert(path.as_str(), blob, 0o100644)?;
    }
    if let Authored::Replace(files) = authored {
        for dir in super::serialize::AUTHORED_DIRS {
            let prefix = format!("{dir}/");
            let mut sub = repo.treebuilder(None)?;
            let mut any = false;
            for f in files.iter().filter(|f| f.path.starts_with(&prefix)) {
                let blob = repo.blob(&f.content)?;
                let mode = if f.executable { 0o100755 } else { 0o100644 };
                sub.insert(&f.path[prefix.len()..], blob, mode)?;
                any = true;
            }
            // Пустого каталога в git не бывает: без файлов его нет и в дереве.
            if any {
                root.insert(dir, sub.write()?, 0o040000)?;
            }
        }
        return root.write();
    }
    if let Some(parent) = parent_tree {
        for dir in super::serialize::AUTHORED_DIRS {
            if let Some(entry) = parent.get_name(dir)
                && entry.kind() == Some(ObjectType::Tree)
            {
                root.insert(dir, entry.id(), entry.filemode())?;
            }
        }
    }
    root.write()
}

/// Дерево родителя коммита (или None у корневого) — источник авторских каталогов.
fn parent_tree(repo: &Repository, parent: Option<Oid>) -> Result<Option<git2::Tree<'_>>, git2::Error> {
    parent.map(|oid| repo.find_commit(oid).and_then(|c| c.tree())).transpose()
}

// Один коммит версии с фиксированной идентичностью/датой (SHA-идентично `git commit`).
fn commit_version(
    repo: &Repository,
    parent: Option<Oid>,
    v: &VersionData,
    authored: Authored<'_>,
) -> Result<Oid, git2::Error> {
    let tree = repo.find_tree(build_tree(repo, v, parent_tree(repo, parent)?.as_ref(), authored)?)?;
    let sig = Signature::new(AUTHOR_NAME, AUTHOR_EMAIL, &Time::new(v.ts, 0))?; // offset 0 → +0000
    let msg = commit_message(v);
    let parents: Vec<git2::Commit> = match parent {
        Some(oid) => vec![repo.find_commit(oid)?],
        None => vec![],
    };
    let refs: Vec<&git2::Commit> = parents.iter().collect();
    repo.commit(None, &sig, &sig, &msg, &tree, &refs) // update_ref=None: main выставим в конце
}

// Строит историю версий: коммиты, refs/heads/main ЧЕРЕЗ update_main (та же
// валидация, что у pre-receive), затем теги vN + HEAD→main. Возвращает tip.
//
// Порядок намеренный: main двигается ДО тегов. Если бы теги ставились первыми,
// отказ update_main (например, CAS при гонке) оставил бы теги vN на осиротевших
// коммитах — и история версий начала бы врать.
fn build_history(
    repo: &Repository,
    versions: &[VersionData],
    mut parent: Option<Oid>,
    authored: Authored<'_>,
) -> Result<Option<Oid>, MainUpdateError> {
    let expected_old = parent;
    let mut tagged: Vec<(i32, Oid)> = Vec::with_capacity(versions.len());
    for v in versions {
        let oid = commit_version(repo, parent, v, authored)?;
        tagged.push((v.version, oid));
        parent = Some(oid);
    }
    if let Some(tip) = parent
        && parent != expected_old
    {
        update_main(repo, tip, expected_old, "setfork: versions")?;
        for (ver, oid) in tagged {
            let obj = repo.find_object(oid, Some(ObjectType::Commit))?;
            repo.tag_lightweight(&format!("v{ver}"), &obj, true)?;
        }
        let _ = repo.set_head(MAIN_REF);
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
    let build = (|| -> Result<(), MainUpdateError> {
        let repo = Repository::init_bare(&work)?;
        build_history(&repo, versions, None, Authored::Carry)?;
        Ok(())
    })();
    match build {
        Ok(()) => Ok(work),
        Err(e) => {
            let _ = fs::remove_dir_all(&work);
            Err(upd_io(e))
        }
    }
}

// pre-receive hook (порт store.ts PRE_RECEIVE): main защищён от удаления и
// non-fast-forward (канон версий; черновики force-push'абельны), каждый
// пушнутый коммит обязан нести list.json в корне, и состав дерева ограничен
// allowlist'ом (Ф0 трека git-surface; список путей — serialize::tree_path_allowed,
// правило одно на два пути записи).
//
// Состав проверяется у КАЖДОГО нового коммита, а не только у вершины: чистый tip
// пропускал бы мусор из промежуточных коммитов, а тот остаётся достижимым из
// истории и уезжает на зеркало.
//
// ⚠️ Перечисляем ПУТИ (`ls-tree -r --name-only` по коммиту), а НЕ объекты.
// Первая версия брала одну команду `rev-list --objects … --not --all`, и это была
// дыра (авто-ревью core#70, P1): `--objects` печатает каждый OID ОДИН раз, и если
// лишний файл содержит те же байты, что разрешённый, его имя не печатается вовсе.
// Проверено: два одинаковых файла `README.md` и `evil` дают в выводе только
// `README.md`, то есть `evil` проезжал незамеченным. `ls-tree -r` перечисляет все
// имена и заодно листает только листья — каталоги в выводе не появляются, а
// подмодули (gitlink) появляются и потому тоже судятся.
//
// Отказ ГОВОРЯЩИЙ и называет сами пути: линза 01 (ledger 28.07, «Угол 1»)
// показала, что лишний файл сегодня принимается и игнорируется без единого
// слова — человек узнаёт о потере, только если сам заметит. stderr хука
// доезжает до клиента строками `remote: …`.
//
// `</dev/null` у git-вызовов: stdin хука — это список рефов, который читает
// `while read`, и дочерний процесс не должен его подъедать.
/// Тело хука ПОСТРОЧНО.
///
/// Раньше это была одна строка с экранированными переводами строк, и в неё же
/// попадал регэксп с обратными слешами: правка превращалась в подсчёт уровней
/// экранирования, а ошибка вылезала уже в шелле.
/// Массив строк читается как обычный shell и не требует ничего экранировать
/// сверх самих кавычек.
///
/// Тексты сюда не пишутся: только `msg <ключ>` — оба языка живут в `messages`.
const PRE_RECEIVE_BODY: &[&str] = &[
    "actor=\"${SETFORK_ACTOR:-}\"",
    // Ф5: роль пушащего. Решает приложение (ADR-0011 §2 — пользовательская
    // авторизация не в ядре), сюда она доезжает готовой.
    "role=\"${SETFORK_ROLE:-}\"",
    // H15-002: кого и о чём спрашивать про СОДЕРЖИМОЕ. Все три переменные
    // выставляет `smart_http::apply_push_env` тем же способом, что ник и роль.
    // Пустая любая из них = «спросить не у кого» → проверка пропускается, и пуш
    // идёт как шёл. Это не дыра, а совместимость: подложить сюда своё значение
    // может только тот, кто и так запускает receive-pack.
    "core=\"${SETFORK_CORE_BIN:-}\"",
    "owner=\"${SETFORK_OWNER:-}\"",
    "slug=\"${SETFORK_SLUG:-}\"",
    // Хеши уже проверенных авторских блобов — на весь пуш (см. проверку бинарности).
    "seen_blobs=$(mktemp)",
    "trap 'rm -f \"$seen_blobs\"' EXIT",
    "while read old new ref; do",
    // ЧТО МОЖЕТ ПОСТОРОННИЙ (Ф5): предъявить правку — и ничего больше.
    //
    // Никаких именованных веток: `refs/for/main` и всё. Имя ветки, куда лягут
    // коммиты, придумывает сервер (`u/<id>/<base>`), и человек его не набирает.
    // Так же устроен Gerrit: вклад приходит одним магическим рефом, а
    // `refs/changes/NN/CCCC/PP` называет сервер. Прежний вариант — «своё
    // пространство `u/<ник>/*`» — отвергнут вместе с ником в логике: ник
    // сменяем, и пространство, названное им, однажды достаётся другому человеку.
    // Список тех, кому МОЖНО всё, — перечислен явно; всё остальное непустое
    // ограничивается. Обратное («ограничиваем ровно contributor») открывало дверь
    // любому незнакомому значению: опечатка, новая роль во фронте, мусор в поле —
    // и посторонний пишет в main. Поле приходит строкой и не валидируется
    // протоколом, поэтому судить о нём надо по белому списку (авто-ревью core#80).
    //
    // Пустая роль — отдельный, ЗНАКОМЫЙ случай: фронт старше Ф5. Он ролей не
    // шлёт и посторонних не впускает, поэтому ограничивать нечего; ядро пишет об
    // этом в лог, чтобы окно выкатки не было тихим.
    "  case \"$role\" in",
    "    owner|collaborator|\"\") restricted=no ;;",
    "    *) restricted=yes ;;",
    "  esac",
    "  if [ \"$restricted\" = yes ]; then",
    "    if [ -z \"$actor\" ]; then",
    "      msg contributor_needs_actor >&2",
    "      exit 1",
    "    fi",
    "    case \"$ref\" in",
    "      refs/for/main) ;;",
    "      *)",
    "        msg contributor_namespace \"$ref\" >&2",
    "        msg contributor_namespace_hint >&2",
    "        exit 1",
    "        ;;",
    "    esac",
    "  fi",
    // Ф4, магический реф: `refs/for/<base>` значит «это не ветка, это
    // предложение к base». Отказы тут, а не после приёма: отвергнуть пуш
    // задним числом уже нельзя, а молча проглотить — тем более.
    "  case \"$ref\" in",
    "    refs/for/*)",
    "      if [ -z \"$actor\" ]; then",
    "        msg magic_needs_actor >&2",
    "        exit 1",
    "      fi",
    "      if [ \"$ref\" != \"refs/for/main\" ]; then",
    "        msg magic_bad_base \"$ref\" >&2",
    "        exit 1",
    "      fi",
    "      ;;",
    "  esac",
    // Имена версий принадлежат СЕРВЕРУ, и до этой проверки правило жило только на
    // веб-двери (`is_version_tag` в services/git_core). Владелец мог запушить
    // `refs/tags/v99` руками, после чего старший тег становился 99 при текущей
    // версии 1, выравнивание объявляло Conflict — и запись в список останавливалась
    // до вмешательства оператора. Правило слово в слово то же: `v` + только цифры;
    // человеческие теги (`v1.0`, `release-1`) остаются доступными.
    "  case \"$ref\" in",
    "    refs/tags/v*)",
    "      ver=${ref#refs/tags/v}",
    "      case \"$ver\" in",
    "        ''|*[!0-9]*) ;;",
    "        *) msg reserved_tag_name \"$ref\" >&2; exit 1 ;;",
    "      esac",
    "      ;;",
    "  esac",
    "  if [ \"$ref\" = \"refs/heads/main\" ]; then",
    "    if [ \"$new\" = \"$zero\" ]; then",
    "      msg main_no_delete >&2",
    "      exit 1",
    "    fi",
    "    if [ \"$old\" != \"$zero\" ] && ! git merge-base --is-ancestor \"$old\" \"$new\"; then",
    "      msg main_no_force >&2",
    "      exit 1",
    "    fi",
    "  fi",
    "  case \"$new\" in *$zero) continue ;; esac",
    "  if ! git cat-file -e \"$new:list.json\" 2>/dev/null; then",
    "    msg list_json_required >&2",
    "    exit 1",
    "  fi",
    // Перечисляем ПУТИ по каждому новому коммиту, а не объекты: `rev-list --objects`
    // печатает каждый OID один раз, и лишний файл с байтами разрешённого не
    // появлялся в выводе вовсе (авто-ревью core#70, P1).
    "  for c in $(git rev-list \"$new\" --not --all </dev/null); do",
    // core.quotePath=false — ИНАЧЕ не-ASCII имена печатаются в кавычках и с
    // \\xNN-экранированием, якорное правило по ним не совпадает, и законный
    // `steps/шаг.md` отвергается из-за ФОРМЫ ВЫВОДА, а не из-за содержания
    // (F6 линзы 02: в отказе было видно `"steps/шаг.md"` — с кавычками).
    // `<каталог>/<файл>` для авторских каталогов скилла (близнец
    // `serialize::authored_path`: один уровень, непустое имя). Список каталогов
    // подставляется в шапку хука из `AUTHORED_DIRS` — копии здесь нет.
    r#"    bad=$(git -c core.quotePath=false ls-tree -r --name-only "$c" </dev/null | grep -v -E "^(README\.md|list\.json|\.gitattributes|steps/[^/]+\.md|($authored_re)/[^/]+)\$" | sort -u | head -5)"#,
    "    if [ -n \"$bad\" ]; then",
    "      msg tree_allowlist >&2",
    "      msg tree_foreign_header \"$c\" >&2",
    r#"      echo "$bad" | sed 's/^/  /' >&2"#,
    "      msg tree_foreign_hint >&2",
    "      exit 1",
    "    fi",
    // АВТОРСКИЕ ФАЙЛЫ: путь уже проверен выше, здесь — то, чего по пути не видно.
    // Близнец `update::authored_violation`; правила обязаны совпадать.
    //
    // `ls-tree -l` печатает `<режим> <тип> <объект> <размер>\t<путь>`: режим отличает
    // ссылку (120000) и подмодуль (160000) от файла, размер нужен сумме.
    r#"    authored=$(git -c core.quotePath=false ls-tree -r -l "$c" -- $authored_dirs </dev/null)"#,
    "    if [ -n \"$authored\" ]; then",
    r#"      notfile=$(printf '%s\n' "$authored" | awk -F '\t' '{ split($1, f, " "); if (f[1] != "100644" && f[1] != "100755") print $2 }' | head -1)"#,
    "      if [ -n \"$notfile\" ]; then msg authored_not_file \"$notfile\" >&2; exit 1; fi",
    // Бинарь — есть байт NUL: без него размер после `tr -d '\000'` совпадает с
    // исходным. `</dev/null` у git обязателен: stdin внутреннего цикла — это
    // перечень файлов, и cat-file не должен его подъедать.
    // ⚠️ Каждый блоб — ОДИН РАЗ за пуш, по хешу. Замер линзы 06 «ресурсы»: 200 коммитов
    // при 50 файлах давали 58 секунд хука (линейно, ~0,3 с на коммит), потому что каждый
    // коммит заново читал ВСЕ файлы, хотя меняется обычно один. Импорт скилла вместе с
    // историей — ровно сотни коммитов. Уже проверенные хеши лежат в `$seen_blobs`.
    // Читаются в BEGIN, а не приёмом `NR == FNR`: тот ломается на ПУСТОМ первом файле —
    // ровно на первом коммите пуша, — и молча считал «виденными» все блобы.
    r#"      fresh=$(printf '%s\n' "$authored" | awk -F '\t' -v seen_file="$seen_blobs" 'BEGIN { while ((getline h < seen_file) > 0) seen[h] = 1 } { split($1, f, " "); if (!(f[3] in seen)) print }')"#,
    r#"      binary=$(printf '%s\n' "$fresh" | while IFS="$(printf '\t')" read -r meta path; do [ -n "$meta" ] || continue; set -- $meta; n=$(git cat-file blob "$3" </dev/null | tr -d '\000' | wc -c | tr -d ' '); [ "$n" = "$4" ] || { printf '%s\n' "$path"; break; }; printf '%s\n' "$3" >> "$seen_blobs"; done)"#,
    "      if [ -n \"$binary\" ]; then msg authored_binary \"$binary\" >&2; exit 1; fi",
    r#"      totals=$(printf '%s\n' "$authored" | awk -F '\t' '{ split($1, f, " "); n++; s += f[4] } END { print n + 0, s + 0 }')"#,
    "      files=${totals% *}; bytes=${totals#* }",
    "      if [ \"$files\" -gt \"$authored_max_files\" ]; then msg authored_too_many \"$files\" \"$authored_max_files\" >&2; exit 1; fi",
    "      if [ \"$bytes\" -gt \"$authored_max_bytes\" ]; then msg authored_too_large \"$bytes\" \"$authored_max_bytes\" >&2; exit 1; fi",
    "    fi",
    "  done",
    // H15-002: СОДЕРЖИМОЕ. До этой строки хук судил только форму дерева, а
    // `command` шага оставался непрозрачным JSON — список с `rm -rf /` в шаге
    // приезжал пушем и жил в каноне, хотя из формы продукт его бы не принял.
    //
    // Решает по-прежнему приложение (набор правил — политика безопасности на TS,
    // копий в Rust не заводим), принуждает ядро. Но HTTP из шелла не сделать, а
    // curl в runtime-образе нет — поэтому спрашивает подкоманда ЭТОГО ЖЕ бинаря,
    // а канон едет ей на stdin: объекты пуша лежат в карантине receive-pack, и
    // читать их умеет шелловый `git`, а не libgit2.
    //
    // ⚠️ ДВА рефа, а не все. `refs/heads/main` — это канон, `refs/for/main` —
    // предложение, и приложение проверяет оба своим фасадом. Черновые ветки и
    // теги не судим НАМЕРЕННО: на них законно уезжает уже существующая история,
    // и отказ по содержимому, которое давно лежит в репозитории, был бы ложным —
    // человек потерял бы работу, не поняв за что.
    //
    // ⚠️ Вершина, а не каждый коммит (в отличие от allowlist'а путей): в канон
    // проецируется только tip, команда из промежуточного коммита исполниться не
    // может. Судить её значило бы отказывать за то, что человек уже исправил.
    "  case \"$ref\" in",
    "    refs/heads/main|refs/for/main)",
    "      if [ -n \"$core\" ] && [ -n \"$owner\" ] && [ -n \"$slug\" ]; then",
    r#"        git cat-file -p "$new:list.json" </dev/null | "$core" check-content "$owner" "$slug" "$new""#,
    "        rc=$?",
    "        case \"$rc\" in",
    "          0) ;;",
    // 126/127 — бинарь не нашёлся или не исполняется. Это НАША поломка, а не
    // приговор пушу: отказать тут значило бы остановить работу всем и сразу
    // из-за неверного пути. Говорим вслух и пропускаем — ровно как при пустой
    // роли в Ф5: окно несовместимости не должно быть тихим, но и рабочий путь
    // записи оно ронять не должно.
    "          126|127) msg content_check_missing >&2 ;;",
    "          *) exit 1 ;;",
    "        esac",
    "      fi",
    "      ;;",
    "  esac",
    "  case \"$ref\" in refs/for/*) msg magic_accepted >&2 ;; esac",
    "done",
    "exit 0",
];

/// Полный текст хука: шапка + сгенерированная `msg()` + тело.
fn pre_receive() -> String {
    // Лимиты авторских файлов — из тех же констант, что у `update_main`: одно
    // число на две двери, а не две копии, которые однажды разъедутся.
    format!(
        "#!/bin/sh
zero=0000000000000000000000000000000000000000
authored_max_files={}
authored_max_bytes={}
authored_dirs='{}'
authored_re='{}'
{}{}
",
        super::serialize::AUTHORED_MAX_FILES,
        super::serialize::AUTHORED_MAX_BYTES,
        super::serialize::AUTHORED_DIRS.join(" "),
        super::serialize::AUTHORED_DIRS.join("|"),
        super::messages::shell_msg_fn(),
        PRE_RECEIVE_BODY.join(
            "
"
        )
    )
}

/// Ставит pre-receive hook (защита main + list.json + состав дерева) и потолок
/// входящего пака; идемпотентно — обновления правил докатываются до старых репо.
pub fn install_hook(bare: &Path) -> io::Result<()> {
    let hooks = bare.join("hooks");
    fs::create_dir_all(&hooks)?;
    fs::write(hooks.join("pre-receive"), pre_receive())?;
    // executable bit — только на unix; на Windows git-for-windows берёт хук через sh.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(hooks.join("pre-receive"), fs::Permissions::from_mode(0o755));
    }
    set_max_input_size(bare);
    Ok(())
}

/// Потолок входящего пака (`receive.maxInputSize`, SETFORK_MAX_PACK_MB, дефолт 16).
///
/// РОДНОЙ механизм git, а не самодельная проверка: receive-pack сверяет размер
/// потока и отказывает ДО распаковки, то есть мусорный пак не успевает стать
/// объектами на диске. Своя проверка после приёма такого свойства не даёт.
/// 0 = без ограничения (семантика самого git).
///
/// Это ДРУГАЯ граница, чем потолок gRPC-сообщения: та про транспорт ядро↔фронт,
/// эта — про продукт («какой пуш мы вообще готовы принять»).
fn set_max_input_size(bare: &Path) {
    let mb = std::env::var("SETFORK_MAX_PACK_MB").ok().and_then(|v| v.trim().parse::<u64>().ok());
    let bytes = mb.unwrap_or(16) * 1024 * 1024;
    let out = std::process::Command::new("git")
        .args(["--git-dir", &bare.to_string_lossy(), "config", "receive.maxInputSize", &bytes.to_string()])
        .output();
    // Обслуживание, не работа: не записалось — залогируем, но репо остаётся рабочим.
    match out {
        Ok(o) if !o.status.success() => {
            tracing::warn!(repo = %bare.display(), err = %String::from_utf8_lossy(&o.stderr),
                "receive.maxInputSize not set");
        }
        Err(e) => tracing::warn!(repo = %bare.display(), error = %e, "receive.maxInputSize not set"),
        _ => {}
    }
}

/// Размер bare-репо на диске, байты (рекурсивный обход). Нужен метрике и порогу
/// `SETFORK_REPO_LIMIT_MB`: per-push потолок не мешает вырастить репо серией
/// мелких пушей, а квоты размера у git нет вовсе — это уровень приложения
/// Так же устроено у GitLab: «Repository size limit» — настройка приложения
/// (инстанс/группа/проект), и при превышении пуш ОТКЛОНЯЕТСЯ
/// (docs.gitlab.com/administration/settings/account_and_limit_settings).
pub fn repo_size_bytes(bare: &Path) -> u64 {
    fn walk(dir: &Path, acc: &mut u64) {
        let Ok(entries) = fs::read_dir(dir) else { return };
        for e in entries.flatten() {
            match e.file_type() {
                Ok(t) if t.is_dir() => walk(&e.path(), acc),
                Ok(t) if t.is_file() => *acc += e.metadata().map(|m| m.len()).unwrap_or(0),
                _ => {}
            }
        }
    }
    let mut total = 0;
    walk(bare, &mut total);
    total
}

/// `git gc --auto` на bare: упаковывает loose-объекты при превышении порога
/// gc.auto (иначе почти no-op). git2-запись версий/мержей не триггерит авто-gc
/// (в отличие от receive-pack/worktree-commit), поэтому зовём вручную после
/// материализации/дозаписи. Ошибки глушим — это обслуживание, не критично.
pub fn gc_auto(bare: &Path) {
    let _ = std::process::Command::new("git")
        .args(["--git-dir", &bare.to_string_lossy(), "gc", "--auto", "--quiet"])
        .status();
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
    (|| -> Result<(), MainUpdateError> {
        let repo = Repository::init_bare(bare)?;
        build_history(&repo, versions, None, Authored::Carry)?;
        Ok(())
    })()
    .map_err(upd_io)?;
    install_hook(bare)?;
    gc_auto(bare); // упаковать объекты стартовой истории
    Ok(())
}

/// Дописывает версии поверх текущего main (git2), сохраняя запушенные коммиты.
/// `versions` — только те, что добавить (version > have). Без worktree.
/// Возвращает hex-sha нового tip main (None — дописывать было нечего).
///
/// Это НЕ «ленивая досыпка» (её больше нет): функцию зовут единый путь записи
/// версии (git::version::commit_web_version) и одноразовый догон sync-repos.
pub fn append_versions(bare: &Path, versions: &[VersionData]) -> io::Result<Option<String>> {
    if versions.is_empty() {
        return Ok(None);
    }
    let tip = (|| -> Result<Option<Oid>, MainUpdateError> {
        let repo = Repository::open_bare(bare)?;
        let parent = repo.refname_to_id(MAIN_REF).ok();
        build_history(&repo, versions, parent, Authored::Carry)
    })()
    .map_err(upd_io)?;
    gc_auto(bare); // loose-объекты дозаписанных версий → упаковка при пороге
    Ok(tip.map(|o| o.to_string()))
}

/// Одна версия поверх main с НАБОРОМ файлов автора (ADR-0028): блоки и файлы одним
/// коммитом. Отказ по набору или по дереву возвращается ТИПОМ, а не текстом: вызывающему
/// надо отличить «файлы не по правилу» (ошибка ввода) от сбоя git.
pub fn append_version_with(
    bare: &Path,
    v: &VersionData,
    authored: Authored<'_>,
) -> Result<Option<String>, MainUpdateError> {
    let repo = Repository::open_bare(bare)?;
    let parent = repo.refname_to_id(MAIN_REF).ok();
    let tip = build_history(&repo, std::slice::from_ref(v), parent, authored)?;
    gc_auto(bare);
    Ok(tip.map(|o| o.to_string()))
}

/// Рождение репозитория ОДНОЙ версией с набором файлов (Create git-first): тег v1 сразу
/// на вершине, файлы в дереве. Хук ставится так же, как у бутстрапа из БД.
pub fn init_with_version(
    bare: &Path,
    v: &VersionData,
    authored: Authored<'_>,
) -> Result<(), MainUpdateError> {
    if let Some(parent) = bare.parent() {
        fs::create_dir_all(parent).map_err(|e| MainUpdateError::Git(e.to_string()))?;
    }
    let repo = Repository::init_bare(bare)?;
    build_history(&repo, std::slice::from_ref(v), None, authored)?;
    install_hook(bare).map_err(|e| MainUpdateError::Git(e.to_string()))?;
    gc_auto(bare);
    Ok(())
}

/// Лежат ли ВСЕ версии `versions` уже коммитами в хвосте main (сверху вниз,
/// по деревьям)? Вернёт их коммиты по порядку версий, иначе None.
fn tail_commits(
    repo: &Repository,
    tip: Oid,
    versions: &[VersionData],
) -> Result<Option<Vec<Oid>>, git2::Error> {
    let mut oids = Vec::with_capacity(versions.len());
    let mut cur = Some(tip);
    for v in versions.iter().rev() {
        let Some(oid) = cur else { return Ok(None) };
        let commit = repo.find_commit(oid)?;
        // Авторские каталоги берём У САМОГО КОММИТА: их не генерируют, сравнивать надо
        // сгенерированную часть. Сверка с родительскими не узнавала версию, чей коммит
        // САМ добавил `scripts/` (пуш), — и выравнивание клало рядом пустого двойника.
        let expected = build_tree(repo, v, Some(&commit.tree()?), Authored::Carry)?;
        if commit.tree_id() != expected {
            return Ok(None);
        }
        oids.push(oid);
        cur = commit.parent_ids().next();
    }
    oids.reverse();
    Ok(Some(oids))
}

/// Досыпка версий при ВЫРАВНИВАНИИ (не обычная запись).
///
/// Отличие от `append_versions`: версия, которая уже лежит коммитом на main
/// (потерян тег, а не коммит), получает тег обратно на СВОЙ коммит, а не второй
/// коммит с тем же деревом. Замер линзы 02 §4: удаление тега v2 заставляло
/// досыпку положить пустой коммит-двойник и перевесить тег на него — канон
/// записывал событие, которого не было, а diff версии выходил пустым.
///
/// Обычная запись версии этой поблажки не получает намеренно: там совпадение
/// деревьев значит «сохранили без изменений», и подменять новый коммит тегом на
/// старом нельзя — версия в БД уже своя.
///
/// Смешанное состояние (новые версии лежат, старых нет) досыпкой не чинится —
/// вставить коммит в середину истории нельзя; такое уходит в прежний путь.
pub fn append_missing_versions(bare: &Path, versions: &[VersionData]) -> io::Result<Option<String>> {
    if versions.is_empty() {
        return Ok(None);
    }
    let tip = (|| -> Result<Option<Oid>, MainUpdateError> {
        let repo = Repository::open_bare(bare)?;
        let parent = repo.refname_to_id(MAIN_REF).ok();
        if let Some(tip) = parent
            && let Some(oids) = tail_commits(&repo, tip, versions)?
        {
            for (v, oid) in versions.iter().zip(oids) {
                let obj = repo.find_object(oid, Some(ObjectType::Commit))?;
                repo.tag_lightweight(&format!("v{}", v.version), &obj, true)?;
            }
            return Ok(Some(tip));
        }
        build_history(&repo, versions, parent, Authored::Carry)
    })()
    .map_err(upd_io)?;
    gc_auto(bare);
    Ok(tip.map(|o| o.to_string()))
}

/// Максимальный номер версии среди тегов v* (git2).
/// Номер версии из имени тега — ЕДИНСТВЕННЫЙ разбор в проекте.
///
/// Строго зеркалит правило резервирования (`git_core::names::is_version_tag`, которое теперь
/// через него и выражено): одиночное `v` + ТОЛЬКО десятичные цифры. Иначе разбор шире
/// резервирования, и в щель лезут имена, которые система считает законными релизами:
///   • `vv2` — `trim_start_matches` снимал все ведущие `v`;
///   • `v+2` — `parse::<i32>()` принимает ведущий плюс.
/// Оба разбирались как версия 2 и сталкивались с настоящим тегом `v2`.
///
/// Цена щели разная в разных местах, и худшая — здесь: релиз `v+20` на списке с восемью
/// версиями поднял бы максимум до 20, и новые версии переставали бы доезжать в git МОЛЧА
/// (ровно та беда, от которой заводилось резервирование). На витрине версий та же щель
/// давала бы SHA, меняющийся от запроса к запросу.
///
/// Копий разбора было три; сведены сюда после второго повтора одного корня.
pub fn version_of_tag(name: &str) -> Option<i32> {
    let rest = name.strip_prefix('v')?;
    if rest.is_empty() || !rest.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    rest.parse::<i32>().ok()
}

pub fn max_tag_version(bare: &Path) -> i32 {
    match Repository::open_bare(bare) {
        Ok(r) => max_tag_in(&r),
        Err(_) => 0,
    }
}

/// Есть ли `main` и какова максимальная версия по тегам — ОДНИМ открытием репо.
/// Выравнивание спрашивает и то и другое на КАЖДОМ чтении списка (его зовёт
/// `ensure_repo`), поэтому два отдельных чтения означали два открытия репо и два
/// прыжка в блокирующий пул на ровном месте.
/// `None` — репозиторий НЕ ОТКРЫЛСЯ. Это отдельный ответ, а не `(false, 0)`:
/// «каталог есть, но не читается» (обрубленный HEAD после сбоя, частичное
/// восстановление тома, права/дескрипторы) неотличимо от «ветки и тегов нет», и
/// лечение приняло бы битое репо за пустое — то есть переписало бы историю из БД
/// вместо того, чтобы громко отказать.
pub fn refs_state(bare: &Path) -> Option<(bool, i32)> {
    let repo = Repository::open_bare(bare).ok()?;
    Some((repo.refname_to_id(MAIN_REF).is_ok(), max_tag_in(&repo)))
}

fn max_tag_in(repo: &Repository) -> i32 {
    let names = match repo.tag_names(Some("v*")) {
        Ok(n) => n,
        Err(_) => return 0,
    };
    let mut max = 0i32;
    // git2 0.21: iter() отдаёт Result (не-UTF8 имена больше не глотаются молча).
    for name in names.iter().flatten().flatten() {
        if let Some(n) = version_of_tag(name) {
            max = max.max(n);
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

#[cfg(test)]
mod version_tag_tests {
    use super::version_of_tag;

    /// Разбор обязан быть УЖЕ или РАВЕН правилу резервирования, никогда шире.
    ///
    /// Проверка здесь, а не только в интеграционном тесте, потому что там исход зависит от
    /// порядка обхода рефов: столкнувшись, настоящий тег и подложный выигрывают через раз, и
    /// зелёный прогон ничего не доказывает. Мутация это и показала — снятие проверки «только
    /// цифры» интеграционный тест НЕ уронило. Здесь исход не зависит ни от чего.
    #[test]
    fn only_v_plus_digits_is_a_version() {
        assert_eq!(version_of_tag("v1"), Some(1));
        assert_eq!(version_of_tag("v42"), Some(42));
        // Зарезервировано правилом, разбирается — согласовано.
        assert_eq!(version_of_tag("v02"), Some(2));

        // Имена, которые система считает ЗАКОННЫМИ релизами: разбор обязан их отвергнуть,
        // иначе они столкнутся с настоящим тегом версии.
        assert_eq!(version_of_tag("vv2"), None, "лишнее ведущее 'v' не версия");
        assert_eq!(version_of_tag("v+2"), None, "ведущий плюс не версия (parse его принимает)");
        assert_eq!(version_of_tag("v-5"), None, "минус не версия");
        assert_eq!(version_of_tag("v2.1"), None, "составное имя не версия");
        assert_eq!(version_of_tag("v2-beta"), None, "суффикс не версия");
        assert_eq!(version_of_tag("v"), None, "пустой номер не версия");
        assert_eq!(version_of_tag("release-2"), None, "чужой префикс не версия");
        assert_eq!(version_of_tag("v 2"), None, "пробел не версия");
    }
}
