extern crate libsqlite3_sys as ffi;

mod functions;
#[doc(hidden)]
pub mod raw;
mod serialized_value;
mod sqlite_value;
mod statement_iterator;
mod stmt;

pub use self::sqlite_value::SqliteValue;

use std::os::raw as libc;
use std::rc::Rc;
use std::ptr;
use std::ffi::CString;

use self::raw::RawConnection;
use self::statement_iterator::*;
use self::stmt::{Statement, StatementUse};
use connection::*;
use deserialize::{Queryable, QueryableByName};
use query_builder::bind_collector::RawBytesBindCollector;
use query_builder::*;
use result::*;
use serialize::ToSql;
use sql_types::HasSqlType;
use sqlite::Sqlite;

use std::cell::RefCell;
use std::borrow::BorrowMut;
thread_local! {
    pub static ROTX: RefCell<bool> = RefCell::new(false);
}
/// Connections for the SQLite backend. Unlike other backends, "connection URLs"
/// for SQLite are file paths, [URIs](https://sqlite.org/uri.html), or special
/// identifiers like `:memory:`.
#[allow(missing_debug_implementations)]
pub struct SqliteConnection {
    statement_cache: StatementCache<Sqlite, Statement>,
    raw_connection: Rc<RawConnection>,
    transaction_manager: AnsiTransactionManager,
    on_execute: Option<Box<Fn(&SqliteConnection, &str)>>,
}

struct ReadonlyTx{}
impl ReadonlyTx{
    fn new(){
        ROTX.with(|ro| { *ro.borrow_mut() = true; });
    }
}
impl Drop for ReadonlyTx{
    fn drop(&mut self) {
        ROTX.with(|ro| { *ro.borrow_mut() = false; });
    }
}
pub fn is_readonly_tx() -> bool{
    ROTX.with(|ro| {
        return *ro.borrow();
    })
}

// This relies on the invariant that RawConnection or Statement are never
// leaked. If a reference to one of those was held on a different thread, this
// would not be thread safe.
unsafe impl Send for SqliteConnection {}

impl SimpleConnection for SqliteConnection {
    fn batch_execute(&self, query: &str) -> QueryResult<()> {
        if let Some(ref on_execute) = self.on_execute {
            on_execute(&self, query);
        }
        self.raw_connection.exec(query)
    }
}

impl Connection for SqliteConnection {
    type Backend = Sqlite;
    type TransactionManager = AnsiTransactionManager;

    fn establish(database_url: &str) -> ConnectionResult<Self> {
        RawConnection::establish(database_url).map(|conn| {
            SqliteConnection {
                statement_cache: StatementCache::new(),
                raw_connection: Rc::new(conn),
                transaction_manager: AnsiTransactionManager::new(),
                on_execute: None
            }
        })
    }

    fn transaction<T, E, F>(&self, f: F) -> Result<T, E>
        where
            F: FnOnce() -> Result<T, E>,
            E: From<Error>,
    {
        let _ro_flag = ReadonlyTx::new();
        let transaction_manager = self.transaction_manager();
        try!(transaction_manager.begin_transaction(self));
        match f() {
            Ok(value) => {
                try!(transaction_manager.commit_transaction(self));
                Ok(value)
            }
            Err(e) => {
                try!(transaction_manager.rollback_transaction(self));
                Err(e)
            }
        }
    }

    #[doc(hidden)]
    fn execute(&self, query: &str) -> QueryResult<usize> {
        try!(self.batch_execute(query));
        Ok(self.raw_connection.rows_affected_by_last_query())
    }

    #[doc(hidden)]
    fn query_by_index<T, U>(&self, source: T) -> QueryResult<Vec<U>>
    where
        T: AsQuery,
        T::Query: QueryFragment<Self::Backend> + QueryId,
        Self::Backend: HasSqlType<T::SqlType>,
        U: Queryable<T::SqlType, Self::Backend>,
    {
        let mut statement = try!(self.prepare_query(&source.as_query()));
        let statement_use = StatementUse::new(&mut statement);
        let iter = StatementIterator::new(statement_use);
        iter.collect()
    }

    #[doc(hidden)]
    fn query_by_name<T, U>(&self, source: &T) -> QueryResult<Vec<U>>
    where
        T: QueryFragment<Self::Backend> + QueryId,
        U: QueryableByName<Self::Backend>,
    {
        let mut statement = self.prepare_query(source)?;
        let statement_use = StatementUse::new(&mut statement);
        let iter = NamedStatementIterator::new(statement_use)?;
        iter.collect()
    }

    #[doc(hidden)]
    fn execute_returning_count<T>(&self, source: &T) -> QueryResult<usize>
    where
        T: QueryFragment<Self::Backend> + QueryId,
    {
        let mut statement = try!(self.prepare_query(source));
        let mut statement_use = StatementUse::new(&mut statement);
        try!(statement_use.run());
        Ok(self.raw_connection.rows_affected_by_last_query())
    }

    #[doc(hidden)]
    fn transaction_manager(&self) -> &Self::TransactionManager {
        &self.transaction_manager
    }
}

impl SqliteConnection {
    /// Run a transaction with `BEGIN IMMEDIATE`
    ///
    /// This method will return an error if a transaction is already open.
    ///
    /// # Example
    ///
    /// ```rust
    /// # #[macro_use] extern crate diesel;
    /// # include!("../../doctest_setup.rs");
    /// #
    /// # fn main() {
    /// #     run_test().unwrap();
    /// # }
    /// #
    /// # fn run_test() -> QueryResult<()> {
    /// #     let conn = SqliteConnection::establish(":memory:").unwrap();
    /// conn.immediate_transaction(|| {
    ///     // Do stuff in a transaction
    ///     Ok(())
    /// })
    /// # }
    /// ```
    pub fn immediate_transaction<T, E, F>(&self, f: F) -> Result<T, E>
    where
        F: FnOnce() -> Result<T, E>,
        E: From<Error>,
    {
        self.transaction_sql(f, "BEGIN IMMEDIATE")
    }

    /// Run a transaction with `BEGIN EXCLUSIVE`
    ///
    /// This method will return an error if a transaction is already open.
    ///
    /// # Example
    ///
    /// ```rust
    /// # #[macro_use] extern crate diesel;
    /// # include!("../../doctest_setup.rs");
    /// #
    /// # fn main() {
    /// #     run_test().unwrap();
    /// # }
    /// #
    /// # fn run_test() -> QueryResult<()> {
    /// #     let conn = SqliteConnection::establish(":memory:").unwrap();
    /// conn.exclusive_transaction(|| {
    ///     // Do stuff in a transaction
    ///     Ok(())
    /// })
    /// # }
    /// ```
    pub fn exclusive_transaction<T, E, F>(&self, f: F) -> Result<T, E>
    where
        F: FnOnce() -> Result<T, E>,
        E: From<Error>,
    {
        self.transaction_sql(f, "BEGIN EXCLUSIVE")
    }

    fn transaction_sql<T, E, F>(&self, f: F, sql: &str) -> Result<T, E>
    where
        F: FnOnce() -> Result<T, E>,
        E: From<Error>,
    {
        let transaction_manager = self.transaction_manager();

        transaction_manager.begin_transaction_sql(self, sql)?;
        match f() {
            Ok(value) => {
                transaction_manager.commit_transaction(self)?;
                Ok(value)
            }
            Err(e) => {
                transaction_manager.rollback_transaction(self)?;
                Err(e)
            }
        }
    }

    fn prepare_query<T: QueryFragment<Sqlite> + QueryId>(
        &self,
        source: &T,
    ) -> QueryResult<MaybeCached<Statement>> {
        let mut statement = try!(self.cached_prepared_statement(source));

        let mut bind_collector = RawBytesBindCollector::<Sqlite>::new();
        try!(source.collect_binds(&mut bind_collector, &()));
        let metadata = bind_collector.metadata;
        let binds = bind_collector.binds;
        let value_is_none = bind_collector.value_is_none;
        let mut zip_binds = metadata.into_iter().zip(binds);
        for (column_name, tpe, is_none) in value_is_none.into_iter() {
            // tpe: crate::sqlite::SqliteType
            // source: InsertStatement 带有table信息
            // value: Option<Vec<u8>>，应该default值，如果没有设default值，那就取None
            if is_none {
                let value = None; //Some("test".as_bytes().iter().cloned().collect());
                try!(statement.bind(tpe, value));
            } else {
                let (tpe, value) = zip_binds.next().unwrap();
                try!(statement.bind(tpe, value));
            }
        }

        Ok(statement)
    }

    fn cached_prepared_statement<T: QueryFragment<Sqlite> + QueryId>(
        &self,
        source: &T,
    ) -> QueryResult<MaybeCached<Statement>> {
        self.statement_cache.cached_statement(source, &[], |sql| {
            if let Some(ref on_execute) = self.on_execute {
                on_execute(&self, sql);
            }
            Statement::prepare(&self.raw_connection, sql)
        })
    }

    #[doc(hidden)]
    pub fn register_sql_function<ArgsSqlType, RetSqlType, Args, Ret, F>(
        &self,
        fn_name: &str,
        deterministic: bool,
        f: F,
    ) -> QueryResult<()>
    where
        F: FnMut(Args) -> Ret + Send + 'static,
        Args: Queryable<ArgsSqlType, Sqlite>,
        Ret: ToSql<RetSqlType, Sqlite>,
        Sqlite: HasSqlType<RetSqlType>,
    {
        functions::register(&self.raw_connection, fn_name, deterministic, f)
    }

    pub fn set_on_execute(&mut self, on_execute: Box<Fn(&SqliteConnection, &str)>) {
        self.on_execute = Some(on_execute);
    }

    pub fn clear_on_execute(&mut self) {
        self.on_execute = None;
    }

    pub fn get_fts5_api(&self) -> QueryResult<*mut ffi::fts5_api> {
        let fts_api = CString::new("fts5_api_ptr")?;
        let select_fts = CString::new("SELECT fts5(?1)")?;
        let mut p_ret: *mut ffi::fts5_api = ptr::null_mut();
        let mut p_stmt: *mut ffi::sqlite3_stmt = ptr::null_mut();

        unsafe {
            let mut ret = ffi::sqlite3_prepare_v2(
                self.raw_connection.internal_connection.as_ptr(),
                select_fts.as_ptr(),
                -1,
                &mut p_stmt, ptr::null_mut()
            );
            ::sqlite::connection::stmt::ensure_sqlite_ok(ret, &self.raw_connection)?;
            ret = ffi::sqlite3_bind_pointer(
                p_stmt,
                1,
                &mut p_ret as *mut _ as *mut libc::c_void,
                fts_api.as_ptr(), None
            );
            ::sqlite::connection::stmt::ensure_sqlite_ok(ret, &self.raw_connection)?;
            ffi::sqlite3_step(p_stmt);
            ret = ffi::sqlite3_finalize(p_stmt);
            ::sqlite::connection::stmt::ensure_sqlite_ok(ret, &self.raw_connection)?;
        }
        Ok(p_ret)
    }
}

fn error_message(err_code: libc::c_int) -> &'static str {
    ffi::code_to_str(err_code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsl::sql;
    use prelude::*;
    use sql_types::Integer;

    #[test]
    fn prepared_statements_are_cached_when_run() {
        let connection = SqliteConnection::establish(":memory:").unwrap();
        let query = ::select(1.into_sql::<Integer>());

        assert_eq!(Ok(1), query.get_result(&connection));
        assert_eq!(Ok(1), query.get_result(&connection));
        assert_eq!(1, connection.statement_cache.len());
    }

    #[test]
    fn sql_literal_nodes_are_not_cached() {
        let connection = SqliteConnection::establish(":memory:").unwrap();
        let query = ::select(sql::<Integer>("1"));

        assert_eq!(Ok(1), query.get_result(&connection));
        assert_eq!(0, connection.statement_cache.len());
    }

    #[test]
    fn queries_containing_sql_literal_nodes_are_not_cached() {
        let connection = SqliteConnection::establish(":memory:").unwrap();
        let one_as_expr = 1.into_sql::<Integer>();
        let query = ::select(one_as_expr.eq(sql::<Integer>("1")));

        assert_eq!(Ok(true), query.get_result(&connection));
        assert_eq!(0, connection.statement_cache.len());
    }

    #[test]
    fn queries_containing_in_with_vec_are_not_cached() {
        let connection = SqliteConnection::establish(":memory:").unwrap();
        let one_as_expr = 1.into_sql::<Integer>();
        let query = ::select(one_as_expr.eq_any(vec![1, 2, 3]));

        assert_eq!(Ok(true), query.get_result(&connection));
        assert_eq!(0, connection.statement_cache.len());
    }

    #[test]
    fn queries_containing_in_with_subselect_are_cached() {
        let connection = SqliteConnection::establish(":memory:").unwrap();
        let one_as_expr = 1.into_sql::<Integer>();
        let query = ::select(one_as_expr.eq_any(::select(one_as_expr)));

        assert_eq!(Ok(true), query.get_result(&connection));
        assert_eq!(1, connection.statement_cache.len());
    }

    use sql_types::Text;
    sql_function!(fn fun_case(x: Text) -> Text);

    #[test]
    fn register_custom_function() {
        let connection = SqliteConnection::establish(":memory:").unwrap();
        fun_case::register_impl(&connection, |x: String| {
            x.chars()
                .enumerate()
                .map(|(i, c)| {
                    if i % 2 == 0 {
                        c.to_lowercase().to_string()
                    } else {
                        c.to_uppercase().to_string()
                    }
                })
                .collect::<String>()
        }).unwrap();

        let mapped_string = ::select(fun_case("foobar"))
            .get_result::<String>(&connection)
            .unwrap();
        assert_eq!("fOoBaR", mapped_string);
    }

    sql_function!(fn my_add(x: Integer, y: Integer) -> Integer);

    #[test]
    fn register_multiarg_function() {
        let connection = SqliteConnection::establish(":memory:").unwrap();
        my_add::register_impl(&connection, |x: i32, y: i32| x + y).unwrap();

        let added = ::select(my_add(1, 2)).get_result::<i32>(&connection);
        assert_eq!(Ok(3), added);
    }

    sql_function!(fn add_counter(x: Integer) -> Integer);

    #[test]
    fn register_nondeterministic_function() {
        let connection = SqliteConnection::establish(":memory:").unwrap();
        let mut y = 0;
        add_counter::register_nondeterministic_impl(&connection, move |x: i32| {
            y += 1;
            x + y
        }).unwrap();

        let added = ::select((add_counter(1), add_counter(1), add_counter(1)))
            .get_result::<(i32, i32, i32)>(&connection);
        assert_eq!(Ok((2, 3, 4)), added);
    }
}
