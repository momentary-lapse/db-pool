use std::{
    borrow::Cow,
    fmt::Debug,
    marker::PhantomData,
    ops::{Deref, DerefMut},
};

use async_trait::async_trait;
use uuid::Uuid;

use crate::{common::statement::postgres, util::get_db_name};

use super::super::error::Error as BackendError;

#[async_trait]
pub(super) trait PostgresBackend<'pool>: Send + Sync + 'static {
    type Connection;
    type PooledConnection: DerefMut<Target = Self::Connection>;
    type Pool;

    type BuildError: Into<
            BackendError<
                Self::BuildError,
                Self::PoolError,
                Self::ConnectionError,
                Self::QueryError,
            >,
        > + Debug;
    type PoolError: Into<
            BackendError<
                Self::BuildError,
                Self::PoolError,
                Self::ConnectionError,
                Self::QueryError,
            >,
        > + Debug;
    type ConnectionError: Into<
            BackendError<
                Self::BuildError,
                Self::PoolError,
                Self::ConnectionError,
                Self::QueryError,
            >,
        > + Debug;
    type QueryError: Into<
            BackendError<
                Self::BuildError,
                Self::PoolError,
                Self::ConnectionError,
                Self::QueryError,
            >,
        > + Debug;

    async fn execute_query(
        &self,
        query: &str,
        conn: &mut Self::Connection,
    ) -> Result<(), Self::QueryError>;
    async fn batch_execute_query<'a>(
        &self,
        query: impl IntoIterator<Item = Cow<'a, str>> + Send,
        conn: &mut Self::Connection,
    ) -> Result<(), Self::QueryError>;

    async fn get_default_connection(&'pool self)
    -> Result<Self::PooledConnection, Self::PoolError>;
    async fn establish_privileged_database_connection(
        &self,
        db_id: Uuid,
    ) -> Result<Self::Connection, Self::ConnectionError>;
    async fn establish_restricted_database_connection(
        &self,
        db_id: Uuid,
    ) -> Result<Self::Connection, Self::ConnectionError>;
    fn put_database_connection(&self, db_id: Uuid, conn: Self::Connection);
    fn get_database_connection(&self, db_id: Uuid) -> Self::Connection;

    async fn get_previous_database_names(
        &self,
        conn: &mut Self::Connection,
    ) -> Result<Vec<String>, Self::QueryError>;
    async fn create_entities(&self, conn: Self::Connection) -> Option<Self::Connection>;
    async fn create_connection_pool(&self, db_id: Uuid) -> Result<Self::Pool, Self::BuildError>;

    async fn get_table_names(
        &self,
        privileged_conn: &mut Self::Connection,
    ) -> Result<Vec<String>, Self::QueryError>;

    fn get_drop_previous_databases(&self) -> bool;
}

pub(super) struct PostgresBackendWrapper<'backend, 'pool, B: PostgresBackend<'pool>> {
    inner: &'backend B,
    _marker: &'pool PhantomData<()>,
}

impl<'backend, 'pool, B: PostgresBackend<'pool>> PostgresBackendWrapper<'backend, 'pool, B> {
    pub(super) fn new(backend: &'backend B) -> Self {
        Self {
            inner: backend,
            _marker: &PhantomData,
        }
    }
}

impl<'pool, B: PostgresBackend<'pool>> Deref for PostgresBackendWrapper<'_, 'pool, B> {
    type Target = B;

    fn deref(&self) -> &Self::Target {
        self.inner
    }
}

impl<'backend, 'pool, B> PostgresBackendWrapper<'backend, 'pool, B>
where
    'backend: 'pool,
    B: PostgresBackend<'pool>,
{
    pub(super) async fn init(
        &'backend self,
    ) -> Result<(), BackendError<B::BuildError, B::PoolError, B::ConnectionError, B::QueryError>>
    {
        // Drop previous databases if needed
        if self.get_drop_previous_databases() {
            // Get connection to default database as privileged user
            let conn = &mut self.get_default_connection().await.map_err(Into::into)?;

            // Get previous database names
            let db_names = self
                .get_previous_database_names(conn)
                .await
                .map_err(Into::into)?;

            // Drop databases
            let futures = db_names
                .iter()
                .map(|db_name| async move {
                    let conn = &mut self.get_default_connection().await.map_err(Into::into)?;
                    self.execute_query(postgres::drop_database(db_name.as_str()).as_str(), conn)
                        .await
                        .map_err(Into::into)?;
                    Ok::<
                        _,
                        BackendError<
                            B::BuildError,
                            B::PoolError,
                            B::ConnectionError,
                            B::QueryError,
                        >,
                    >(())
                })
                .collect::<Vec<_>>();
            futures::future::try_join_all(futures).await?;
        }

        Ok(())
    }

    pub(super) async fn create(
        &'backend self,
        db_id: Uuid,
        restrict_privileges: bool,
    ) -> Result<B::Pool, BackendError<B::BuildError, B::PoolError, B::ConnectionError, B::QueryError>>
    {
        // Get database name based on UUID
        let db_name = get_db_name(db_id);
        let db_name = db_name.as_str();

        // Get connection to default database as privileged user
        let default_conn = &mut self.get_default_connection().await.map_err(Into::into)?;

        // Create database
        self.execute_query(postgres::create_database(db_name).as_str(), default_conn)
            .await
            .map_err(Into::into)?;

        // Create role
        self.execute_query(postgres::create_role(db_name).as_str(), default_conn)
            .await
            .map_err(Into::into)?;

        if restrict_privileges {
            // Connect to database as privileged user
            let establish_connection = || async {
                self.establish_privileged_database_connection(db_id)
                    .await
                    .map_err(Into::into)
            };

            let conn = establish_connection().await?;

            // Create entities as privileged user and get back connection if possible
            let mut conn = match self.create_entities(conn).await {
                None => establish_connection().await?,
                Some(conn) => conn,
            };

            // Grant table privileges to restricted role
            self.execute_query(
                postgres::grant_restricted_table_privileges(db_name).as_str(),
                &mut conn,
            )
            .await
            .map_err(Into::into)?;

            // Grant sequence privileges to restricted role
            self.execute_query(
                postgres::grant_restricted_sequence_privileges(db_name).as_str(),
                &mut conn,
            )
            .await
            .map_err(Into::into)?;

            // Store database connection for reuse when cleaning
            self.put_database_connection(db_id, conn);
        } else {
            // Grant database ownership to database-unrestricted role
            self.execute_query(
                postgres::grant_database_ownership(db_name, db_name).as_str(),
                default_conn,
            )
            .await
            .map_err(Into::into)?;

            // Connect to database as database-unrestricted user
            let conn = self
                .establish_restricted_database_connection(db_id)
                .await
                .map_err(Into::into)?;

            // Create entities as database-unrestricted user
            let _ = self.create_entities(conn).await;
        }

        // Create connection pool with attached role
        let pool = self
            .create_connection_pool(db_id)
            .await
            .map_err(Into::into)?;

        Ok(pool)
    }

    pub(super) async fn clean(
        &'backend self,
        db_id: Uuid,
    ) -> Result<(), BackendError<B::BuildError, B::PoolError, B::ConnectionError, B::QueryError>>
    {
        // Get privileged connection to database
        let mut conn = self.get_database_connection(db_id);

        // Get table names
        let table_names = self.get_table_names(&mut conn).await.map_err(Into::into)?;

        // Generate truncate statements
        let stmts = table_names
            .iter()
            .map(|table_name| postgres::truncate_table(table_name.as_str()).into());

        // Truncate tables
        self.batch_execute_query(stmts, &mut conn)
            .await
            .map_err(Into::into)?;

        // Store database connection back for reuse
        self.put_database_connection(db_id, conn);

        Ok(())
    }

    pub(super) async fn drop(
        &'backend self,
        db_id: Uuid,
        is_restricted: bool,
    ) -> Result<(), BackendError<B::BuildError, B::PoolError, B::ConnectionError, B::QueryError>>
    {
        // Drop privileged connection to database
        if is_restricted {
            self.get_database_connection(db_id);
        }

        // Get database name based on UUID
        let db_name = get_db_name(db_id);
        let db_name = db_name.as_str();

        // Get connection to default database as privileged user
        let conn = &mut self.get_default_connection().await.map_err(Into::into)?;

        // Drop database
        self.execute_query(postgres::drop_database(db_name).as_str(), conn)
            .await
            .map_err(Into::into)?;

        // Drop attached role
        self.execute_query(postgres::drop_role(db_name).as_str(), conn)
            .await
            .map_err(Into::into)?;

        Ok(())
    }
}

#[cfg(test)]
pub(super) mod tests {
    #![allow(clippy::unwrap_used)]

    use bb8::Pool as Bb8Pool;
    use diesel::{dsl::exists, insert_into, prelude::*, select, sql_query, table};
    use diesel_async::{
        AsyncPgConnection, RunQueryDsl, pooled_connection::AsyncDieselConnectionManager,
    };
    use futures::{
        Future,
        future::{join_all, try_join_all},
    };
    use tokio::sync::OnceCell;
    use uuid::Uuid;

    use crate::{
        r#async::{backend::r#trait::Backend, db_pool::DatabasePoolBuilder},
        common::statement::postgres::tests::{DDL_STATEMENTS, DML_STATEMENTS},
        tests::{PG_DROP_LOCK, get_privileged_postgres_config},
        util::get_db_name,
    };

    pub type Pool = Bb8Pool<AsyncDieselConnectionManager<AsyncPgConnection>>;

    table! {
        pg_database (oid) {
            oid -> Int4,
            datname -> Text
        }
    }

    table! {
        book (id) {
            id -> Int4,
            title -> Text
        }
    }

    #[allow(unused_variables)]
    pub trait PgDropLock<T>
    where
        Self: Future<Output = T> + Sized,
    {
        async fn lock_drop(self) -> T {
            let guard = PG_DROP_LOCK.write().await;
            self.await
        }

        async fn lock_read(self) -> T {
            let guard = PG_DROP_LOCK.read().await;
            self.await
        }
    }

    impl<T, F> PgDropLock<T> for F where F: Future<Output = T> + Sized {}

    async fn get_privileged_connection_pool() -> &'static Pool {
        static POOL: OnceCell<Pool> = OnceCell::const_new();
        POOL.get_or_init(|| async {
            let config = get_privileged_postgres_config();
            let connection_url = config.default_connection_url();
            let manager = AsyncDieselConnectionManager::new(connection_url);
            Bb8Pool::builder().build(manager).await.unwrap()
        })
        .await
    }

    async fn create_restricted_connection_pool(db_name: &str) -> Pool {
        let config = get_privileged_postgres_config();
        let connection_url =
            config.restricted_database_connection_url(db_name, Some(db_name), db_name);
        let manager = AsyncDieselConnectionManager::new(connection_url);
        Bb8Pool::builder().build(manager).await.unwrap()
    }

    async fn create_database(conn: &mut AsyncPgConnection) -> String {
        let db_id = Uuid::new_v4();
        let db_name = get_db_name(db_id);
        sql_query(format!("CREATE DATABASE {db_name}"))
            .execute(conn)
            .await
            .unwrap();
        db_name
    }

    async fn create_databases(count: i64, pool: &Pool) -> Vec<String> {
        let futures = (0..count)
            .map(|_| async {
                let conn = &mut pool.get().await.unwrap();
                create_database(conn).await
            })
            .collect::<Vec<_>>();
        join_all(futures).await
    }

    async fn count_databases(db_names: &Vec<String>, conn: &mut AsyncPgConnection) -> i64 {
        pg_database::table
            .filter(pg_database::datname.eq_any(db_names))
            .count()
            .get_result(conn)
            .await
            .unwrap()
    }

    async fn count_all_databases(conn: &mut AsyncPgConnection) -> i64 {
        pg_database::table
            .filter(pg_database::datname.like("db_pool_%"))
            .count()
            .get_result(conn)
            .await
            .unwrap()
    }

    async fn database_exists(db_name: &str, conn: &mut AsyncPgConnection) -> bool {
        select(exists(
            pg_database::table.filter(pg_database::datname.eq(db_name)),
        ))
        .get_result(conn)
        .await
        .unwrap()
    }

    async fn insert_books(count: i64, conn: &mut AsyncPgConnection) {
        #[derive(Insertable)]
        #[diesel(table_name = book)]
        struct NewBook {
            title: String,
        }

        let new_books = (0..count)
            .map(|i| NewBook {
                title: format!("Title {}", i + 1),
            })
            .collect::<Vec<_>>();

        insert_into(book::table)
            .values(&new_books)
            .execute(conn)
            .await
            .unwrap();
    }

    pub async fn test_backend_drops_previous_databases<B: Backend>(
        default: B,
        enabled: B,
        disabled: B,
    ) {
        const NUM_DBS: i64 = 3;

        async {
            let conn_pool = get_privileged_connection_pool().await;
            let conn = &mut conn_pool.get().await.unwrap();

            for (backend, cleans) in [(default, true), (enabled, true), (disabled, false)] {
                let db_names = create_databases(NUM_DBS, conn_pool).await;
                assert_eq!(count_databases(&db_names, conn).await, NUM_DBS);
                backend.init().await.unwrap();
                assert_eq!(
                    count_databases(&db_names, conn).await,
                    if cleans { 0 } else { NUM_DBS }
                );
            }
        }
        .lock_drop()
        .await;
    }

    pub async fn test_backend_creates_database_with_restricted_privileges(backend: impl Backend) {
        let db_id = Uuid::new_v4();
        let db_name = get_db_name(db_id);
        let db_name = db_name.as_str();

        async {
            // privileged operations
            {
                let conn_pool = get_privileged_connection_pool().await;
                let conn = &mut conn_pool.get().await.unwrap();

                // database must not exist
                assert!(!database_exists(db_name, conn).await);

                // database must exist after creating through backend
                backend.init().await.unwrap();
                backend.create(db_id, true).await.unwrap();
                assert!(database_exists(db_name, conn).await);
            }

            // restricted operations
            {
                let conn_pool = &mut create_restricted_connection_pool(db_name).await;
                let conn = &mut conn_pool.get().await.unwrap();

                // DDL statements must fail
                for stmt in DDL_STATEMENTS {
                    assert!(sql_query(stmt).execute(conn).await.is_err());
                }

                // DML statements must succeed
                for stmt in DML_STATEMENTS {
                    assert!(sql_query(stmt).execute(conn).await.is_ok());
                }
            }
        }
        .lock_read()
        .await;
    }

    pub async fn test_backend_creates_database_with_unrestricted_privileges(backend: impl Backend) {
        async {
            {
                let db_id = Uuid::new_v4();
                let db_name = get_db_name(db_id);
                let db_name = db_name.as_str();

                // privileged operations
                {
                    let conn_pool = get_privileged_connection_pool().await;
                    let conn = &mut conn_pool.get().await.unwrap();

                    // database must not exist
                    assert!(!database_exists(db_name, conn).await);

                    // database must exist after creating through backend
                    backend.init().await.unwrap();
                    backend.create(db_id, false).await.unwrap();
                    assert!(database_exists(db_name, conn).await);
                }

                // DML statements must succeed
                {
                    let conn_pool = create_restricted_connection_pool(db_name).await;
                    let conn = &mut conn_pool.get().await.unwrap();
                    for stmt in DML_STATEMENTS {
                        let result = sql_query(stmt).execute(conn).await;
                        assert!(result.is_ok());
                    }
                }
            }

            // DDL statements must succeed
            try_join_all(DDL_STATEMENTS.iter().map(|stmt| {
                let backend = &backend;
                async move {
                    let db_id = Uuid::new_v4();
                    let db_name = get_db_name(db_id);
                    let db_name = db_name.as_str();

                    backend.create(db_id, false).await.unwrap();
                    let conn_pool = create_restricted_connection_pool(db_name).await;
                    let conn = &mut conn_pool.get().await.unwrap();

                    sql_query(*stmt).execute(conn).await
                }
            }))
            .await
            .unwrap();
        }
        .lock_read()
        .await;
    }

    pub async fn test_backend_cleans_database_with_tables(backend: impl Backend) {
        const NUM_BOOKS: i64 = 3;

        let db_id = Uuid::new_v4();
        let db_name = get_db_name(db_id);
        let db_name = db_name.as_str();

        async {
            backend.init().await.unwrap();
            backend.create(db_id, true).await.unwrap();

            let conn_pool = &mut create_restricted_connection_pool(db_name).await;
            let conn = &mut conn_pool.get().await.unwrap();

            insert_books(NUM_BOOKS, conn).await;

            // there must be books
            assert_eq!(
                book::table.count().get_result::<i64>(conn).await.unwrap(),
                NUM_BOOKS
            );

            backend.clean(db_id).await.unwrap();

            // there must be no books
            assert_eq!(
                book::table.count().get_result::<i64>(conn).await.unwrap(),
                0
            );
        }
        .lock_read()
        .await;
    }

    pub async fn test_backend_cleans_database_without_tables(backend: impl Backend) {
        let db_id = Uuid::new_v4();

        async {
            backend.init().await.unwrap();
            backend.create(db_id, true).await.unwrap();
            backend.clean(db_id).await.unwrap();
        }
        .lock_read()
        .await;
    }

    pub async fn test_backend_drops_database(backend: impl Backend, restricted: bool) {
        let db_id = Uuid::new_v4();
        let db_name = get_db_name(db_id);
        let db_name = db_name.as_str();

        let conn_pool = get_privileged_connection_pool().await;
        let conn = &mut conn_pool.get().await.unwrap();

        async {
            // database must exist
            backend.init().await.unwrap();
            backend.create(db_id, restricted).await.unwrap();
            assert!(database_exists(db_name, conn).await);

            // database must not exist
            backend.drop(db_id, restricted).await.unwrap();
            assert!(!database_exists(db_name, conn).await);
        }
        .lock_read()
        .await;
    }

    pub async fn test_pool_drops_previous_databases<B: Backend>(
        default: B,
        enabled: B,
        disabled: B,
    ) {
        const NUM_DBS: i64 = 3;

        async {
            let conn_pool = get_privileged_connection_pool().await;
            let conn = &mut conn_pool.get().await.unwrap();

            for (backend, cleans) in [(default, true), (enabled, true), (disabled, false)] {
                let db_names = create_databases(NUM_DBS, conn_pool).await;
                assert_eq!(count_databases(&db_names, conn).await, NUM_DBS);
                backend.create_database_pool().await.unwrap();
                assert_eq!(
                    count_databases(&db_names, conn).await,
                    if cleans { 0 } else { NUM_DBS }
                );
            }
        }
        .lock_drop()
        .await;
    }

    pub async fn test_pool_drops_created_restricted_databases(backend: impl Backend) {
        const NUM_DBS: i64 = 3;

        let conn_pool = get_privileged_connection_pool().await;
        let conn = &mut conn_pool.get().await.unwrap();

        async {
            let db_pool = backend.create_database_pool().await.unwrap();

            // there must be no databases
            assert_eq!(count_all_databases(conn).await, 0);

            // fetch connection pools
            let conn_pools = join_all((0..NUM_DBS).map(|_| db_pool.pull_immutable())).await;

            // there must be databases
            assert_eq!(count_all_databases(conn).await, NUM_DBS);

            // must release databases back to pool
            drop(conn_pools);

            // there must be databases
            assert_eq!(count_all_databases(conn).await, NUM_DBS);

            // must drop databases
            drop(db_pool);

            // there must be no databases
            assert_eq!(count_all_databases(conn).await, 0);
        }
        .lock_drop()
        .await;
    }

    pub async fn test_pool_drops_created_unrestricted_database(backend: impl Backend) {
        let conn_pool = get_privileged_connection_pool().await;
        let conn = &mut conn_pool.get().await.unwrap();

        async {
            let db_pool = backend.create_database_pool().await.unwrap();

            // there must be no databases
            assert_eq!(count_all_databases(conn).await, 0);

            // fetch connection pool
            let conn_pool = db_pool.create_mutable().await.unwrap();

            // there must be a database
            assert_eq!(count_all_databases(conn).await, 1);

            // must drop database
            drop(conn_pool);

            // there must be no databases
            assert_eq!(count_all_databases(conn).await, 0);

            drop(db_pool);

            // there must be no databases
            assert_eq!(count_all_databases(conn).await, 0);
        }
        .lock_drop()
        .await;
    }
}
