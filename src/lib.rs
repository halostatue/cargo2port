use std::collections::BTreeSet;
use std::fmt;
use std::io::{self, Cursor, Read};
use std::str::FromStr;

use cargo_lock::{self, Lockfile, Package};
use flate2::read::GzDecoder;
use tar::Archive;

/// Result type with the `cargo2port` crate's [`Error`] type.
type Result<T> = std::result::Result<T, Error>;

/// Error type.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// Errors from cargo_lock
    CargoLock(cargo_lock::Error),

    /// Errors related to crate download
    Download(reqwest::Error),

    /// Errors related to crate lockfile extraction
    Tar(io::ErrorKind),

    /// Missing lockfile in tarball
    MissingLockfile,

    /// Could not parse the crate specification
    Spec(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::CargoLock(error) => error.fmt(f),
            Error::Download(error) => error.fmt(f),
            Error::Tar(error) => error.fmt(f),
            Error::Spec(err) => write!(f, "invalid crate specifier: {}", err),
            Error::MissingLockfile => write!(f, "crate missing Cargo.lock file"),
        }
    }
}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Self {
        Error::Tar(err.kind())
    }
}

impl From<cargo_lock::Error> for Error {
    fn from(err: cargo_lock::Error) -> Self {
        Error::CargoLock(err)
    }
}

impl From<reqwest::Error> for Error {
    fn from(err: reqwest::Error) -> Self {
        Error::Download(err)
    }
}

impl std::error::Error for Error {}

// The amount of space that will always be put between the name and version when in
// AlignmentMode::Justify, in addition to any other amount calculated.
const JUSTIFIED_BASE_WIDTH: usize = 5;

#[derive(PartialEq)]
pub enum AlignmentMode {
    Normal,
    Maxlen,
    Multiline,
    Justify,
}

/// Load a Cargo.lock file from the filename provided.
/// This is a thin wrapper around cargo_lockfile::Lockfile::load.
pub fn lockfile_from_path(filename: &str) -> Result<Lockfile> {
    Ok(Lockfile::load(filename)?)
}

/// Parse a Cargo.lock file from the contents provided.
/// This is a thin wrapper around cargo_lockfile::Lockfile::from_str.
pub fn lockfile_from_str(contents: &str) -> Result<Lockfile> {
    Ok(Lockfile::from_str(contents)?)
}

/// Load Cargo.lock data from stdin and parse it from the resulting string.
pub fn lockfile_from_stdin() -> Result<Lockfile> {
    let mut stdin = io::stdin().lock();
    let mut contents = String::new();
    stdin.read_to_string(&mut contents)?;
    lockfile_from_str(&contents)
}

fn lockfile_from_crates_io(crate_spec: &str) -> Result<Lockfile> {
    let parts: Vec<&str> = crate_spec.split('@').collect();

    if parts.len() >= 2 {
        let pkg = download_crate(parts[0], parts[1])?;
        let cargo_lock = extract_cargo_lock_from_pkg(&pkg)?;

        return lockfile_from_str(&cargo_lock);
    };

    Err(Error::Spec(crate_spec.to_string()))
}

/// Resolve packages from a vector of Lockfile entries to a de-duplicated sorted vector of
/// Packages.
///
/// Packages without a checksum are omitted (this usually happens for the package with the
/// Cargo.lock file or files being processed).
pub fn resolve_lockfile_packages(lockfiles: &Vec<Lockfile>) -> Result<Vec<Package>> {
    let mut packageset: BTreeSet<&Package> = BTreeSet::new();

    for lockfile in lockfiles {
        for package in &lockfile.packages {
            if package.checksum.is_none() {
                continue;
            }

            packageset.insert(package);
        }
    }

    let mut packages = Vec::new();

    for package in packageset {
        packages.push(package.clone())
    }

    packages.sort();

    Ok(packages)
}

/// Return the portfile `cargo.crates` block given a vector of packages and AlignmentMode.
/// It is assumed that the package vector is already sorted and deduplicated.
pub fn format_cargo_crates(packages: Vec<Package>, mode: AlignmentMode) -> String {
    let mut output = String::new();

    let mut name_min_width = 0;
    let mut version_min_width = 0;
    let mut package_max_width = 0;

    if mode == AlignmentMode::Maxlen {
        for package in &packages {
            let name_len = package.name.as_str().len();
            if name_len > name_min_width {
                name_min_width = name_len;
            }

            let version_len = package.version.to_string().len();
            if version_len > version_min_width {
                version_min_width = version_len;
            }
        }
    } else if mode == AlignmentMode::Justify {
        for package in &packages {
            let len = package.name.as_str().len() + package.version.to_string().len();
            if len > package_max_width {
                package_max_width = len;
            }
        }
    }

    output.push_str("cargo.crates");

    for package in packages {
        if let Some(checksum) = &package.checksum {
            output.push_str(" \\\n");

            let line = match mode {
                AlignmentMode::Maxlen => format!(
                    "    {:<name_width$}  {:<version_width$}  {}",
                    package.name,
                    package.version,
                    checksum,
                    name_width = name_min_width,
                    version_width = version_min_width
                ),
                AlignmentMode::Multiline => format!(
                    "    {} \\\n    {} \\\n    {}",
                    package.name, package.version, checksum
                ),
                AlignmentMode::Normal => format!(
                    "    {:<name_width$}  {:>version_width$}  {}",
                    package.name,
                    package.version,
                    checksum,
                    name_width = 28,
                    version_width = 8
                ),
                AlignmentMode::Justify => {
                    let version_len = package.version.to_string().len();
                    let space_width = package_max_width - package.name.as_str().len() - version_len
                        + JUSTIFIED_BASE_WIDTH;

                    format!(
                        "    {}{:space_width$}{:>version_width$}  {}",
                        package.name,
                        " ",
                        package.version,
                        checksum,
                        space_width = space_width,
                        version_width = version_len,
                    )
                }
            };

            output.push_str(&line);
        }
    }

    output
}

/// Read the Cargo.lock files from the vector of filenames and resolve into
/// a de-duplicated, sorted package vector.
///
/// This is a cargo2port internal function.
pub fn read_packages_from_lockfiles(files: &Vec<String>) -> Result<Vec<Package>> {
    let mut lockfiles: Vec<Lockfile> = vec![];

    for name in files {
        let lockfile = if name == "-" {
            lockfile_from_stdin()?
        } else if let Some(crate_spec) = name.strip_prefix("crate:") {
            lockfile_from_crates_io(crate_spec)?
        } else {
            lockfile_from_path(name)?
        };

        lockfiles.push(lockfile);
    }

    resolve_lockfile_packages(&lockfiles)
}

fn extract_cargo_lock_from_pkg(pkg: &[u8]) -> Result<String> {
    let gzip = GzDecoder::new(Cursor::new(pkg));
    let mut archive = Archive::new(gzip);

    for entry_result in archive.entries()? {
        let mut entry = entry_result?;
        let path = entry.path()?.to_path_buf();

        if path.ends_with("Cargo.lock") {
            let mut contents = String::new();
            entry.read_to_string(&mut contents)?;
            return Ok(contents);
        }
    }

    Err(Error::MissingLockfile)
}

fn download_crate(name: &str, version: &str) -> Result<Vec<u8>> {
    let url = format!(
        "https://crates.io/api/v1/crates/{}/{}/download",
        name, version
    );
    let response = reqwest::blocking::get(url)?.bytes()?;
    Ok(response.to_vec())
}
