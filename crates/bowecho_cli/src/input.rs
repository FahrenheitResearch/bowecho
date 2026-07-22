use std::ffi::OsString;
use std::fs;
use std::path::{Component, Path, PathBuf};

use glob::{MatchOptions, glob_with};
use serde::{Deserialize, Serialize};

use crate::CliError;
use crate::fs::{MAX_DISCOVERED_INPUTS, expand_input_files, is_netcdf_candidate};

const MAX_GLOB_LENGTH: usize = 4096;

/// One local input expression. Globs are parsed by Rust and never passed to a
/// shell, so metacharacters cannot execute commands.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InputSpec {
    Explicit { path: PathBuf },
    Glob { pattern: String },
}

/// Ordered user input. Multiple explicit paths are the safe file-list form;
/// callers do not need to concatenate a shell command.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InputSet {
    pub specs: Vec<InputSpec>,
}

impl InputSet {
    pub fn parse(values: impl IntoIterator<Item = OsString>) -> Result<Self, CliError> {
        let mut specs = Vec::new();
        for value in values {
            if value.is_empty() {
                return Err(CliError::usage(
                    "WRF input path cannot be empty",
                    crate::wrf_help_text(),
                ));
            }
            if let Some(text) = value.to_str()
                && has_glob_syntax(text)
            {
                validate_glob(text)?;
                specs.push(InputSpec::Glob {
                    pattern: text.to_owned(),
                });
            } else {
                specs.push(InputSpec::Explicit {
                    path: PathBuf::from(value),
                });
            }
        }
        if specs.is_empty() {
            return Err(CliError::usage(
                "at least one WRF file, directory, or glob is required",
                crate::wrf_help_text(),
            ));
        }
        Ok(Self { specs })
    }

    /// Expand every expression without following symlinks, then sort and
    /// deduplicate paths for deterministic manifests and work scheduling.
    pub fn expand(&self) -> Result<Vec<PathBuf>, CliError> {
        let mut files = Vec::new();
        for spec in &self.specs {
            match spec {
                InputSpec::Explicit { path } => files.extend(expand_input_files(path)?),
                InputSpec::Glob { pattern } => expand_glob(pattern, &mut files)?,
            }
            if files.len() > MAX_DISCOVERED_INPUTS {
                return Err(CliError::input(format!(
                    "input expansion exceeds the safe limit of {MAX_DISCOVERED_INPUTS} files"
                )));
            }
        }
        files.sort_by_key(|path| stable_path_key(path));
        files.dedup_by(|left, right| stable_path_key(left) == stable_path_key(right));
        if files.is_empty() {
            return Err(CliError::input("WRF input expansion produced no files"));
        }
        Ok(files)
    }
}

fn has_glob_syntax(value: &str) -> bool {
    value.bytes().any(|byte| matches!(byte, b'*' | b'?' | b'['))
}

fn validate_glob(pattern: &str) -> Result<(), CliError> {
    if pattern.len() > MAX_GLOB_LENGTH {
        return Err(CliError::usage(
            format!("glob exceeds the safe limit of {MAX_GLOB_LENGTH} bytes"),
            crate::wrf_help_text(),
        ));
    }
    if pattern.chars().any(char::is_control) {
        return Err(CliError::usage(
            "glob contains a control character",
            crate::wrf_help_text(),
        ));
    }
    glob::Pattern::new(pattern).map_err(|error| {
        CliError::usage(
            format!("invalid WRF input glob '{pattern}': {error}"),
            crate::wrf_help_text(),
        )
    })?;
    let path = Path::new(pattern);
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            CliError::usage(
                "WRF input glob must include a filename pattern",
                crate::wrf_help_text(),
            )
        })?;
    let parent = path.parent().unwrap_or_else(|| Path::new(""));
    if !has_glob_syntax(file_name)
        || has_glob_syntax(&parent.to_string_lossy())
        || pattern.contains("**")
        || parent
            .components()
            .any(|component| component == Component::ParentDir)
    {
        return Err(CliError::usage(
            "WRF globs may pattern-match one directory's filename only; pass a directory for recursive discovery",
            crate::wrf_help_text(),
        ));
    }
    Ok(())
}

fn expand_glob(pattern: &str, files: &mut Vec<PathBuf>) -> Result<(), CliError> {
    let parent = Path::new(pattern)
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    reject_symlink_path(parent)?;
    let options = MatchOptions {
        case_sensitive: !cfg!(windows),
        require_literal_separator: true,
        require_literal_leading_dot: true,
    };
    let entries = glob_with(pattern, options).map_err(|error| {
        CliError::input(format!("cannot expand WRF input glob '{pattern}': {error}"))
    })?;
    let mut matched = false;
    for entry in entries {
        let path = entry.map_err(|error| {
            CliError::input(format!(
                "error expanding WRF input glob '{pattern}': {error}"
            ))
        })?;
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            CliError::input(format!(
                "cannot stat glob match {}: {error}",
                path.display()
            ))
        })?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() || (metadata.is_file() && is_netcdf_candidate(&path)) {
            files.extend(expand_input_files(&path)?);
            matched = true;
        }
        if files.len() > MAX_DISCOVERED_INPUTS {
            return Err(CliError::input(format!(
                "glob expansion exceeds the safe limit of {MAX_DISCOVERED_INPUTS} files"
            )));
        }
    }
    if !matched {
        return Err(CliError::input(format!(
            "WRF input glob matched no NetCDF/HDF5 files: {pattern}"
        )));
    }
    Ok(())
}

fn reject_symlink_path(path: &Path) -> Result<(), CliError> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        if !matches!(component, Component::Normal(_)) {
            continue;
        }
        let metadata = fs::symlink_metadata(&current).map_err(|error| {
            CliError::input(format!(
                "cannot inspect glob directory component {}: {error}",
                current.display()
            ))
        })?;
        if metadata.file_type().is_symlink() {
            return Err(CliError::input(format!(
                "refusing glob through symlink directory {}",
                current.display()
            )));
        }
        if !metadata.is_dir() {
            return Err(CliError::input(format!(
                "glob parent component is not a directory: {}",
                current.display()
            )));
        }
    }
    Ok(())
}

fn stable_path_key(path: &Path) -> String {
    let key = path.to_string_lossy().replace('\\', "/");
    if cfg!(windows) {
        key.to_ascii_lowercase()
    } else {
        key
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn represents_one_file_file_list_directory_and_glob_without_shell() {
        let one = InputSet::parse([OsString::from("wrfout_d01")]).unwrap();
        assert!(matches!(one.specs[0], InputSpec::Explicit { .. }));

        let list = InputSet::parse([OsString::from("a.nc"), OsString::from("b.nc")]).unwrap();
        assert_eq!(list.specs.len(), 2);

        let glob = InputSet::parse([OsString::from("run/wrfout_d0*")]).unwrap();
        assert!(matches!(glob.specs[0], InputSpec::Glob { .. }));
    }

    #[test]
    fn glob_expansion_is_sorted_deduplicated_and_filters_non_netcdf() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("wrfout_d02"), b"CDF\x01x").unwrap();
        fs::write(root.path().join("wrfout_d01"), b"CDF\x01x").unwrap();
        fs::write(root.path().join("wrfout_preview.png"), b"png").unwrap();
        let pattern = format!(
            "{}/wrfout*",
            root.path().to_string_lossy().replace('\\', "/")
        );
        let set = InputSet::parse([
            OsString::from(pattern),
            root.path().join("wrfout_d01").into_os_string(),
        ])
        .unwrap();
        let expanded = set.expand().unwrap();
        assert_eq!(expanded.len(), 2);
        assert_eq!(expanded[0].file_name().unwrap(), "wrfout_d01");
        assert_eq!(expanded[1].file_name().unwrap(), "wrfout_d02");
    }

    #[test]
    fn invalid_or_unmatched_glob_fails_closed() {
        assert!(InputSet::parse([OsString::from("[")]).is_err());
        assert!(InputSet::parse([OsString::from("../*/wrfout*")]).is_err());
        assert!(InputSet::parse([OsString::from("runs/**/wrfout*")]).is_err());
        let root = tempfile::tempdir().unwrap();
        let pattern = format!(
            "{}/missing*",
            root.path().to_string_lossy().replace('\\', "/")
        );
        let set = InputSet::parse([OsString::from(pattern)]).unwrap();
        assert!(set.expand().is_err());
    }
}
