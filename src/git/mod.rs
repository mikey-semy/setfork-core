//! Git-подсистема: всё, что работает с git2/bare-репо, ниже уровня gRPC.
//! serialize  — версия списка → файлы git-дерева (зеркало serialize.ts);
//! bundle     — материализация репо из истории версий (типы serialize ре-экспортирует);
//! project    — обратное чтение состояния списка из git-дерева (проекция в БД);
//! history    — чтение истории коммитов (вкладка «Коммиты» у правки);
//! write      — запись list.json в ветку коммитом («предложенные правки»);
//! repo       — персистентные bare-репо (GIT_DATA_DIR), пер-репо локи;
//! smart_http — git smart-HTTP поверх материализованного репо (порт smart-http.ts).
/// Единственная ветка-канон: все проекции/merge/защита хука ходят по ней.
pub const MAIN_REF: &str = "refs/heads/main";

pub mod bundle;
pub mod history;
pub mod project;
pub mod repo;
pub mod serialize;
pub mod smart_http;
pub mod write;
