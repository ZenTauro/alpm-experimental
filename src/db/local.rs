use std::{
    borrow::Cow,
    cell::RefCell,
    collections::HashMap,
    ffi::OsStr,
    fs,
    io::{self, Write},
    path::PathBuf,
    rc::{Rc, Weak},
};

use atoi::atoi;

use crate::{
    db::{Database, DbStatus, DbUsage, SignatureLevel, LOCAL_DB_NAME},
    error::{Error, ErrorKind},
    package::PackageKey,
    Handle,
};

mod package;
pub use self::package::{InstallReason, LocalPackage, ValidationError};

const LOCAL_DB_VERSION_FILE: &str = "ALPM_DB_VERSION";
const LOCAL_DB_CURRENT_VERSION: u64 = 9;

/// The package database of installed packages.
///
/// Clones will be shallow - they will still point to the same internal database.
#[derive(Debug, Clone)]
pub struct LocalDatabase {
    inner: Rc<RefCell<LocalDatabaseInner>>,
}

impl LocalDatabase {
    /// Helper to create a new database
    ///
    /// Path is the root path for databases.
    pub(crate) fn new(inner: Rc<RefCell<LocalDatabaseInner>>) -> LocalDatabase {
        LocalDatabase { inner }
    }
}

impl Database for LocalDatabase {
    type Pkg = Rc<LocalPackage>;

    /// Get the name of this database
    fn name(&self) -> &str {
        LOCAL_DB_NAME
    }

    /// Get the path of the root file or directory for this database.
    fn path(&self) -> PathBuf {
        self.inner.borrow().path.clone()
    }

    /// Get the status of this database.
    fn status(&self) -> Result<DbStatus, Error> {
        self.inner.borrow().status()
    }

    fn count(&self) -> usize {
        self.inner.borrow().package_cache.len()
    }

    /// Get a package in this database, if present.
    fn package(
        &self,
        name: impl AsRef<str>,
        version: impl AsRef<str>,
    ) -> Result<Rc<LocalPackage>, Error> {
        self.inner.borrow().package(name, version)
    }

    /// Iterate over all packages.
    ///
    /// The closure allows propagating errors, but errors can occur outside of the closure of type
    /// `Error`, which is why the `From` bound exists. If your closure can't error, just use
    /// `E = Error`.
    ///
    /// Because the closure receives reference counted packages, they are cheap to clone, and can
    /// be collected into a Vec if that is desired.
    fn packages<E, F>(&self, f: F) -> Result<(), E>
    where
        F: FnMut(Rc<LocalPackage>) -> Result<(), E>,
        E: From<Error>,
    {
        self.inner.borrow().packages(f)
    }

    /// Get the latest version of a package in this database, if a version is present.
    fn package_latest<Str>(&self, name: Str) -> Result<Rc<LocalPackage>, Error>
    where
        Str: AsRef<str>,
    {
        self.inner.borrow().package_latest(name)
    }
}

/// A package database.
#[derive(Debug)]
pub struct LocalDatabaseInner {
    handle: Weak<RefCell<Handle>>,
    /// The level of signature verification required to accept packages
    sig_level: SignatureLevel,
    /// Which operations this database will be used for.
    usage: DbUsage,
    /// The database path.
    path: PathBuf,
    /// The package cache (HashMap of package name to package version to package, which lazily
    /// gets info from disk)
    package_cache: HashMap<PackageKey<'static>, RefCell<MaybePackage>>,
    /// Count of the number of packages (cached)
    package_count: usize,
}

impl LocalDatabaseInner {
    /// Helper to create a new database
    ///
    /// Path is the root path for databases.
    ///
    /// The database folder will be read to get a cache of package names.
    // This function must not panic, it is UB to panic here.
    pub(crate) fn new(
        handle: &Rc<RefCell<Handle>>,
        sig_level: SignatureLevel,
    ) -> LocalDatabaseInner {
        //  path is `$db_path SEP $local_db_name` for local
        let path = handle.borrow().database_path.join(LOCAL_DB_NAME);
        LocalDatabaseInner {
            handle: Rc::downgrade(handle),
            sig_level,
            usage: DbUsage::default(),
            path,
            package_cache: HashMap::new(),
            package_count: 0,
        }
    }

    /// Helper to create a new version file for the local database.
    #[inline]
    fn create_version_file(&self) -> io::Result<()> {
        let mut version_file = fs::File::create(&self.path)?;
        // Format is number followed by single newline
        writeln!(version_file, "{}", LOCAL_DB_CURRENT_VERSION)?;
        Ok(())
    }

    /// Get a package from the database
    fn package(
        &self,
        name: impl AsRef<str>,
        version: impl AsRef<str>,
    ) -> Result<Rc<LocalPackage>, Error> {
        let name = name.as_ref();
        let version = version.as_ref();

        self.package_cache
            .get(&PackageKey::from_borrowed(name, version))
            .ok_or(ErrorKind::InvalidLocalPackage(name.to_owned()))?
            .borrow_mut()
            .load(self.handle.clone())
    }

    /// Get the latest version of a package from the database.
    ///
    /// There should only be one version of a package installed at any time,
    /// so this function is kinda useless, and it's also expensive as it has to traverse the
    /// hashtable.
    fn package_latest(&self, name: impl AsRef<str>) -> Result<Rc<LocalPackage>, Error> {
        let name = name.as_ref();

        self.package_cache
            .iter()
            .filter(|(key, _value)| key.name == name)
            .max_by_key(|(key, _value)| &key.version)
            .ok_or(ErrorKind::InvalidLocalPackage(name.to_owned()))?
            .1
            .borrow_mut()
            .load(self.handle.clone())
    }

    fn packages<'a, E, F>(&'a self, mut f: F) -> Result<(), E>
    where
        F: FnMut(Rc<LocalPackage>) -> Result<(), E>,
        E: From<Error>,
    {
        for pkg in self
            .package_cache
            .values()
            .map(|pkg| pkg.borrow_mut().load(self.handle.clone()))
        {
            f(pkg?)?;
        }
        Ok(())
    }

    /// Get the status of this database.
    ///
    /// This does not validate installed packages, just the internal structure of the database.
    fn status(&self) -> Result<DbStatus, Error> {
        let md = match fs::metadata(&self.path) {
            Ok(md) => md,
            Err(ref e) if e.kind() == io::ErrorKind::NotFound => {
                return Ok(DbStatus::Missing);
            }
            Err(e) => return Err(e.into()),
        };

        if !md.is_dir() {
            return Ok(DbStatus::Invalid);
        }

        log::debug!("checking local database version");
        let valid = match fs::read(self.path.join(&LOCAL_DB_VERSION_FILE)) {
            Ok(version_raw) => {
                // Check version is up to date.
                if let Some(version) = atoi::<u64>(&version_raw) {
                    if version == LOCAL_DB_CURRENT_VERSION {
                        true
                    } else {
                        log::warn!(
                            r#"local database version is "{}" which is not the latest ("{}")"#,
                            version,
                            LOCAL_DB_CURRENT_VERSION
                        );
                        false
                    }
                } else {
                    log::error!(
                        r#""{}" is not a valid version"#,
                        String::from_utf8_lossy(&version_raw)
                    );
                    false
                }
            }
            Err(ref e) if e.kind() == io::ErrorKind::NotFound => {
                // check directory is empty and create version file
                log::debug!("local database version file not found - creating");
                match fs::read_dir(&self.path) {
                    Ok(ref mut d) => match d.next() {
                        Some(_) => false,
                        None => match self.create_version_file() {
                            Ok(_) => true,
                            Err(e) => {
                                log::error!(
                                    "could not create version file for local database at {}",
                                    self.path.display()
                                );
                                log::error!("caused by {}", e);
                                false
                            }
                        },
                    },
                    Err(e) => {
                        log::error!(
                            "could not check contents of local database directory at {}",
                            self.path.display()
                        );
                        log::error!("caused by {}", e);
                        false
                    }
                }
            }
            Err(e) => {
                log::error!(
                    "could not read version file for the local database at {}",
                    self.path.display()
                );
                log::error!("caused by {}", e);
                false
            }
        };
        Ok(if valid {
            DbStatus::Valid
        } else {
            DbStatus::Invalid
        })
    }

    /// Load all package names into the cache, and validate the database
    // The syscalls for this function are a single readdir and a stat per subentry
    pub(crate) fn populate_package_cache(&mut self) -> Result<(), Error> {
        log::debug!(
            r#"searching for local packages in "{}""#,
            self.path.display()
        );
        for entry in fs::read_dir(&self.path)? {
            let entry = entry?;
            if !entry.metadata()?.is_dir() {
                // Check ALPM_DB_VERSION
                if entry.file_name() == OsStr::new(LOCAL_DB_VERSION_FILE) {
                } else {
                    // ignore extra files for now (should probably error)
                    log::warn!(
                        "Unexpected file {} found in local db directory",
                        entry.path().display()
                    );
                }
                continue;
            }
            let path = entry.path();
            // Non-utf8 is hard until https://github.com/rust-lang/rfcs/pull/2295 lands
            let file_name = entry
                .file_name()
                .into_string()
                .expect("non-utf8 package names not yet supported");
            let (name, version) = super::split_package_dirname(&file_name)
                .ok_or(ErrorKind::InvalidLocalPackage(file_name.to_owned()))?;
            log::debug!(r#"found "{}", version: "{}""#, name, version);
            if self
                .package_cache
                .insert(
                    PackageKey::from_owned(name.to_owned(), version),
                    RefCell::new(MaybePackage::new(path, name, version)),
                )
                .is_some()
            {
                // This should not be possible (since name comes from unique filename)
                panic!("Found package in localdb with duplicate name/version");
            }
        }
        Ok(())
    }
}

/// A lazy-loading package
#[derive(Debug, Clone, PartialEq)]
enum MaybePackage {
    /// Not loaded the package yet
    Unloaded {
        path: PathBuf,
        name: String,
        version: String,
    },
    /// Loaded the package
    Loaded(Rc<LocalPackage>),
}

impl MaybePackage {
    /// Create an unloaded package
    fn new(
        path: impl Into<PathBuf>,
        name: impl Into<String>,
        version: impl Into<String>,
    ) -> MaybePackage {
        MaybePackage::Unloaded {
            path: path.into(),
            name: name.into(),
            version: version.into(),
        }
    }

    /// Load the package if necessary and return it
    fn load(&mut self, handle: Weak<RefCell<Handle>>) -> Result<Rc<LocalPackage>, Error> {
        match self {
            MaybePackage::Unloaded {
                path,
                name,
                version,
            } => {
                // todo find a way to avoid cloning `path`
                let pkg = Rc::new(LocalPackage::from_local(
                    path.clone(),
                    name,
                    version,
                    handle,
                )?);
                *self = MaybePackage::Loaded(pkg.clone());
                Ok(pkg)
            }
            MaybePackage::Loaded(pkg) => Ok(pkg.clone()),
        }
    }
}
