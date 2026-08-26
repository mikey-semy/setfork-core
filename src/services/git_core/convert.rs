//! Преобразования protobuf ↔ домен и сборка канонического `list.json`.
//!
//! Зона названа по ответственности, а не по слою: здесь живёт ровно то место, где
//! ошибка не падает, а ТИХО ТЕРЯЕТ поле — незнакомое значение превращается в
//! умолчание, пустая строка в отсутствие. Линзы 01 и 04 нашли тут три таких потери,
//! поэтому смысл держать их рядом и под одним заголовком.

use tonic::Status;
use uuid::Uuid;

use crate::db;
use crate::git::bundle::{SerStep, StepRef, VersionData};
use crate::git::{project, serialize};
use crate::pb::{BranchSnapshotResponse, ListContent, SnapshotRef, SnapshotStep};

// BranchSnapshotData -> pb-снапшот (переиспользуется snapshot-RPC и merge-state).
pub(super) fn to_snapshot_pb(sn: project::BranchSnapshotData) -> BranchSnapshotResponse {
    BranchSnapshotResponse {
        found: true,
        tip_sha: sn.tip,
        title: sn.title,
        desc: sn.desc,
        tags: sn.tags,
        ordered: sn.ordered,
        steps: steps_to_pb(&sn.steps),
    }
}

/// Разобранный канон -> провод (Ф4): то же содержимое, что клиент понесёт в
/// AddVersion. Собирается из ListParts, а не из текста повторно: разбор канона
/// живёт в одном месте, иначе редактор и проекция начнут понимать файл по-разному.
pub(super) fn list_content_from_parts(p: project::ListParts) -> ListContent {
    ListContent {
        title: p.title,
        desc: p.desc,
        tags: p.tags,
        ordered: p.ordered,
        version: p.version,
        steps: steps_to_pb(&p.steps),
    }
}

// Шаги домена -> шаги провода. Общее у снапшота ветки, merge-state и разбора
// канона: три места, где ProjStep уезжает клиенту.
pub(super) fn steps_to_pb(steps: &[project::ProjStep]) -> Vec<SnapshotStep> {
    steps
        .iter()
        .enumerate()
        .map(|(i, st)| SnapshotStep {
            n: (i as i32) + 1,
            title: st.title.clone(),
            desc: st.desc.clone(),
            command: st.command.clone(),
            level: st.level.clone(),
            why: st.why.clone(),
            section: st.section.clone(),
            subtasks: st.subtasks.clone(),
            refs: st
                .refs
                .iter()
                .map(|r| SnapshotRef { label: r.label.clone(), url: r.url.clone().unwrap_or_default() })
                .collect(),
            r#type: st.block_type.clone(),
            content_json: if st.block_type.is_empty() { String::new() } else { st.content.to_string() },
            // Идентичность блока — сквозь провод: без неё дифф ветки читает
            // переименование как «удалён + добавлен» (ADR-0013).
            block_id: st.block_id.clone().unwrap_or_default(),
            // Разрушительный пункт: снапшот ветки ЧИТАЕТ пометку из канона —
            // тот, кто смотрит чужую правку, обязан видеть её до слияния.
            // Обратно (запись в ветку) она едет не отсюда, см. canon_list_json.
            danger: st.danger.unwrap_or(false),
            // Ф4: канон эти поля несёт, и вернуть содержимое без них значит
            // стереть картинку и пометку при сохранении из редактора кода —
            // набор шагов перезаписывается целиком. Отсутствие в тексте = «нет»:
            // редактор видит полный снимок (proto/git.proto, SnapshotStep).
            image_key: st.image_key.clone().unwrap_or_default(),
            needs_human: st.needs_human.unwrap_or(false),
            needs_human_ask: st.needs_human_ask.clone().unwrap_or_default(),
        })
        .collect()
}

/// Провод → домен сериализации: структура версии от клиента становится тем же
/// `VersionData`, из которого материализуются коммиты версий. Один тип на оба
/// пути записи — поэтому канон не может разойтись между «пуш» и «правка в ветке».
pub(super) fn from_list_content(c: ListContent) -> VersionData {
    VersionData {
        version: c.version,
        note: String::new(), // сообщение коммита приходит отдельным полем запроса
        ts: 0,               // ветка коммитится «сейчас», дата берётся не отсюда
        title: c.title,
        desc: c.desc,
        tags: c.tags,
        ordered: c.ordered,
        // Провод kind не несёт: тип списка — свойство templates, канон получает
        // его из БД (canon_list_json), иначе каждая веточная запись стирала бы
        // kind из дерева (ловушка №1 разведки Ф2a).
        kind: None,
        steps: c
            .steps
            .into_iter()
            .map(|s| SerStep {
                n: s.n,
                // Провод не отличает '' от отсутствия: пустой type = шаг (blocks::is_step_type).
                block_type: if crate::blocks::is_step_type(&s.r#type) {
                    None
                } else {
                    Some(s.r#type.clone())
                },
                content: crate::blocks::content_value(&s.r#type, &s.content_json),
                block_id: Some(s.block_id).filter(|v| !v.trim().is_empty()),
                title: s.title,
                desc: s.desc,
                command: s.command,
                // Провод SnapshotStep этих полей не несёт: canon_list_json обогащает
                // их из текущей версии по block_id (иначе веточная запись стирала бы
                // картинку/пометку из канона — та же ловушка, что была с kind).
                image_key: None,
                level: s.level,
                needs_human: false,
                needs_human_ask: None,
                // Как и пометка «нужен человек»: значение с провода здесь НЕ берём,
                // его подставляет canon_list_json из текущей версии по block_id.
                // Клиент, который поля не заполнил, иначе снял бы пометку с
                // разрушительной команды одной записью в ветку.
                danger: false,
                why: s.why,
                section: s.section,
                subtasks: s.subtasks,
                refs: s
                    .refs
                    .into_iter()
                    .map(|r| StepRef { label: r.label, url: Some(r.url).filter(|u| !u.is_empty()) })
                    .collect(),
            })
            .collect(),
    }
}

/// Канонические байты list.json для записи в ветку — ВСЕГДА собираются здесь,
/// из присланной структуры. Прислать готовый файл больше нельзя: поле `list_json`
/// снято из контракта (`reserved`), потому что оно требовало от клиента знать
/// правила формата, а значит держать вторую его реализацию.
pub(super) fn canon_list_json(
    content: Option<ListContent>,
    kind: Option<String>,
    carry: &std::collections::HashMap<Uuid, db::CarryOver>,
) -> Result<Vec<u8>, Status> {
    let c = content.ok_or_else(|| Status::invalid_argument("content required"))?;
    let mut v = from_list_content(c);
    v.kind = kind.filter(|k| serialize::is_valid_kind(k));
    // Ф2a-довесок: провод не несёт картинку/пометку — обогащаем из текущей версии
    // по идентичности блока, иначе веточная запись стирала бы их из канона
    // (та же ловушка, что была с kind).
    for s in &mut v.steps {
        let Some(bid) = s.block_id.as_deref().and_then(|b| Uuid::parse_str(b).ok()) else { continue };
        let Some(co) = carry.get(&bid) else { continue };
        s.image_key = co.image_key.clone();
        s.needs_human = co.needs_human;
        s.needs_human_ask = Some(db::loc(&co.needs_human_ask)).filter(|a| !a.is_empty() && co.needs_human);
        s.danger = co.danger;
    }
    Ok(serialize::list_json(&v).into_bytes())
}

#[cfg(test)]
mod canon_tests {
    use super::{ListContent, canon_list_json, from_list_content};
    use crate::git::bundle::version_files;
    use crate::pb::{SnapshotRef, SnapshotStep};

    fn step(n: i32) -> SnapshotStep {
        SnapshotStep {
            n,
            title: "Install Redis".into(),
            desc: "Grab it".into(),
            command: "brew install redis".into(),
            level: "required".into(),
            why: "нужно для кэша".into(),
            section: "Setup".into(),
            subtasks: vec!["проверить версию".into()],
            refs: vec![SnapshotRef { label: "docs".into(), url: "https://redis.io".into() }],
            r#type: String::new(),
            content_json: String::new(),
            block_id: "11111111-2222-3333-4444-555555555555".into(),
            danger: false,
            // Довески канона: в веточной записи они приходят НЕ отсюда, а из БД
            // по block_id (canon_list_json) — здесь пусто намеренно.
            image_key: String::new(),
            needs_human: false,
            needs_human_ask: String::new(),
        }
    }

    fn content(steps: Vec<SnapshotStep>) -> ListContent {
        ListContent {
            title: "Redis Caching".into(),
            desc: "Описание".into(),
            tags: vec!["redis".into(), "кэш".into()],
            ordered: true,
            version: 7,
            steps,
        }
    }

    /// Канон из структуры == list.json из материализации той же версии.
    #[test]
    fn structured_content_matches_materialized_list_json() {
        let c = content(vec![step(1), SnapshotStep { n: 2, title: "Configure".into(), ..step(2) }]);
        let from_wire =
            canon_list_json(Some(c.clone()), None, &Default::default()).expect("канон из структуры");
        let materialized = version_files(&from_list_content(c))
            .into_iter()
            .find(|(p, _)| p == "list.json")
            .expect("list.json")
            .1;
        assert_eq!(String::from_utf8(from_wire).unwrap(), materialized);
    }

    /// Не-step блоки: type/content едут проводом и попадают в канон.
    #[test]
    fn non_step_block_carries_type_and_content() {
        let block = SnapshotStep {
            n: 1,
            r#type: "text".into(),
            content_json: r#"{"md":"Вступление"}"#.into(),
            block_id: String::new(),
            ..step(1)
        };
        let out = canon_list_json(Some(content(vec![block])), None, &Default::default()).expect("канон");
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("\"type\": \"text\""), "тип блока в каноне: {s}");
        assert!(s.contains("\"md\": \"Вступление\""), "payload блока в каноне: {s}");
        assert!(!s.contains("\"blockId\""), "пустая идентичность не пишется вовсе");
    }

    /// Пустой url ссылки — это ОТСУТСТВИЕ url (провод не различает '' и None).
    #[test]
    fn empty_ref_url_is_omitted() {
        let s = SnapshotStep {
            refs: vec![SnapshotRef { label: "без ссылки".into(), url: String::new() }],
            ..step(1)
        };
        let out = canon_list_json(Some(content(vec![s])), None, &Default::default()).expect("канон");
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("\"label\": \"без ссылки\""));
        assert!(!text.contains("\"url\""), "пустой url не должен попадать в канон: {text}");
    }

    /// Прислать готовый файл больше нельзя: без структуры запрос бессмыслен.
    #[test]
    fn без_содержимого_запрос_отклоняется() {
        let err = canon_list_json(None, None, &Default::default()).expect_err("канон не из чего собрать");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert_eq!(err.message(), "content required");
    }
}
