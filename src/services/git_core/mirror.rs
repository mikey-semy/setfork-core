//! Фоновый пуш зеркала: схлопывание, сериализация исхода, запись статуса.
//!
//! Отдельно от транспорта, потому что это единственная часть `GitCore`, которая
//! ходит в ЧУЖУЮ сеть, и правила у неё свои: исход пишется в статус настроек,
//! ошибка чужого форджа — не наша поломка, а повторный пуш гонит состояние целиком.

use sqlx::PgPool;
use uuid::Uuid;

use crate::db;

/// Пуш зеркала СЕЙЧАС (Ф3): читает настройки, расшифровывает токен, пушит,
/// записывает статус. «Не настроено» — не ошибка (Ok). Любой сбой — в статус
/// списка (mirror_error) и метрику: молчаливой деградации быть не должно.
///
/// Ф2: пуши одного списка ВЫСТРОЕНЫ В ОЧЕРЕДЬ — замок здесь, а не у вызывающих,
/// потому что путей два (фоновый через `spawn_mirror` и ручной RPC), и обойти
/// правило не должен ни один. Без этого фоновый пуш, висящий в сетевом таймауте,
/// мог записать свой старый провал ПОСЛЕ успешного ручного и объявить
/// синхронизированное зеркало сломанным — счётчик неудач при этом уезжал с 0 на 1,
/// и подметальщик повторял ненужный пуш (авто-ревью core#78).
pub(crate) async fn push_mirror_now(pool: &PgPool, id: Uuid, bare: &std::path::Path) -> Result<(), String> {
    crate::throttle::serialized(id, push_mirror_locked(pool, id, bare)).await
}

async fn push_mirror_locked(pool: &PgPool, id: Uuid, bare: &std::path::Path) -> Result<(), String> {
    // Настройки читаем ПОД замком: пока предыдущий пуш висел, владелец мог
    // поменять токен, и ждавшему в очереди нужен новый, а не тот, что был при
    // постановке.
    let Some((url, token_enc)) = db::load_mirror(pool, id).await.map_err(|e| e.to_string())? else {
        return Ok(()); // зеркало не настроено
    };
    let started = std::time::Instant::now();
    let outcome = match crate::git::mirror::mirror_secret() {
        None => {
            Err("SETFORK_MIRROR_SECRET не задан на сервере — зеркало не может расшифровать токен".to_string())
        }
        Some(secret) => match crate::git::mirror::decrypt_token(&token_enc, secret) {
            None => {
                Err("токен зеркала не расшифровался (секрет сменён?) — сохраните токен заново".to_string())
            }
            Some(token) => crate::git::mirror::mirror_push(bare, &url, &token).await,
        },
    };
    // Ф2: длительность рядом со счётчиком. Счётчик отвечает «сколько сломалось»,
    // гистограмма — «сколько это занимает»: пуш, доросший до таймаута 60с, до сих
    // пор выглядел ровно как мгновенный, пока не падал.
    let result = if outcome.is_ok() { "ok" } else { "error" };
    metrics::histogram!("mirror_push_duration_seconds", "result" => result)
        .record(started.elapsed().as_secs_f64());
    match &outcome {
        Ok(()) => {
            metrics::counter!("mirror_push_total", "result" => "ok").increment(1);
            tracing::info!(%id, url, "mirror updated");
        }
        Err(e) => {
            metrics::counter!("mirror_push_total", "result" => "error").increment(1);
            tracing::warn!(%id, url, error = %e, "mirror push failed");
        }
    }
    if let Err(e) = db::record_mirror_result(pool, id, outcome.as_ref().err().map(|s| s.as_str())).await {
        tracing::error!(%id, error = %e, "mirror status not recorded");
    }
    outcome
}

/// Фоновый пуш зеркала после записи в main (fire-and-forget: запись не ждёт
/// сети; исход виден в статусе настроек и метрике).
///
/// Ф2: вызовы для одного списка схлопываются окном `SETFORK_MIRROR_THROTTLE_SEC` —
/// серия версий даёт один пуш, а не пачку одновременных. Безопасно потому, что
/// пуш гонит текущее состояние ref'ов целиком (подробности — `throttle`).
/// Ручной пуш (RPC MirrorPush) идёт мимо: там человек ждёт ответа.
pub(crate) fn spawn_mirror(pool: PgPool, id: Uuid, bare: std::path::PathBuf) {
    crate::throttle::coalesce(id, crate::config::mirror_throttle(), move || {
        let (pool, bare) = (pool.clone(), bare.clone());
        async move {
            let _ = push_mirror_now(&pool, id, &bare).await;
        }
    });
}
