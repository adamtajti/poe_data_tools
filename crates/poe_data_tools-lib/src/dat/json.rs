//! Schema-driven, reference-resolved JSON decoding of dat tables.
//!
//! While [`crate::dat::table`] decodes tables into Arrow record batches with
//! references left as raw row indices, this module decodes tables into
//! `serde_json::Value` rows with `row`/`foreignrow`/`enumrow` references
//! resolved against the referenced tables' key columns.
//!
//! Reference resolution is transitive: a table whose key columns themselves
//! reference other tables is resolved depth-first, so the output is
//! self-contained JSON per table.

use std::collections::HashMap;

use winnow::Parser;

use crate::{
    Patch,
    dat::{
        parser::create_parser,
        schema::{Enumeration, SchemaCollection},
    },
    file_parsers::{FileParser, dat::DatParser},
    fs::{FS, FileSystem},
};

/// Resolved key values for every table decoded so far, keyed by lowercase
/// table name. Values are `None` for tables whose schema defines no key
/// columns.
pub type ResolvedKeys = HashMap<String, Option<Vec<serde_json::Value>>>;

/// Errors produced while resolving a table to JSON.
#[derive(Debug)]
pub enum JsonTableError {
    /// No schema exists for the requested table.
    SchemaNotFound(String),
    /// The dat file is missing from the game file system.
    FileNotFound(String),
    /// Failed to read from the file system.
    Read(String),
    /// Failed to parse the dat container.
    Parse(String),
}

impl std::fmt::Display for JsonTableError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JsonTableError::SchemaNotFound(table) => {
                write!(f, "Failed to find schema for table {table:?}")
            }
            JsonTableError::FileNotFound(path) => write!(f, "File not found: {path}"),
            JsonTableError::Read(e) => write!(f, "Failed to read file contents: {e}"),
            JsonTableError::Parse(e) => write!(f, "Failed to parse dat file: {e}"),
        }
    }
}

impl std::error::Error for JsonTableError {}

fn resolve_enum(schema: &Enumeration) -> Vec<serde_json::Value> {
    std::iter::repeat_n(serde_json::Value::Null, schema.indexing)
        .chain(schema.enumerators.iter().map(|e| match e {
            Some(value) => serde_json::Value::String(value.clone()),
            None => serde_json::Value::Null,
        }))
        .collect()
}

/// Resolve all enum tables in the schema collection into key values.
///
/// Enums have no dependencies and should be resolved first.
pub fn resolve_enums(schemas: &SchemaCollection) -> ResolvedKeys {
    let mut resolved = HashMap::new();
    schemas.enumerations.iter().for_each(|e| {
        let e_resolved = resolve_enum(e);
        resolved.insert(e.name.to_lowercase(), Some(e_resolved));
    });
    resolved
}

/// Dat table path for the given game version
pub fn table_filename(version: &Patch, table_name: &str) -> String {
    match version.major() {
        1 => format!("data/{}.datc64", table_name),
        2 => format!("data/balance/{}.datc64", table_name),
        _ => unreachable!("Invalid major version"),
    }
}

fn schema_for<'a>(
    schemas: &'a SchemaCollection,
    table_name: &str,
) -> Result<&'a crate::dat::schema::DatTableSchema, JsonTableError> {
    schemas
        .tables
        .iter()
        .find(|s| s.name.eq_ignore_ascii_case(table_name))
        .ok_or_else(|| JsonTableError::SchemaNotFound(table_name.to_owned()))
}

/// Depth-first resolution of table keys
fn resolve_keys(
    fs: &mut FS,
    schemas: &SchemaCollection,
    version: &Patch,
    keys: &mut ResolvedKeys,
    table_name: &str,
    resolve_keys_stack: &mut Vec<String>,
) -> Result<(), JsonTableError> {
    let schema = schema_for(schemas, table_name)?;

    let mut keys_columns = schema.primary_keys().collect::<Vec<_>>();
    if keys_columns.is_empty()
        && let Some(col_name) = schema.column_names().next()
    {
        // Fall back to first column as key for tables without any key
        log::debug!(
            "No keys for table {:?}, falling back to first column: {:?}",
            schema.name,
            col_name
        );
        keys_columns.push(col_name);
    }

    let ref_keys = schema
        .enumerate()
        // Select key columns that are references
        .filter_map(|(name, c)| {
            keys_columns
                .contains(&name)
                .then_some(c.get_ref())
                .flatten()
        })
        .map(|s| s.to_lowercase())
        // And only ones that have not yet had their keys resolved
        .filter(|table_name| !keys.contains_key(table_name))
        .collect::<Vec<_>>();

    if !ref_keys.is_empty() {
        // This table is not yet ready to be resolved. Push children to stack and go again
        log::debug!("Table not yet resolvable: {table_name}");
        resolve_keys_stack.push(table_name.to_owned());
        resolve_keys_stack.extend(ref_keys);
        return Ok(());
    }

    // All reference keys have been resolved, so this table can be resolved
    let parsed = load_rows(fs, schemas, version, keys, table_name)?;

    // Extract keys from the parsed table
    let key_values = (!keys_columns.is_empty()).then(|| {
        // Try get the corresponding values for them
        parsed
            .iter()
            .map(|row| {
                let keys = keys_columns
                    .iter()
                    .map(|k| row.get(k).unwrap_or(&serde_json::Value::Null).clone())
                    .collect::<Vec<_>>();

                // If there's multiple primary keys, use a list
                match keys.len() {
                    0 => unreachable!(),
                    1 => keys[0].clone(),
                    _ => serde_json::Value::Array(keys),
                }
            })
            .collect::<Vec<_>>()
    });

    log::debug!("Resolved keys for table: {table_name}");
    if keys.insert(table_name.to_owned(), key_values).is_some() {
        unreachable!("Keys already present for {:?}", table_name);
    }

    Ok(())
}

/// Read, container-parse and schema-decode a table's rows into JSON values.
///
/// Any tables referenced by this table's key columns must already be
/// present in `keys`; use [`resolve_table`] for the full transitive
/// resolution.
fn load_rows(
    fs: &mut FS,
    schemas: &SchemaCollection,
    version: &Patch,
    keys: &ResolvedKeys,
    table_name: &str,
) -> Result<Vec<serde_json::Value>, JsonTableError> {
    let schema = schema_for(schemas, table_name)?;

    let filename = table_filename(version, table_name);
    let bytes = fs.read(&filename).map_err(|e| {
        log::error!("Failed to read file contents: {filename:?}: {e}");
        JsonTableError::FileNotFound(filename.clone())
    })?;
    let contents = DatParser
        .parse(&bytes)
        .map_err(|e| JsonTableError::Parse(format!("{e}")))?;

    // FIXME: Figure out a way to give variable section to the parser without leaking it to a
    //          'static lifetime
    let variable_section: &'static [u8] =
        Box::leak(contents.variable_data.clone().into_boxed_slice());
    let mut parser = create_parser(keys, variable_section, schema);

    Ok(contents
        .rows
        .iter()
        .map(|row| parser.parse(row).unwrap_or(serde_json::Value::Null))
        .collect())
}

/// Resolve a single table into per-row JSON values with all references
/// resolved.
///
/// Reference chains are followed depth-first: every referenced table is
/// itself loaded and resolved until all key dependencies are satisfied.
/// Resolved key tables are retained in `keys`, so decoding several tables
/// through one [`ResolvedKeys`] avoids re-reading shared dependencies.
///
/// # Errors
///
/// Returns [`JsonTableError`] if the schema is missing, a dat file cannot
/// be read or parsed, or a transitive dependency fails to resolve.
pub fn resolve_table(
    fs: &mut FS,
    schemas: &SchemaCollection,
    version: &Patch,
    keys: &mut ResolvedKeys,
    table_name: &str,
) -> Result<Vec<serde_json::Value>, JsonTableError> {
    let schema = schema_for(schemas, table_name)?;

    // Start off with all unresolved children in the stack
    let mut resolve_keys_stack = schema
        .references()
        .map(|r| r.to_lowercase())
        .filter(|r| !keys.contains_key(r))
        .collect::<Vec<_>>();

    // Recursively resolve all keys
    while let Some(child) = resolve_keys_stack.pop() {
        // Child may have already been resolved, so check again
        if keys.contains_key(&child) {
            continue;
        }

        resolve_keys(fs, schemas, version, keys, &child, &mut resolve_keys_stack)?;
    }

    // All keys for reference tables have been resolved, so we can now fully resolve this table
    load_rows(fs, schemas, version, keys, table_name)
}

/// Convenience wrapper: resolve a table using freshly-resolved enums.
///
/// Suitable for one-off table loads. When loading many tables, create one
/// [`ResolvedKeys`] via [`resolve_enums`] and pass it through
/// [`resolve_table`] calls instead.
///
/// # Errors
///
/// Returns [`JsonTableError`] under the same conditions as
/// [`resolve_table`].
pub fn load_table_json(
    fs: &mut FS,
    schemas: &SchemaCollection,
    version: &Patch,
    table_name: &str,
) -> Result<Vec<serde_json::Value>, JsonTableError> {
    let mut keys = resolve_enums(schemas);
    resolve_table(fs, schemas, version, &mut keys, table_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Patch;

    #[test]
    fn table_filenames() {
        assert_eq!(table_filename(&Patch::One, "Stats"), "data/Stats.datc64");
        assert_eq!(
            table_filename(&Patch::Two, "Stats"),
            "data/balance/Stats.datc64"
        );
        assert_eq!(
            table_filename(&Patch::Specific("4.10.2.5".into()), "Stats"),
            "data/balance/Stats.datc64"
        );
    }
}
