use anyhow::{anyhow, bail, Context, Result};
use qlty_config::Workspace;
use std::fs::Permissions;
use std::fs::{copy, create_dir_all};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{debug, error, trace};

use crate::source_reader::{SourceReader, SourceReaderFs};
use crate::utils::generate_random_id;

#[allow(dead_code)]
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum Mode {
    ReadOnly,
    ReadWrite,
    Source,
}

impl Mode {
    #[cfg(unix)]
    fn permissions(&self) -> Permissions {
        use std::os::unix::fs::PermissionsExt;
        match *self {
            Mode::ReadOnly => Permissions::from_mode(0o555), // r-xr-xr-x
            Mode::ReadWrite => Permissions::from_mode(0o755), // rwxr-xr-x
            Mode::Source => Permissions::from_mode(0o555),   // not used
        }
    }

    fn suffix(&self) -> &'static str {
        match *self {
            Mode::ReadOnly => "-ro",
            Mode::ReadWrite => "-rw",
            Mode::Source => "-ro", // not used
        }
    }
}

#[derive(Debug, Clone)]
pub struct StagingArea {
    pub source_directory: PathBuf,
    pub destination_directory: PathBuf,
    pub mode: Mode,
    source_reader: Arc<dyn SourceReader>,
}

impl SourceReader for StagingArea {
    fn read(&self, relative_path: PathBuf) -> std::io::Result<String> {
        let staged_file_path = self.destination_directory.join(relative_path);
        self.source_reader.read(staged_file_path)
    }

    fn write(&self, relative_path: PathBuf, content: String) -> std::io::Result<()> {
        let staged_file_path = self.destination_directory.join(relative_path);
        self.source_reader.write(staged_file_path, content)
    }
}

impl StagingArea {
    pub fn new(source: PathBuf, destination: PathBuf, mode: Mode) -> Self {
        Self {
            source_reader: Arc::<SourceReaderFs>::default(),
            source_directory: source,
            destination_directory: destination,
            mode,
        }
    }

    pub fn generate(mode: Mode, source: PathBuf, tmp_dir: Option<PathBuf>) -> Self {
        if mode == Mode::Source {
            // don't do anything in Source mode
            return Self::new(source.clone(), source, mode);
        }

        let random_id = generate_random_id(8);

        Self::new(
            source.clone(),
            tmp_dir.unwrap().join(random_id + mode.suffix()),
            mode,
        )
    }

    pub fn stage(&self, path: &Path) -> Result<()> {
        if self.mode == Mode::Source {
            // don't do anything in Source mode
            return Ok(());
        }

        let from_workspace_root = self.source_directory.join(path);
        let to_staging_directory = self.destination_directory.join(path);

        self.copy_targets(&from_workspace_root, &to_staging_directory)
    }

    pub fn unstage_file(&self, path: &Path) -> Result<()> {
        match self.mode {
            Mode::Source => Ok(()),
            Mode::ReadOnly => bail!("Cannot unstage_file in read-only mode"),
            Mode::ReadWrite => {
                let from_staging = self.destination_directory.join(path);
                let to_workspace = self.source_directory.join(path);
                self.copy_file(&from_staging, &to_workspace)
            }
        }
    }

    pub fn set_directory_permissions(&self, permissions: Permissions) -> Result<()> {
        std::fs::set_permissions(&self.destination_directory, permissions).with_context(|| {
            format!(
                "Failed to set permissions for dir {}",
                self.destination_directory.display()
            )
        })
    }

    pub fn create_directory(&self) -> Result<()> {
        create_dir_all(&self.destination_directory).with_context(|| {
            format!(
                "Failed to create tmpfile dir {}",
                self.destination_directory.display()
            )
        })?;

        #[cfg(unix)]
        {
            self.set_directory_permissions(self.mode.permissions())?;
        }

        Ok(())
    }

    pub fn copy_file(&self, from: &Path, to: &Path) -> Result<()> {
        trace!("Copying file {} to {}", from.display(), to.display());

        let to_dir = to
            .parent()
            .ok_or_else(|| anyhow!("parent directory not found for {:?}", to))?;

        create_dir_all(to_dir).with_context(|| {
            format!(
                "Failed to create workspace entry parentdir: {}",
                to_dir.display()
            )
        })?;

        // copy with retry to handle in-use files
        let mut copy_result: std::io::Result<_> = Ok(0);
        for i in 0..10 {
            copy_result = copy(from, to);
            if copy_result.is_ok() {
                break;
            }

            let retry = std::time::Duration::from_millis(50 * i);
            debug!(
                "Failed to copy {} to {} ({}). Retrying in {}ms...",
                from.display(),
                to.display(),
                copy_result.as_ref().unwrap_err(),
                retry.as_millis()
            );
            std::thread::sleep(std::time::Duration::from_millis(50 * i));
        }
        copy_result
            .with_context(|| format!("Failed to copy {} to {}", from.display(), to.display()))?;

        Ok(())
    }

    fn copy_directory(&self, from: &Path, to: &Path) -> Result<()> {
        trace!("Copying directory {} to {}", from.display(), to.display());

        create_dir_all(to)
            .with_context(|| format!("Failed to create target parentdir: {}", to.display()))?;

        let mut copy_options = fs_extra::dir::CopyOptions::new();
        copy_options.content_only = true;

        fs_extra::dir::copy(from, to, &copy_options)
            .with_context(|| format!("Failed to copy {} to {}", from.display(), to.display()))?;

        Ok(())
    }

    pub fn copy_targets(&self, from: &Path, to: &Path) -> Result<()> {
        if let Ok(metadata) =
            std::fs::metadata(from).with_context(|| format!("Could not find: {:?}", from))
        {
            if metadata.is_file() {
                self.copy_file(from, to)?
            } else if metadata.is_dir() {
                self.copy_directory(from, to)?
            } else {
                error!("{:?} is neither a standard file nor a directory.", from);
            }
        } else {
            error!("Error reading metadata for: {:?}", from);
        }

        Ok(())
    }

    pub fn read_lines(&self, relative_path: &Path) -> Result<Vec<String>> {
        Ok(self
            .read(relative_path.into())?
            .lines()
            .map(|line| line.to_string())
            .collect())
    }

    pub fn write_to_source(&self, relative_path: &Path, content: String) -> Result<()> {
        self.write(relative_path.to_path_buf(), content)?;
        self.unstage_file(relative_path)?;
        Ok(())
    }
}

pub fn load_config_file_from_repository(
    config_file: impl AsRef<Path>,
    workspace: &Workspace,
    destination: impl AsRef<Path>,
) -> Result<String> {
    let to = destination
        .as_ref()
        .join(config_file.as_ref().strip_prefix(&workspace.root).unwrap());

    load_config_file_from(config_file, to)
}

pub fn load_config_file_from_source(
    config_file: impl AsRef<Path>,
    destination: impl AsRef<Path>,
) -> Result<String> {
    load_config_file_from(
        &config_file,
        destination
            .as_ref()
            .join(config_file.as_ref().file_name().unwrap()),
    )
}

pub fn load_config_file_from_qlty_dir(
    config_file: impl AsRef<Path>,
    workspace: &Workspace,
    destination: impl AsRef<Path>,
) -> Result<String> {
    let config_file_name = config_file.as_ref().file_name().unwrap();
    let from = workspace.library()?.configs_dir().join(config_file_name);
    let to = destination.as_ref().join(config_file_name);
    load_config_file_from(from, to)
}

fn load_config_file_from(from: impl AsRef<Path>, to: impl AsRef<Path>) -> Result<String> {
    let from = from.as_ref();
    let to = to.as_ref();
    if !from.exists() {
        return Ok("".to_string());
    } else if to.exists() {
        debug!("Config file already exists in workspace: {:?}", to);
        return Ok(to.display().to_string());
    }

    debug!("Symlinking config file: {:?} -> {:?}", from, to);

    symlink(from, to).with_context(|| {
        format!(
            "Failed to symlink config file {} to {}",
            from.display(),
            to.display()
        )
    })?;

    Ok(to.display().to_string())
}

#[cfg(windows)]
fn symlink(from: &Path, to: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_file(from, to)
}

#[cfg(unix)]
fn symlink(from: &Path, to: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(from, to)
}

#[cfg(test)]
mod test {
    use super::*;
    use tempfile::{tempdir, TempDir};

    struct TempPaths {
        source: Arc<TempDir>,
        dest: Arc<TempDir>,
    }

    fn new_staging_area(mode: Mode) -> (StagingArea, TempPaths) {
        let source = Arc::new(tempdir().unwrap());
        let dest = if mode == Mode::Source {
            source.clone()
        } else {
            Arc::new(tempdir().unwrap())
        };
        (
            StagingArea::new(source.path().to_path_buf(), dest.path().to_path_buf(), mode),
            TempPaths { source, dest },
        )
    }

    fn read_from_disk(temp_path: Arc<TempDir>, file: &str) -> String {
        std::fs::read_to_string(temp_path.path().join(file)).unwrap()
    }

    #[test]
    fn source_reader_impl() {
        let (stage, paths) = new_staging_area(Mode::ReadWrite);
        assert!(stage.write("test".into(), "expected".to_string()).is_ok());

        let result = std::fs::read_to_string(paths.dest.path().join("test")).unwrap();
        assert_eq!(result, "expected");
        assert!(std::fs::read_to_string(paths.source.path().join("test")).is_err());

        let read_result = stage.read("test".into()).unwrap();
        assert_eq!(read_result, "expected");
    }

    #[test]
    fn write_to_source() {
        let (stage, paths) = new_staging_area(Mode::ReadWrite);
        stage
            .write_to_source(Path::new("test"), "expected".to_string())
            .unwrap();

        assert_eq!(read_from_disk(paths.source, "test"), "expected");
        assert_eq!(read_from_disk(paths.dest, "test"), "expected");
        assert_eq!(stage.read("test".into()).unwrap(), "expected");
    }

    #[test]
    fn unstage_file_readonly_mode() {
        let (stage, paths) = new_staging_area(Mode::ReadOnly);
        assert!(stage.write("test".into(), "test".into()).is_ok());
        assert!(stage.unstage_file(Path::new("test")).is_err());
        assert!(paths.dest.path().join("test").exists());
    }

    #[test]
    fn unstage_file_source_mode() {
        let (stage, paths) = new_staging_area(Mode::Source);
        assert!(stage.write("test".into(), "test".into()).is_ok());
        assert!(stage.unstage_file(Path::new("test")).is_ok());
        assert_eq!(read_from_disk(paths.source, "test"), "test");
    }

    #[test]
    fn unstage_file_readwrite_mode() {
        let (stage, paths) = new_staging_area(Mode::ReadWrite);
        assert!(stage.write("test".into(), "test".into()).is_ok());
        assert!(stage.unstage_file(Path::new("test")).is_ok());
        assert_eq!(read_from_disk(paths.source, "test"), "test");
    }

    #[test]
    fn clone_retains_underlying_source_reader() {
        let (stage, paths) = new_staging_area(Mode::ReadWrite);
        stage.write("test".into(), "expected".to_string()).unwrap();

        // write something else into file so we can test cached reads
        std::fs::write(paths.dest.path().join("test"), "other").unwrap();

        let clone = stage.clone();
        assert_eq!(clone.read("test".into()).unwrap(), "expected");
    }

    #[test]
    fn test_load_config_file_from_repository() {
        let (_, paths) = new_staging_area(Mode::ReadWrite);

        let config_file = paths.source.path().join("abc").join("conf.yml");
        create_dir_all(config_file.parent().unwrap()).unwrap();
        create_dir_all(paths.dest.path().join("abc")).unwrap();
        std::fs::write(&config_file, "repository_config_content").unwrap();

        let workspace = Workspace::for_root(paths.source.path()).unwrap();
        let result = load_config_file_from_repository(&config_file, &workspace, paths.dest.path());
        assert!(result.is_ok());

        let dest_file = paths.dest.path().join("abc").join("conf.yml");
        assert!(dest_file.exists());

        let content = std::fs::read_to_string(&dest_file).unwrap();
        assert_eq!(content, "repository_config_content");
    }

    #[test]
    fn test_load_config_file_from_symlink_fail() {
        let (_, paths) = new_staging_area(Mode::ReadWrite);

        let config_file = paths.source.path().join("abc").join("conf.yml");
        create_dir_all(config_file.parent().unwrap()).unwrap();

        std::fs::write(&config_file, "repository_config_content").unwrap();

        let workspace = Workspace::for_root(paths.source.path()).unwrap();
        let result = load_config_file_from_repository(&config_file, &workspace, paths.dest.path());
        assert!(result.is_err());

        let dest_file = paths.dest.path().join("abc").join("conf.yml");
        assert!(!dest_file.exists());
    }

    #[test]
    fn test_load_config_file_from_source() {
        let (_, paths) = new_staging_area(Mode::ReadWrite);

        let config_file = paths.source.path().join("nested").join("conf.yml");
        create_dir_all(config_file.parent().unwrap()).unwrap();
        std::fs::write(&config_file, "source_config_content").unwrap();

        let result = load_config_file_from_source(&config_file, paths.dest.path());
        assert!(result.is_ok());

        let dest_file = paths.dest.path().join("conf.yml");
        assert!(dest_file.exists());

        let content = std::fs::read_to_string(&dest_file).unwrap();
        assert_eq!(content, "source_config_content");
    }

    #[test]
    fn test_load_config_file_from_qlty_dir() {
        let (_, paths) = new_staging_area(Mode::ReadWrite);
        let mock_workspace_path = tempdir().unwrap();
        let configs_dir = mock_workspace_path.path().join(".qlty").join("configs");
        create_dir_all(&configs_dir).unwrap();

        let config_file = configs_dir.join("conf.yml");
        std::fs::write(&config_file, "qlty_dir_config_content").unwrap();

        let workspace = Workspace::for_root(mock_workspace_path.path()).unwrap();
        let result = load_config_file_from_qlty_dir("conf.yml", &workspace, paths.dest.path());
        assert!(result.is_ok());

        let dest_file = paths.dest.path().join("conf.yml");
        assert!(dest_file.exists());

        let content = std::fs::read_to_string(&dest_file).unwrap();
        assert_eq!(content, "qlty_dir_config_content");
    }

    #[test]
    fn test_load_config_file_nonexistent() {
        let (_, paths) = new_staging_area(Mode::ReadWrite);
        let nonexistent_file = paths.source.path().join("conf.yml");
        let result = load_config_file_from(&nonexistent_file, paths.dest.path().join("conf.yml"));

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "");
    }

    #[test]
    fn test_load_config_file_already_exists() {
        let (_, paths) = new_staging_area(Mode::ReadWrite);
        let source_file = paths.source.path().join("conf.yml");
        let dest_file = paths.dest.path().join("conf.yml");

        std::fs::write(&source_file, "source_content").unwrap();
        std::fs::write(&dest_file, "destination_content").unwrap();

        let result = load_config_file_from(&source_file, &dest_file);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), dest_file.display().to_string());

        let content = std::fs::read_to_string(&dest_file).unwrap();
        assert_eq!(content, "destination_content");
    }
}
