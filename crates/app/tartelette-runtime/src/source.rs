//! The files a run is built from.

/// A file the run was built from.
#[derive(Clone)]
pub struct Source {
    pub name: String,
    pub bytes: Vec<u8>,
    /// Where it came from on disk (native only), for reloading.
    #[cfg(not(target_arch = "wasm32"))]
    pub path: Option<std::path::PathBuf>,
}

impl Source {
    pub fn new(name: impl Into<String>, bytes: Vec<u8>) -> Self {
        Self {
            name: name.into(),
            bytes,
            #[cfg(not(target_arch = "wasm32"))]
            path: None,
        }
    }

    /// Read a file, remembering where it came from.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn from_file(path: &std::path::Path) -> Result<Self, String> {
        let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let name = path.file_name().map_or_else(
            || path.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
        Ok(Self {
            name,
            bytes,
            path: std::fs::canonicalize(path).ok(),
        })
    }
}
