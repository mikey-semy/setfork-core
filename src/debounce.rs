//! Ф2: схлопывание фоновых задач по ключу («последний за окно побеждает»).
//!
//! Зачем. Пуш зеркала висит на каждой записи в `main`. Пачка версий подряд —
//! импорт, серия правок, слияние нескольких предложений — давала пачку
//! одновременных `git push` в один и тот же remote: форджа отвечает медленнее,
//! часть попыток отваливается по таймауту, а результат у всех один и тот же.
//!
//! Схлопывать здесь БЕЗОПАСНО ровно потому, что пуш зеркала не инкрементальный:
//! он всегда гонит ТЕКУЩЕЕ состояние ref'ов (`+refs/heads/main`, `+refs/tags/*`).
//! Пропущенный промежуточный пуш ничего не теряет — следующий догоняет истину
//! целиком. Для инкрементальных задач этот модуль не годится.
//!
//! Форма — trailing-edge debounce с продолжением: событие, пришедшее ВО ВРЕМЯ
//! прогона, не проглатывается, а даёт ещё один прогон. Иначе версия,
//! закоммиченная между «git push прочитал ref» и «git push завершился», уехала бы
//! на зеркало только со следующей записью — то есть, возможно, никогда.
//!
//! ⚠️ Окно живёт в памяти процесса: рестарт внутри окна теряет отложенный пуш.
//! Это осознанно — надёжность догоняется очередью ретраев на стороне приложения
//! (вторая половина Ф2), а не удержанием состояния в ядре.
use std::collections::HashMap;
use std::future::Future;
use std::sync::Mutex;
use std::time::Duration;

use uuid::Uuid;

/// Ключ → «пришло ли новое событие, пока мы работали». Наличие ключа означает,
/// что для него уже есть живой воркер, и второй заводить не нужно.
fn slots() -> &'static Mutex<HashMap<Uuid, bool>> {
    static SLOTS: std::sync::OnceLock<Mutex<HashMap<Uuid, bool>>> = std::sync::OnceLock::new();
    SLOTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Паника в чужом потоке не должна выключать схлопывание навсегда: карта — не
/// инвариант, который можно испортить, поэтому отравленный замок разотравляем.
fn lock() -> std::sync::MutexGuard<'static, HashMap<Uuid, bool>> {
    slots().lock().unwrap_or_else(|e| e.into_inner())
}

/// Отложить `run` на `window`, схлопнув все вызовы с тем же `key`, пришедшие за
/// это время, в один прогон. `window` = 0 тоже проходит через эту машинерию:
/// одновременные вызовы всё равно не должны наложиться друг на друга.
pub fn coalesce<F, Fut>(key: Uuid, window: Duration, run: F)
where
    F: Fn() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send,
{
    {
        let mut slots = lock();
        if let Some(pending) = slots.get_mut(&key) {
            *pending = true; // воркер уже есть — он подхватит
            return;
        }
        slots.insert(key, false);
    }
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(window).await;
            // Флаг снимаем ДО прогона: всё, что накопилось за окно, уедет именно
            // им. События после этой точки честно попросят ещё один виток.
            lock().insert(key, false);
            run().await;
            let again = {
                let mut slots = lock();
                match slots.get(&key) {
                    Some(true) => true,
                    // Ключ убираем под тем же замком, что и проверку: иначе между
                    // «новых нет» и удалением проскочил бы вызов, который решил бы,
                    // что воркер живой, — и не уехал бы никогда.
                    _ => {
                        slots.remove(&key);
                        false
                    }
                }
            };
            if !again {
                break;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn counting() -> (Arc<AtomicUsize>, impl Fn() -> std::future::Ready<()> + Send + Clone) {
        let n = Arc::new(AtomicUsize::new(0));
        let c = n.clone();
        (n, move || {
            c.fetch_add(1, Ordering::SeqCst);
            std::future::ready(())
        })
    }

    #[tokio::test]
    async fn пачка_вызовов_за_окно_даёт_один_прогон() {
        let (n, run) = counting();
        let key = Uuid::new_v4();
        for _ in 0..5 {
            coalesce(key, Duration::from_millis(40), run.clone());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(n.load(Ordering::SeqCst), 1, "пять версий подряд — один пуш");
    }

    #[tokio::test]
    async fn вызов_во_время_прогона_не_теряется() {
        let n = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(tokio::sync::Notify::new());
        let key = Uuid::new_v4();
        let (c, s) = (n.clone(), started.clone());
        let run = move || {
            let (c, s) = (c.clone(), s.clone());
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                s.notify_waiters();
                // Долгий пуш: событие придёт ровно в это окно.
                tokio::time::sleep(Duration::from_millis(120)).await;
            }
        };
        coalesce(key, Duration::from_millis(10), run.clone());
        started.notified().await;
        coalesce(key, Duration::from_millis(10), run);
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(n.load(Ordering::SeqCst), 2, "версия во время пуша обязана уехать вторым витком");
    }

    #[tokio::test]
    async fn разные_ключи_не_схлопываются_друг_с_другом() {
        let (n, run) = counting();
        for _ in 0..3 {
            coalesce(Uuid::new_v4(), Duration::from_millis(10), run.clone());
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(n.load(Ordering::SeqCst), 3, "разные списки — разные пуши");
    }

    #[tokio::test]
    async fn ключ_освобождается_после_прогона() {
        let (n, run) = counting();
        let key = Uuid::new_v4();
        coalesce(key, Duration::from_millis(10), run.clone());
        tokio::time::sleep(Duration::from_millis(120)).await;
        coalesce(key, Duration::from_millis(10), run);
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert_eq!(n.load(Ordering::SeqCst), 2, "новая пачка после паузы едет своим пушем");
        assert!(!lock().contains_key(&key), "слот не течёт");
    }
}
