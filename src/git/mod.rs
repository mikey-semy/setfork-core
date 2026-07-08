// Git-подсистема: всё, что работает с git2/bare-репо, ниже уровня gRPC.
// bundle     — сериализация версий в git-дерево (зеркало serialize.ts) и материализация;
// project    — обратное чтение состояния списка из git-дерева (проекция в БД);
// repo       — персистентные bare-репо (GIT_DATA_DIR), пер-репо локи;
// smart_http — git smart-HTTP поверх материализованного репо (порт smart-http.ts).
pub mod bundle;
pub mod project;
pub mod repo;
pub mod smart_http;
