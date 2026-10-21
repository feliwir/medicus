//! Dialect-aware statement builder and the minimal interface a SQL backend implements.

use std::fmt::Write as _;

use async_trait::async_trait;
use medicus_core::Result;

/// SQL dialect differences the shared code has to know about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    Sqlite,
    Postgres,
}

/// A bound argument or a fetched value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cell {
    Null,
    Text(String),
    Int(i64),
    /// A JSON document, bound as text and converted to the dialect's binary JSON type
    /// (`jsonb(?)` in `SQLite`, `$n::jsonb` in `PostgreSQL`). Only used for arguments.
    Json(String),
}

impl Cell {
    pub fn text(&self) -> Option<&str> {
        match self {
            Self::Text(s) => Some(s),
            _ => None,
        }
    }

    pub fn int(&self) -> Option<i64> {
        match self {
            Self::Int(i) => Some(*i),
            _ => None,
        }
    }
}

impl From<&str> for Cell {
    fn from(s: &str) -> Self {
        Self::Text(s.to_owned())
    }
}

impl From<String> for Cell {
    fn from(s: String) -> Self {
        Self::Text(s)
    }
}

impl From<i64> for Cell {
    fn from(i: i64) -> Self {
        Self::Int(i)
    }
}

impl<T: Into<Cell>> From<Option<T>> for Cell {
    fn from(v: Option<T>) -> Self {
        v.map_or(Self::Null, Into::into)
    }
}

/// How a fetched column is decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellKind {
    Text,
    Int,
}

/// A statement with positional arguments, rendered for one dialect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stmt {
    pub sql: String,
    pub args: Vec<Cell>,
    dialect: Dialect,
}

impl Stmt {
    pub fn new(dialect: Dialect) -> Self {
        Self {
            sql: String::new(),
            args: Vec::new(),
            dialect,
        }
    }

    /// A statement without arguments.
    pub fn raw(dialect: Dialect, sql: &str) -> Self {
        let mut s = Self::new(dialect);
        s.sql.push_str(sql);
        s
    }

    pub fn dialect(&self) -> Dialect {
        self.dialect
    }

    pub fn push(&mut self, sql: &str) -> &mut Self {
        self.sql.push_str(sql);
        self
    }

    /// Appends a placeholder and binds `value` to it.
    pub fn bind(&mut self, value: impl Into<Cell>) -> &mut Self {
        let value = value.into();
        let json = matches!(value, Cell::Json(_));
        self.args.push(value);
        let n = self.args.len();
        match (self.dialect, json) {
            (Dialect::Sqlite, false) => self.sql.push('?'),
            (Dialect::Sqlite, true) => self.sql.push_str("jsonb(?)"),
            (Dialect::Postgres, false) => {
                let _ = write!(self.sql, "${n}");
            }
            (Dialect::Postgres, true) => {
                let _ = write!(self.sql, "${n}::jsonb");
            }
        }
        self
    }

    /// Appends `(?, ?, ...)` for a list of values.
    pub fn bind_list<I, T>(&mut self, values: I) -> &mut Self
    where
        I: IntoIterator<Item = T>,
        T: Into<Cell>,
    {
        self.push("(");
        for (i, v) in values.into_iter().enumerate() {
            if i > 0 {
                self.push(", ");
            }
            self.bind(v);
        }
        self.push(")")
    }
}

/// A connection inside a transaction. Dropping it without [`Tx::commit`] rolls back.
#[async_trait]
pub trait Tx: Send {
    fn dialect(&self) -> Dialect;

    /// Executes a statement; returns the number of affected rows.
    async fn execute(&mut self, stmt: &Stmt) -> Result<u64>;

    /// Runs a query and decodes each row's columns according to `kinds`.
    async fn fetch(&mut self, stmt: &Stmt, kinds: &[CellKind]) -> Result<Vec<Vec<Cell>>>;

    async fn commit(self) -> Result<()>;
}

/// What a concrete database backend provides to [`crate::SqlIndex`].
#[async_trait]
pub trait Backend: Send + Sync + 'static {
    type Tx: Tx;

    fn dialect(&self) -> Dialect;

    /// A transaction for writes. Backends serialize writers as needed for correctness of the
    /// event sequence (see `SqlIndex` docs).
    async fn begin_write(&self) -> Result<Self::Tx>;

    /// A transaction for consistent reads.
    async fn begin_read(&self) -> Result<Self::Tx>;
}

/// Helpers on top of [`Tx`].
#[async_trait]
pub(crate) trait TxExt: Tx {
    async fn fetch_one_int(&mut self, stmt: &Stmt) -> Result<Option<i64>> {
        let rows = self.fetch(stmt, &[CellKind::Int]).await?;
        Ok(rows
            .into_iter()
            .next()
            .and_then(|r| r.into_iter().next())
            .and_then(|c| c.int()))
    }
}

impl<T: Tx + ?Sized> TxExt for T {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_per_dialect() {
        let mut s = Stmt::new(Dialect::Postgres);
        s.push("a = ")
            .bind("x")
            .push(" AND b IN ")
            .bind_list([1i64, 2]);
        assert_eq!(s.sql, "a = $1 AND b IN ($2, $3)");
        let mut s = Stmt::new(Dialect::Sqlite);
        s.push("a = ")
            .bind("x")
            .push(" AND b IN ")
            .bind_list([1i64, 2]);
        assert_eq!(s.sql, "a = ? AND b IN (?, ?)");
        assert_eq!(s.args, vec![Cell::from("x"), Cell::Int(1), Cell::Int(2)]);
    }

    #[test]
    fn json_arguments_use_binary_json() {
        let mut s = Stmt::new(Dialect::Sqlite);
        s.push("x = ").bind(Cell::Json("{}".into()));
        assert_eq!(s.sql, "x = jsonb(?)");
        let mut s = Stmt::new(Dialect::Postgres);
        s.push("x = ")
            .bind(1i64)
            .push(", y = ")
            .bind(Cell::Json("{}".into()));
        assert_eq!(s.sql, "x = $1, y = $2::jsonb");
    }
}
