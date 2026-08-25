//! Curation — порт CurationStore (звёзды/watch), read + write.
//! READ: простые exists/count. WRITE зеркалит src/features/curation/adapter.ts:
//! toggle возвращает НОВОЕ состояние; звезда двигает templates.stars_count под
//! одной транзакцией.
use sqlx::postgres::PgPool;
use tonic::{Request, Response, Status};
use uuid::Uuid;

use super::util::{db_status, parse_id};
use crate::pb_domain::curation_read_server::CurationRead;
use crate::pb_domain::curation_write_server::CurationWrite;
use crate::pb_domain::{BoolResponse, CountResponse, IdsResponse, ListId, UserList};

/// CurationRead: exists/count по звёздам и watch.
pub struct CurationReadSvc {
    pub pool: PgPool,
}

#[tonic::async_trait]
impl CurationRead for CurationReadSvc {
    async fn is_starred(&self, req: Request<UserList>) -> Result<Response<BoolResponse>, Status> {
        let UserList { list_id, user_id } = req.into_inner();
        let (tid, uid) = (parse_id(&list_id)?, parse_id(&user_id)?);
        let row: Option<(i32,)> =
            sqlx::query_as("select 1 from stars where template_id = $1 and user_id = $2 limit 1")
                .bind(tid)
                .bind(uid)
                .fetch_optional(&self.pool)
                .await
                .map_err(db_status)?;
        Ok(Response::new(BoolResponse { value: row.is_some() }))
    }

    async fn is_watching(&self, req: Request<UserList>) -> Result<Response<BoolResponse>, Status> {
        let UserList { list_id, user_id } = req.into_inner();
        let (tid, uid) = (parse_id(&list_id)?, parse_id(&user_id)?);
        let row: Option<(i32,)> =
            sqlx::query_as("select 1 from watches where template_id = $1 and user_id = $2 limit 1")
                .bind(tid)
                .bind(uid)
                .fetch_optional(&self.pool)
                .await
                .map_err(db_status)?;
        Ok(Response::new(BoolResponse { value: row.is_some() }))
    }

    async fn watch_count(&self, req: Request<ListId>) -> Result<Response<CountResponse>, Status> {
        let tid = parse_id(&req.into_inner().id)?;
        let (n,): (i64,) = sqlx::query_as("select count(*) from watches where template_id = $1")
            .bind(tid)
            .fetch_one(&self.pool)
            .await
            .map_err(db_status)?;
        Ok(Response::new(CountResponse { value: n as i32 }))
    }

    async fn watcher_ids(&self, req: Request<ListId>) -> Result<Response<IdsResponse>, Status> {
        let tid = parse_id(&req.into_inner().id)?;
        let rows: Vec<(Uuid,)> =
            sqlx::query_as("select user_id from watches where template_id = $1 order by created_at asc")
                .bind(tid)
                .fetch_all(&self.pool)
                .await
                .map_err(db_status)?;
        Ok(Response::new(IdsResponse { ids: rows.iter().map(|r| r.0.to_string()).collect() }))
    }
}

/// CurationWrite: toggle звёзд/watch (звезда двигает stars_count транзакционно).
pub struct CurationWriteSvc {
    pub pool: PgPool,
}

#[tonic::async_trait]
impl CurationWrite for CurationWriteSvc {
    async fn toggle_star(&self, req: Request<UserList>) -> Result<Response<BoolResponse>, Status> {
        let UserList { list_id, user_id } = req.into_inner();
        let (tid, uid) = (parse_id(&list_id)?, parse_id(&user_id)?);
        let mut tx = self.pool.begin().await.map_err(db_status)?;
        // СЧЁТЧИК ДВИГАЕТСЯ ПО ФАКТУ ЗАПИСИ, а не по прочитанному состоянию.
        //
        // Раньше было «прочитал → решил → записал»: шесть одновременных нажатий одного
        // человека читали «звезды нет», вставляли (строка одна, `on conflict do nothing`)
        // — и КАЖДОЕ увеличивало счётчик. Замер пробы `star_count_probe`: строк 1,
        // счётчик 4 (дефект 1 реестра ревью ядра). Это корень K32 карты корней.
        //
        // Теперь считаем то, что действительно произошло: `rows_affected` у вставки и
        // удаления. Замок не нужен — уникальный индекс сам решает, кто первый, а
        // проигравшему достаётся ноль строк и ноль изменений счётчика.
        let existed: Option<(i32,)> =
            sqlx::query_as("select 1 from stars where user_id = $1 and template_id = $2 limit 1")
                .bind(uid)
                .bind(tid)
                .fetch_optional(&mut *tx)
                .await
                .map_err(db_status)?;
        let now_starred = if existed.is_some() {
            let ушло = sqlx::query("delete from stars where user_id = $1 and template_id = $2")
                .bind(uid)
                .bind(tid)
                .execute(&mut *tx)
                .await
                .map_err(db_status)?
                .rows_affected();
            if ушло > 0 {
                sqlx::query("update templates set stars_count = GREATEST(stars_count - 1, 0) where id = $1")
                    .bind(tid)
                    .execute(&mut *tx)
                    .await
                    .map_err(db_status)?;
            }
            false
        } else {
            let легло = sqlx::query(
                "insert into stars (user_id, template_id) values ($1, $2) on conflict do nothing",
            )
            .bind(uid)
            .bind(tid)
            .execute(&mut *tx)
            .await
            .map_err(db_status)?
            .rows_affected();
            if легло > 0 {
                sqlx::query("update templates set stars_count = stars_count + 1 where id = $1")
                    .bind(tid)
                    .execute(&mut *tx)
                    .await
                    .map_err(db_status)?;
            }
            true
        };
        tx.commit().await.map_err(db_status)?;
        Ok(Response::new(BoolResponse { value: now_starred }))
    }

    async fn toggle_watch(&self, req: Request<UserList>) -> Result<Response<BoolResponse>, Status> {
        let UserList { list_id, user_id } = req.into_inner();
        let (tid, uid) = (parse_id(&list_id)?, parse_id(&user_id)?);
        let existed: Option<(i32,)> =
            sqlx::query_as("select 1 from watches where user_id = $1 and template_id = $2 limit 1")
                .bind(uid)
                .bind(tid)
                .fetch_optional(&self.pool)
                .await
                .map_err(db_status)?;
        let now_watching = if existed.is_some() {
            sqlx::query("delete from watches where user_id = $1 and template_id = $2")
                .bind(uid)
                .bind(tid)
                .execute(&self.pool)
                .await
                .map_err(db_status)?;
            false
        } else {
            sqlx::query("insert into watches (user_id, template_id) values ($1, $2) on conflict do nothing")
                .bind(uid)
                .bind(tid)
                .execute(&self.pool)
                .await
                .map_err(db_status)?;
            true
        };
        Ok(Response::new(BoolResponse { value: now_watching }))
    }

    async fn ensure_watch(&self, req: Request<UserList>) -> Result<Response<BoolResponse>, Status> {
        let UserList { list_id, user_id } = req.into_inner();
        let (tid, uid) = (parse_id(&list_id)?, parse_id(&user_id)?);
        sqlx::query("insert into watches (user_id, template_id) values ($1, $2) on conflict do nothing")
            .bind(uid)
            .bind(tid)
            .execute(&self.pool)
            .await
            .map_err(db_status)?;
        Ok(Response::new(BoolResponse { value: true }))
    }
}
