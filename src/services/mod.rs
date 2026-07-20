//! gRPC-сервисы, сгруппированные по домену (транспортный слой поверх db/git):
//! git_core — GitCore: smart-HTTP, ветки/теги/merge/bundle (proto/git.proto);
//! list     — ListRead + ListWrite: списки/версии/шаги (proto/domain_read.proto);
//! curation — CurationRead + CurationWrite: звёзды/watch;
//! collab   — CollabWrite: issues/suggestions/комментарии.
pub mod collab;
pub mod curation;
pub mod git_core;
pub mod golden;
pub mod list;
mod util;
