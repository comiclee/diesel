//! Types related to managing bind parameters during query construction.

use backend::Backend;
use result::Error::SerializationError;
use result::QueryResult;
use serialize::{IsNull, Output, ToSql};
use sql_types::{HasSqlType, TypeMetadata};

/// A type which manages serializing bind parameters during query construction.
///
/// The only reason you would ever need to interact with this trait is if you
/// are adding support for a new backend to Diesel. Plugins which are extending
/// the query builder will use [`AstPass::push_bind_param`] instead.
///
/// [`AstPass::push_bind_param`]: ../struct.AstPass.html#method.push_bind_param
pub trait BindCollector<DB: Backend> {
    /// Serializes the given bind value, and collects the result.
    fn push_bound_value<T, U>(
        &mut self,
        bind: &U,
        metadata_lookup: &DB::MetadataLookup,
    ) -> QueryResult<()>
    where
        DB: HasSqlType<T>,
        U: ToSql<T, DB>;

    fn push_is_value_none(&mut self, _column_name: String, _sql_type: DB::TypeMetadata, _is_none: bool) {

    }
}

#[derive(Debug)]
/// A bind collector used by backends which transmit bind parameters as an
/// opaque blob of bytes.
///
/// For most backends, this is the concrete implementation of `BindCollector`
/// that should be used.
pub struct RawBytesBindCollector<DB: Backend + TypeMetadata> {
    /// The metadata associated with each bind parameter.
    ///
    /// This vec is guaranteed to be the same length as `binds`.
    pub metadata: Vec<DB::TypeMetadata>,
    /// The serialized bytes for each bind parameter.
    ///
    /// This vec is guaranteed to be the same length as `metadata`.
    pub binds: Vec<Option<Vec<u8>>>,

    pub value_is_none: Vec<(String, DB::TypeMetadata, bool)>,
}

#[cfg_attr(feature = "cargo-clippy", allow(new_without_default_derive))]
impl<DB: Backend + TypeMetadata> RawBytesBindCollector<DB> {
    /// Construct an empty `RawBytesBindCollector`
    pub fn new() -> Self {
        RawBytesBindCollector {
            metadata: Vec::new(),
            binds: Vec::new(),
            value_is_none: Vec::new(),
        }
    }
}

impl<DB: Backend + TypeMetadata> BindCollector<DB> for RawBytesBindCollector<DB> {
    fn push_bound_value<T, U>(
        &mut self,
        bind: &U,
        metadata_lookup: &DB::MetadataLookup,
    ) -> QueryResult<()>
    where
        DB: HasSqlType<T>,
        U: ToSql<T, DB>,
    {
        let mut to_sql_output = Output::new(Vec::new(), metadata_lookup);
        let is_null = bind.to_sql(&mut to_sql_output).map_err(SerializationError)?;
        let bytes = to_sql_output.into_inner();
        let metadata = <DB as HasSqlType<T>>::metadata(metadata_lookup);
        match is_null {
            IsNull::No => self.binds.push(Some(bytes)),
            IsNull::Yes => self.binds.push(None),
        }
        self.metadata.push(metadata);
        Ok(())
    }

    fn push_is_value_none(&mut self, column_name: String, sql_type: DB::TypeMetadata, is_none: bool) {
        self.value_is_none.push((column_name, sql_type, is_none));
    }
}
