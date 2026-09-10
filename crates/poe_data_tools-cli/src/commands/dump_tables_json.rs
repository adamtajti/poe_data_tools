use std::{
    fs::{self, File},
    io::BufWriter,
    path::Path,
};

use anyhow::{Context, Result, ensure};
use glob::{MatchOptions, Pattern};
use poe_data_tools::{
    Patch,
    dat::{
        json::{resolve_enums, resolve_table},
        schema::{SchemaCollection, fetch_schema, load_schema},
    },
    fs::{FS, FileSystem},
};

use crate::VERBOSE;

fn dump_table(
    fs: &mut FS,
    version: &Patch,
    schemas: &SchemaCollection,
    output_folder: &Path,
    resolved: &mut poe_data_tools::dat::json::ResolvedKeys,
    filename: &str,
) -> Result<()> {
    let path = Path::new(&filename);
    let table_name = path.file_stem().unwrap().to_str().unwrap().to_lowercase();

    let json = resolve_table(fs, schemas, version, resolved, &table_name)
        .map_err(|e| anyhow::anyhow!("Failed to resolve table: {e}"))?;

    // Save out
    let output_path = output_folder.join(path).with_added_extension("json");
    fs::create_dir_all(output_path.parent().unwrap()).context("Failed to create output folder")?;

    let mut out =
        BufWriter::new(File::create(&output_path).context("Failed to create output file")?);
    serde_json::to_writer_pretty(&mut out, &json).context("Failed to serialize json")?;

    Ok(())
}

pub fn dump_tables(
    fs: &mut FS,
    patterns: &[Pattern],
    cache_dir: &Path,
    output_folder: &Path,
    version: &Patch,
    schema: Option<impl AsRef<Path>>,
) -> Result<()> {
    for pattern in patterns {
        ensure!(
            pattern.as_str().ends_with(".datc64"),
            "Only .datc64 table export is supported."
        );
    }

    // Load schema
    let schemas = if let Some(path) = schema {
        load_schema(path.as_ref()).context("Failed to load schema file")?
    } else {
        fetch_schema(cache_dir).context("Failed to fetch schema file")?
    }
    .filter_version(version);

    // Resolve enums first as they have no dependencies
    let mut resolved = resolve_enums(&schemas);

    let schema_names = schemas
        .tables
        .iter()
        .map(|t| t.name.to_lowercase())
        .collect::<Vec<_>>();

    // Filter list of files we're going to extract
    let filenames = fs
        .list()
        // Filter on glob
        .filter(|filename| {
            patterns.iter().any(|pattern| {
                pattern.matches_with(
                    filename,
                    MatchOptions {
                        require_literal_separator: true,
                        ..Default::default()
                    },
                )
            })
        })
        // Skip files we can't process
        .filter(|filename| {
            let path = Path::new(filename);
            let table_name = path.file_stem().unwrap().to_str().unwrap().to_lowercase();

            let keep = schema_names.contains(&table_name);

            if !keep {
                log::warn!("Skipping {:?}, schema not found", path);
            }

            keep
        })
        .collect::<Vec<_>>();

    filenames.into_iter().for_each(|filename| {
        let result = dump_table(
            fs,
            version,
            &schemas,
            output_folder,
            &mut resolved,
            &filename,
        );

        if let Err(e) = result {
            let error_message = if *VERBOSE.get().unwrap() {
                format!("{e:?}")
            } else {
                format!("{e}")
            };
            log::error!("Failed to extract file {filename:?}: {error_message}");
        } else {
            log::info!("Extracted file: {}", filename);
        }
    });

    Ok(())
}
