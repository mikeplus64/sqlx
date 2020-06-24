use std::borrow::Cow;
use std::env;

use proc_macro2::{Span, TokenStream};
use syn::Type;
use url::Url;

pub use input::QueryMacroInput;
use quote::{format_ident, quote};
use sqlx_core::connection::Connect;
use sqlx_core::database::Database;
use sqlx_core::describe::Describe;

use crate::database::DatabaseExt;
use crate::query::data::QueryData;
use crate::query::input::RecordType;
use crate::runtime::block_on;

mod args;
mod data;
mod input;
mod output;

pub fn expand_input(input: QueryMacroInput) -> crate::Result<TokenStream> {
    let manifest_dir =
        env::var("CARGO_MANIFEST_DIR").map_err(|_| "`CARGO_MANIFEST_DIR` must be set")?;

    // If a .env file exists at CARGO_MANIFEST_DIR, load environment variables from this,
    // otherwise fallback to default dotenv behaviour.
    let env_path = std::path::Path::new(&manifest_dir).join(".env");
    if env_path.exists() {
        dotenv::from_path(&env_path)
            .map_err(|e| format!("failed to load environment from {:?}, {}", env_path, e))?
    }

    // if `dotenv` wasn't initialized by the above we make sure to do it here
    match dotenv::var("DATABASE_URL").ok() {
        Some(db_url) => expand_from_db(input, &db_url),

        #[cfg(feature = "offline")]
        None => {
            let data_file_path = std::path::Path::new(&manifest_dir).join("sqlx-data.json");

            if data_file_path.exists() {
                expand_from_file(input, data_file_path)
            } else {
                Err(
                    "`DATABASE_URL` must be set, or `cargo sqlx prepare` must have been run \
                     and sqlx-data.json must exist, to use query macros"
                        .into(),
                )
            }
        }

        #[cfg(not(feature = "offline"))]
        None => Err("`DATABASE_URL` must be set to use query macros".into()),
    }
}

#[allow(unused_variables)]
fn expand_from_db(input: QueryMacroInput, db_url: &str) -> crate::Result<TokenStream> {
    // FIXME: Introduce [sqlx::any::AnyConnection] and [sqlx::any::AnyDatabase] to support
    //        runtime determinism here

    let db_url = Url::parse(db_url)?;
    match db_url.scheme() {
        #[cfg(feature = "postgres")]
        "postgres" | "postgresql" => {
            let data = block_on(async {
                let mut conn = sqlx_core::postgres::PgConnection::connect(db_url.as_str()).await?;
                QueryData::from_db(&mut conn, &input.src).await
            })?;

            expand_with_data(input, data)
        },

        #[cfg(not(feature = "postgres"))]
        "postgres" | "postgresql" => Err(format!("database URL has the scheme of a PostgreSQL database but the `postgres` feature is not enabled").into()),

        #[cfg(feature = "mssql")]
        "mssql" | "sqlserver" => {
            let data = block_on(async {
                let mut conn = sqlx_core::mssql::MssqlConnection::connect(db_url.as_str()).await?;
                QueryData::from_db(&mut conn, &input.src).await
            })?;

            expand_with_data(input, data)
        },

        #[cfg(not(feature = "mssql"))]
        "mssql" | "sqlserver" => Err(format!("database URL has the scheme of a MSSQL database but the `mssql` feature is not enabled").into()),

        #[cfg(feature = "mysql")]
        "mysql" | "mariadb" => {
            let data = block_on(async {
                let mut conn = sqlx_core::mysql::MySqlConnection::connect(db_url.as_str()).await?;
                QueryData::from_db(&mut conn, &input.src).await
            })?;

            expand_with_data(input, data)
        },

        #[cfg(not(feature = "mysql"))]
        "mysql" | "mariadb" => Err(format!("database URL has the scheme of a MySQL/MariaDB database but the `mysql` feature is not enabled").into()),

        #[cfg(feature = "sqlite")]
        "sqlite" => {
            let data = block_on(async {
                let mut conn = sqlx_core::sqlite::SqliteConnection::connect(db_url.as_str()).await?;
                QueryData::from_db(&mut conn, &input.src).await
            })?;

            expand_with_data(input, data)
        },

        #[cfg(not(feature = "sqlite"))]
        "sqlite" => Err(format!("database URL has the scheme of a SQLite database but the `sqlite` feature is not enabled").into()),

        scheme => Err(format!("unknown database URL scheme {:?}", scheme).into())
    }
}

#[cfg(feature = "offline")]
pub fn expand_from_file(
    input: QueryMacroInput,
    file: std::path::PathBuf,
) -> crate::Result<TokenStream> {
    use data::offline::DynQueryData;

    let query_data = DynQueryData::from_data_file(file, &input.src)?;
    assert!(!query_data.db_name.is_empty());

    match &*query_data.db_name {
        #[cfg(feature = "postgres")]
        sqlx_core::postgres::Postgres::NAME => expand_with_data(
            input,
            QueryData::<sqlx_core::postgres::Postgres>::from_dyn_data(query_data)?,
        ),
        #[cfg(feature = "mysql")]
        sqlx_core::mysql::MySql::NAME => expand_with_data(
            input,
            QueryData::<sqlx_core::mysql::MySql>::from_dyn_data(query_data)?,
        ),
        #[cfg(feature = "sqlite")]
        sqlx_core::sqlite::Sqlite::NAME => expand_with_data(
            input,
            QueryData::<sqlx_core::sqlite::Sqlite>::from_dyn_data(query_data)?,
        ),
        _ => Err(format!(
            "found query data for {} but the feature for that database was not enabled",
            query_data.db_name
        )
        .into()),
    }
}

// marker trait for `Describe` that lets us conditionally require it to be `Serialize + Deserialize`
#[cfg(feature = "offline")]
trait DescribeExt: serde::Serialize + serde::de::DeserializeOwned {}

#[cfg(feature = "offline")]
impl<DB: Database> DescribeExt for Describe<DB> where
    Describe<DB>: serde::Serialize + serde::de::DeserializeOwned
{
}

#[cfg(not(feature = "offline"))]
trait DescribeExt {}

#[cfg(not(feature = "offline"))]
impl<DB: Database> DescribeExt for Describe<DB> {}

fn expand_with_data<DB: DatabaseExt>(
    input: QueryMacroInput,
    data: QueryData<DB>,
) -> crate::Result<TokenStream>
where
    Describe<DB>: DescribeExt,
{
    // validate at the minimum that our args match the query's input parameters
    if input.arg_names.len() != data.describe.params.len() {
        return Err(syn::Error::new(
            Span::call_site(),
            format!(
                "expected {} parameters, got {}",
                data.describe.params.len(),
                input.arg_names.len()
            ),
        )
        .into());
    }

    let args_tokens = args::quote_args(&input, &data.describe)?;

    let query_args = format_ident!("query_args");

    let output = if data.describe.columns.is_empty() {
        if let RecordType::Generated = input.record_type {
            let db_path = DB::db_path();
            let sql = &input.src;

            quote! {
                sqlx::query_with::<#db_path, _>(#sql, #query_args)
            }
        } else {
            return Err("query produces no columns but this macro variant expects columns".into());
        }
    } else {
        match input.record_type {
            RecordType::Generated => {
                let columns = output::columns_to_rust::<DB>(&data.describe)?;

                let record_name: Type = syn::parse_str("Record").unwrap();

                for rust_col in &columns {
                    if rust_col.type_.is_none() {
                        return Err(
                            "columns may not have wildcard overrides in `query!()` or `query_unchecked!()"
                                .into(),
                        );
                    }
                }

                let record_fields = columns.iter().map(
                    |&output::RustColumn {
                         ref ident,
                         ref type_,
                     }| quote!(#ident: #type_,),
                );

                let query_as =
                    output::quote_query_as::<DB>(&input, &record_name, &query_args, &columns);

                quote! {
                    #[derive(Debug)]
                    struct #record_name {
                        #(#record_fields)*
                    }

                    #query_as
                }
            }
            RecordType::Given(ref out_ty) => {
                let columns = output::columns_to_rust::<DB>(&data.describe)?;
                output::quote_query_as::<DB>(&input, out_ty, &query_args, &columns)
            }
            RecordType::Scalar => {
                if data.describe.columns.len() != 1 {
                    return Err(format!(
                        "expected exactly one column from query, got {}",
                        data.describe.columns.len()
                    )
                    .into());
                }

                let ty = output::get_scalar_type(1, &data.describe.columns[0]);
                let db_path = DB::db_path();
                let query = &input.src;

                quote! {
                    sqlx::query_scalar_with::<#db_path, #ty, _>(#query, #query_args)
                }
            }
        }
    };

    let arg_names = &input.arg_names;

    let ret_tokens = quote! {
        macro_rules! macro_result {
            (#($#arg_names:expr),*) => {{
                use sqlx::Arguments as _;

                #args_tokens

                #output
            }}
        }
    };

    #[cfg(feature = "offline")]
    {
        let mut save_dir = std::path::PathBuf::from(
            env::var("CARGO_TARGET_DIR").unwrap_or_else(|_| "target/".into()),
        );

        save_dir.push("sqlx");

        std::fs::create_dir_all(&save_dir)?;
        data.save_in(save_dir, input.src_span)?;
    }

    Ok(ret_tokens)
}
