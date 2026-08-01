//! Ф2: схлопывание фоновых задач по ключу — не чаще одного прогона за окно.
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
//! ⚠️ **Это throttle, а НЕ debounce, и разница здесь принципиальная.** Окно
//! отсчитывается от ПЕРВОГО события пачки и новыми событиями НЕ продлевается.
//! Классический debounce («ждём тишины, каждое новое событие перезапускает
//! таймер») для зеркала — ошибка: пока человек правит список чаще, чем раз в
//! окно, тишина не наступает вовсе, и зеркало не обновляется НИ РАЗУ — ровно
//! тогда, когда список меняется активнее всего. Здесь отставание ограничено
//! сверху окном плюс длительность пуша, при любой частоте правок.
//!
//! Так же устроено у GitLab: push mirror получает изменение «in five minutes, or
//! one minute if Only mirror protected branches is on» — фиксированный интервал,
//! а не ожидание тишины (docs.gitlab.com, Push mirroring).
//!
//! Событие, пришедшее ВО ВРЕМЯ прогона, не проглатывается, а даёт ещё один виток:
//! иначе версия, закоммиченная между «git push прочитал ref» и «git push
//! завершился», уехала бы на зеркало только со следующей записью — то есть,
//! возможно, никогда.
//!
//! ⚠️ Окно живёт в памяти процесса: рестарт внутри окна теряет отложенный пуш.
//! Это осознанно — надёжность догоняется очередью ретраев на стороне приложения
//! (вторая половина Ф2), а не удержанием состояния в ядре.
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
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
/// это время, в один прогон. Окно НЕ продлевается новыми вызовами — почему
/// именно так, см. шапку модуля. `window` = 0 тоже проходит через эту машинерию:
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

/// Ключ → замок. Живёт, пока замком кто-то пользуется (см. уборку в `serialized`).
fn gates() -> &'static Mutex<HashMap<Uuid, Arc<tokio::sync::Mutex<()>>>> {
    static GATES: std::sync::OnceLock<Mutex<HashMap<Uuid, Arc<tokio::sync::Mutex<()>>>>> =
        std::sync::OnceLock::new();
    GATES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Выполнить `fut` так, чтобы для одного `key` в каждый момент шёл ровно один —
/// остальные ЖДУТ, а не пропускаются (в отличие от `coalesce`).
///
/// Зачем отдельно от схлопывания. Схлопывание годится там, где лишний прогон
/// просто не нужен; здесь другое — прогоны нужны все, но их РЕЗУЛЬТАТЫ пишутся в
/// одну строку, и порядок записи обязан совпадать с порядком выполнения.
/// Конкретный случай (авто-ревью core#78): фоновый пуш зеркала висит в сетевом
/// таймауте, владелец жмёт «Синхронизировать», ручной пуш успевает и обнуляет
/// счётчик неудач — а следом падает старый фоновый и снова помечает зеркало
/// сломанным. Синхронизированное зеркало выглядело бы упавшим, и подметальщик
/// повторял бы пуш, который не нужен.
///
/// Ждать безопасно: время удержания ограничено таймаутом самого пуша.
pub async fn serialized<F, T>(key: Uuid, fut: F) -> T
where
    F: Future<Output = T>,
{
    // Уборка через Drop, а не строчкой в конце: `serialized` могут БРОСИТЬ —
    // истёк дедлайн gRPC, клиент отвалился, паника выше по стеку. Тогда код
    // после `await` не выполнится никогда, и запись осталась бы в карте до конца
    // жизни процесса (авто-ревью core#78). Drop отрабатывает и при отмене, и при
    // раскрутке паники.
    //
    // ⚠️ Порядок объявления значим: локальные переменные дропаются в обратном
    // порядке, поэтому сначала объявляем уборщика, потом `gate` — иначе уборщик
    // считал бы ссылки, пока наш собственный клон ещё жив, и не убирал НИКОГДА.
    let _cleanup = GateCleanup(key);
    let gate = {
        let mut gates = gates().lock().unwrap_or_else(|e| e.into_inner());
        Arc::clone(gates.entry(key).or_default())
    };
    let _held = gate.lock().await;
    fut.await
}

/// Снимает замок из карты, когда им больше никто не пользуется.
///
/// `strong_count == 1` означает «держит только сама карта», и проверять это
/// безопасно ровно под её замком: новый клон можно получить лишь через неё.
/// Поэтому ждущий в очереди (у него свой клон) не может быть выброшен из карты и
/// разъехаться с тем, кто зайдёт следом.
struct GateCleanup(Uuid);

impl Drop for GateCleanup {
    fn drop(&mut self) {
        let mut gates = gates().lock().unwrap_or_else(|e| e.into_inner());
        if gates.get(&self.0).is_some_and(|g| Arc::strong_count(g) == 1) {
            gates.remove(&self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

    // Разница между throttle и debounce, из-за которой модуль так назван: поток
    // правок ЧАЩЕ окна не имеет права заморозить зеркало. Классический debounce с
    // перезапуском таймера дал бы здесь ноль прогонов, пока правки не кончатся.
    #[tokio::test]
    async fn непрерывный_поток_правок_не_морозит_зеркало() {
        let (n, run) = counting();
        let key = Uuid::new_v4();
        for _ in 0..20 {
            coalesce(key, Duration::from_millis(30), run.clone());
            tokio::time::sleep(Duration::from_millis(15)).await; // правки чаще окна
        }
        let during = n.load(Ordering::SeqCst);
        assert!(during >= 3, "за 300мс правок при окне 30мс зеркало обязано обновиться, а было {during}");
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

    // ── serialized ────────────────────────────────────────────────────────
    // Гонка, ради которой это заведено: результаты пишутся в одну строку, и
    // порядок записи обязан совпадать с порядком выполнения.

    #[tokio::test]
    async fn два_прогона_одного_ключа_не_накладываются() {
        let key = Uuid::new_v4();
        let inside = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for _ in 0..4 {
            let (inside, peak) = (inside.clone(), peak.clone());
            tasks.push(tokio::spawn(async move {
                serialized(key, async {
                    let n = inside.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(n, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    inside.fetch_sub(1, Ordering::SeqCst);
                })
                .await
            }));
        }
        for t in tasks {
            t.await.expect("join");
        }
        assert_eq!(peak.load(Ordering::SeqCst), 1, "одновременных прогонов быть не должно");
    }

    #[tokio::test]
    async fn порядок_завершения_совпадает_с_порядком_входа() {
        // Ради этого всё и делается: старый исход не имеет права записаться
        // ПОСЛЕ нового и объявить синхронизированное зеркало сломанным.
        let key = Uuid::new_v4();
        let order = Arc::new(Mutex::new(Vec::<u8>::new()));
        let first = {
            let order = order.clone();
            tokio::spawn(async move {
                serialized(key, async {
                    tokio::time::sleep(Duration::from_millis(60)).await;
                    order.lock().expect("lock").push(1);
                })
                .await
            })
        };
        tokio::time::sleep(Duration::from_millis(10)).await; // второй входит позже
        let second = {
            let order = order.clone();
            tokio::spawn(async move {
                serialized(key, async {
                    order.lock().expect("lock").push(2);
                })
                .await
            })
        };
        first.await.expect("join");
        second.await.expect("join");
        assert_eq!(*order.lock().expect("lock"), vec![1, 2], "быстрый второй не обгоняет медленный первый");
    }

    #[tokio::test]
    async fn разные_ключи_идут_одновременно() {
        let started = Arc::new(tokio::sync::Barrier::new(2));
        let mut tasks = Vec::new();
        for _ in 0..2 {
            let started = started.clone();
            let key = Uuid::new_v4();
            tasks.push(tokio::spawn(async move { serialized(key, async { started.wait().await }).await }));
        }
        // Барьер разойдётся только если оба зашли внутрь одновременно; замок на
        // разные списки не имеет права их выстроить в очередь.
        let both = tokio::time::timeout(Duration::from_secs(2), async {
            for t in tasks {
                t.await.expect("join");
            }
        })
        .await;
        assert!(both.is_ok(), "замки разных списков не должны мешать друг другу");
    }

    #[tokio::test]
    async fn замок_не_течёт() {
        let key = Uuid::new_v4();
        serialized(key, async {}).await;
        assert!(!gates().lock().expect("lock").contains_key(&key), "карта замков не растёт вечно");
    }

    #[tokio::test]
    async fn брошенный_вызов_тоже_убирает_за_собой() {
        // У ручного пуша есть дедлайн gRPC: вызов БРОСАЮТ, не доводя до конца, и
        // код после await не выполняется никогда. Пока уборка была строчкой в
        // конце функции, запись оставалась в карте до конца жизни процесса.
        //
        // Брошенный вызов здесь ЕДИНСТВЕННЫЙ — иначе тест ничего не доказывает:
        // при живом соседе запись убрал бы он, и старая реализация тоже прошла бы.
        let key = Uuid::new_v4();
        let dropped = tokio::time::timeout(
            Duration::from_millis(30),
            serialized(key, async {
                tokio::time::sleep(Duration::from_millis(300)).await;
            }),
        )
        .await;
        assert!(dropped.is_err(), "вызов обязан быть брошен на дедлайне — иначе тест проверяет не то");
        assert!(
            !gates().lock().expect("lock").contains_key(&key),
            "запись обязана уйти вместе с брошенным вызовом, а не жить до перезапуска"
        );
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
