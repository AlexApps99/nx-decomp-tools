use crate::repo;
use anyhow::{bail, ensure, Context, Result};
use lazy_static::lazy_static;
use rayon::prelude::*;
use rustc_hash::FxHashMap;
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Matching,
    NonMatchingMinor,
    NonMatchingMajor,
    NotDecompiled,
    Wip,
    Library,
}

impl Status {
    pub fn description(&self) -> &'static str {
        match &self {
            Status::Matching => "matching",
            Status::NonMatchingMinor => "non-matching (minor)",
            Status::NonMatchingMajor => "non-matching (major)",
            Status::NotDecompiled => "not decompiled",
            Status::Wip => "WIP",
            Status::Library => "library function",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Info {
    pub addr: u64,
    pub size: u32,
    pub name: String,
    pub status: Status,
}

impl Info {
    pub fn is_decompiled(&self) -> bool {
        !matches!(self.status, Status::NotDecompiled | Status::Library)
    }
}

pub const CSV_HEADER: &[&str] = &["Address", "Quality", "Size", "Name"];
pub const ADDRESS_BASE: u64 = 0x71_0000_0000;

lazy_static! {
    static ref FUNCTIONS_CSV_PATH: PathBuf = {
        let mut path = repo::get_repo_root().expect("Failed to get repo root");
        path.push(repo::CONFIG["functions_csv"].as_str().expect("Failed to read \"functions_csv\" from config TOML"));
        path
    };
}

fn parse_base_16(value: &str) -> Result<u64> {
    if let Some(stripped) = value.strip_prefix("0x") {
        Ok(u64::from_str_radix(stripped, 16)?)
    } else {
        Ok(u64::from_str_radix(value, 16)?)
    }
}

pub fn parse_address(value: &str) -> Result<u64> {
    Ok(parse_base_16(value)? - ADDRESS_BASE)
}

fn parse_function_csv_entry(record: &csv::StringRecord) -> Result<Info> {
    ensure!(record.len() == 4, "invalid record");

    let addr = parse_address(&record[0])?;
    let status_code = record[1].chars().next();
    let size = record[2].parse::<u32>()?;
    let decomp_name = record[3].to_string();

    let status = match status_code {
        Some('m') => Status::NonMatchingMinor,
        Some('M') => Status::NonMatchingMajor,
        Some('O') => Status::Matching,
        Some('U') => Status::NotDecompiled,
        Some('W') => Status::Wip,
        Some('L') => Status::Library,
        Some(code) => bail!("unexpected status code: {}", code),
        None => bail!("missing status code"),
    };

    Ok(Info {
        addr,
        size,
        name: decomp_name,
        status,
    })
}

/// Returns a Vec of all functions that are listed in the specified CSV.
pub fn get_functions_for_path(csv_path: &Path) -> Result<Vec<Info>> {
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(false)
        .quoting(false)
        .from_path(csv_path)?;

    // We build the result array manually without using csv iterators for performance reasons.
    let mut result = Vec::with_capacity(110_000);
    let mut record = csv::StringRecord::new();
    let mut line_number = 1;
    let mut num_names = 0;
    if reader.read_record(&mut record)? {
        // Verify that the CSV has the correct format.
        ensure!(record.len() == 4, "invalid record; expected 4 fields");
        ensure!(record == *CSV_HEADER,
            "wrong CSV format; this program only works with the new function list format (added in commit 1d4c815fbae3)"
        );
        line_number += 1;
    }

    while reader.read_record(&mut record)? {
        let entry = parse_function_csv_entry(&record)
            .with_context(|| format!("failed to parse CSV record at line {}", line_number))?;

        if !entry.name.is_empty() {
            num_names += 1;
        }

        result.push(entry);
        line_number += 1;
    }

    // Check for duplicate names in the CSV.
    let mut known_names = HashSet::with_capacity(num_names);
    let mut duplicates = Vec::new();
    for entry in &result {
        if entry.is_decompiled() && entry.name.is_empty() {
            bail!(
                "function at {:016x} is marked as O/M/m but has an empty name",
                entry.addr | ADDRESS_BASE
            );
        }

        if !entry.name.is_empty() && !known_names.insert(&entry.name) {
            duplicates.push(&entry.name);
        }
    }
    if !duplicates.is_empty() {
        bail!("found duplicates: {:#?}", duplicates);
    }

    Ok(result)
}

pub fn write_functions_to_path(csv_path: &Path, functions: &[Info]) -> Result<()> {
    let mut writer = csv::Writer::from_path(csv_path)?;
    writer.write_record(CSV_HEADER)?;

    for function in functions {
        let addr = format!("0x{:016x}", function.addr | ADDRESS_BASE);
        let status = match function.status {
            Status::Matching => "O",
            Status::NonMatchingMinor => "m",
            Status::NonMatchingMajor => "M",
            Status::NotDecompiled => "U",
            Status::Wip => "W",
            Status::Library => "L",
        }
        .to_string();
        let size = format!("{:06}", function.size);
        let name = function.name.clone();
        writer.write_record(&[addr, status, size, name])?;
    }

    Ok(())
}

/// Returns a Vec of all known functions in the executable.
pub fn get_functions() -> Result<Vec<Info>> {
    get_functions_for_path(FUNCTIONS_CSV_PATH.as_path())
}

pub fn write_functions(functions: &[Info]) -> Result<()> {
    write_functions_to_path(FUNCTIONS_CSV_PATH.as_path(), functions)
}

pub fn make_known_function_map(functions: &[Info]) -> FxHashMap<u64, &Info> {
    let mut known_functions =
        FxHashMap::with_capacity_and_hasher(functions.len(), Default::default());

    for function in functions {
        if function.name.is_empty() {
            continue;
        }
        known_functions.insert(function.addr, function);
    }

    known_functions
}

/// Demangle a C++ symbol.
pub fn demangle_str(name: &str) -> Result<String> {
    if !name.starts_with("_Z") {
        bail!("not an external mangled name");
    }

    let symbol = cpp_demangle::Symbol::new(name)?;
    let options = cpp_demangle::DemangleOptions::new();
    Ok(symbol.demangle(&options)?)
}

pub fn find_function_fuzzy<'a>(functions: &'a [Info], name: &str) -> Option<&'a Info> {
    functions
        .par_iter()
        .find_first(|function| function.name == name)
        .or_else(|| {
            // Comparing the demangled names is more expensive than a simple string comparison,
            // so only do this as a last resort.
            functions.par_iter().find_first(|function| {
                demangle_str(&function.name)
                    .unwrap_or_else(|_| "".to_string())
                    .contains(name)
            })
        })
}
