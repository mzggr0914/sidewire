use super::*;
use crate::client::{exec_control, list_devices, request_control};
use std::path::{Component, Path};

const MAX_TRANSFER_JOBS: usize = 16;
const MAX_MKDIR_BATCH_PATHS: usize = 64;
const MAX_MKDIR_BATCH_BYTES: usize = 32 * 1024;

async fn join_transfer(
    transfers: &mut tokio::task::JoinSet<Result<u64>>,
    files: &mut u64,
    bytes: &mut u64,
) -> Result<()> {
    let transferred = transfers
        .join_next()
        .await
        .context("transfer task set ended unexpectedly")???;
    *files += 1;
    *bytes = bytes
        .checked_add(transferred)
        .context("transfer byte count overflow")?;
    Ok(())
}

async fn drain_transfers(
    transfers: &mut tokio::task::JoinSet<Result<u64>>,
    files: &mut u64,
    bytes: &mut u64,
) -> Result<()> {
    while !transfers.is_empty() {
        join_transfer(transfers, files, bytes).await?;
    }
    Ok(())
}

fn transfer_jobs(jobs: usize) -> usize {
    jobs.clamp(1, MAX_TRANSFER_JOBS)
}

fn absolute_output(path: PathBuf) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

async fn pull_file_destination(remote: &str, local: PathBuf) -> Result<PathBuf> {
    let metadata = match tokio::fs::metadata(&local).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(local),
        Err(error) => {
            return Err(error).with_context(|| format!("inspect local path {}", local.display()));
        }
    };
    if !metadata.is_dir() {
        return Ok(local);
    }

    let file_name = remote
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .context("remote file path does not have a file name")?;
    let mut components = Path::new(file_name).components();
    let file_name = match (components.next(), components.next()) {
        (Some(Component::Normal(name)), None) => name,
        _ => bail!("remote file path does not have a valid local file name: {remote}"),
    };
    Ok(local.join(file_name))
}

fn remote_join(base: &str, relative: &Path) -> Result<String> {
    let mut output = if base == "/" {
        "/".to_owned()
    } else {
        base.trim_end_matches('/').to_owned()
    };
    for component in relative.components() {
        let part = match component {
            Component::Normal(part) => part,
            Component::CurDir => continue,
            _ => bail!("unsupported local path component in {}", relative.display()),
        };
        if !output.ends_with('/') {
            output.push('/');
        }
        output.push_str(&part.to_string_lossy());
    }
    Ok(output)
}
async fn push_file(
    control: &str,
    device: Option<String>,
    run_as: RunAs,
    local: &Path,
    remote: String,
) -> Result<u64> {
    let size = tokio::fs::metadata(local).await?.len();
    let request = ControlRequest::Push {
        device,
        local: local.to_string_lossy().into_owned(),
        remote,
        run_as,
    };
    match request_control(control, &request).await? {
        ControlResponse::Ok { .. } => Ok(size),
        ControlResponse::Error { message } => bail!(message),
        _ => bail!("unexpected push response"),
    }
}

async fn pull_file(
    control: &str,
    device: Option<String>,
    run_as: RunAs,
    remote: String,
    local: &Path,
) -> Result<u64> {
    let request = ControlRequest::Pull {
        device,
        remote,
        local: local.to_string_lossy().into_owned(),
        run_as,
    };
    match request_control(control, &request).await? {
        ControlResponse::Ok { .. } => Ok(tokio::fs::metadata(local).await?.len()),
        ControlResponse::Error { message } => bail!(message),
        _ => bail!("unexpected pull response"),
    }
}

async fn ensure_remote_dirs(
    control: &str,
    device: Option<String>,
    run_as: RunAs,
    paths: &[String],
) -> Result<()> {
    let mut start = 0usize;
    while start < paths.len() {
        let mut end = start;
        let mut bytes = 0usize;
        while end < paths.len() && end - start < MAX_MKDIR_BATCH_PATHS {
            let next = paths[end].len().saturating_add(1);
            if end > start && bytes.saturating_add(next) > MAX_MKDIR_BATCH_BYTES {
                break;
            }
            bytes = bytes.saturating_add(next);
            end += 1;
        }
        let mut args = Vec::with_capacity(end - start + 1);
        args.push("-p".to_owned());
        args.extend(paths[start..end].iter().cloned());
        let (_, stderr, code) =
            exec_control(control, device.clone(), run_as, "/system/bin/mkdir", args).await?;
        if code.unwrap_or(1) != 0 {
            bail!(
                "mkdir batch for {} paths failed: {}",
                end - start,
                stderr.trim()
            );
        }
        start = end;
    }
    Ok(())
}

async fn remote_is_dir(
    control: &str,
    device: Option<String>,
    run_as: RunAs,
    path: &str,
) -> Result<bool> {
    let (_, _, code) = exec_control(
        control,
        device,
        run_as,
        "/system/bin/sh",
        vec![
            "-c".into(),
            "[ -d \"$1\" ]".into(),
            "sidewire-dir-test".into(),
            path.into(),
        ],
    )
    .await?;
    Ok(code == Some(0))
}

async fn remote_find(
    control: &str,
    device: Option<String>,
    run_as: RunAs,
    root: &str,
    kind: &str,
) -> Result<Vec<String>> {
    let (stdout, stderr, code) = exec_control(
        control,
        device,
        run_as,
        "/system/bin/find",
        vec![root.into(), "-type".into(), kind.into(), "-print0".into()],
    )
    .await?;
    if code.unwrap_or(1) != 0 {
        bail!("find {root}: {}", stderr.trim());
    }
    Ok(stdout
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
        .collect())
}

fn local_relative(remote_root: &str, remote_path: &str) -> Result<PathBuf> {
    let root = remote_root.trim_end_matches('/');
    let relative = if root.is_empty() {
        remote_path.trim_start_matches('/')
    } else {
        remote_path
            .strip_prefix(root)
            .with_context(|| format!("'{remote_path}' is outside '{remote_root}'"))?
            .trim_start_matches('/')
    };
    let mut output = PathBuf::new();
    for part in relative.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        let mut components = Path::new(part).components();
        match (components.next(), components.next()) {
            (Some(Component::Normal(name)), None) => output.push(name),
            _ => bail!("unsafe remote path '{remote_path}'"),
        }
    }
    Ok(output)
}

async fn push_path(
    control: &str,
    device: Option<String>,
    run_as: RunAs,
    jobs: usize,
    local: PathBuf,
    remote: String,
) -> Result<String> {
    let jobs = transfer_jobs(jobs);
    let local = tokio::fs::canonicalize(local).await?;
    let metadata = tokio::fs::metadata(&local).await?;
    if metadata.is_file() {
        let remote = if remote_is_dir(control, device.clone(), run_as, &remote).await? {
            let file_name = local
                .file_name()
                .context("local file path does not have a file name")?;
            remote_join(&remote, Path::new(file_name))?
        } else {
            remote
        };
        let bytes = push_file(control, device, run_as, &local, remote).await?;
        return Ok(format!("pushed 1 file ({bytes} bytes)"));
    }
    if !metadata.is_dir() {
        bail!(
            "local path is not a regular file or directory: {}",
            local.display()
        );
    }

    let mut stack = vec![local.clone()];
    let mut remote_dirs = vec![remote.clone()];
    let mut pending_files = Vec::new();
    while let Some(directory) = stack.pop() {
        let mut entries = tokio::fs::read_dir(&directory).await?;
        while let Some(entry) = entries.next_entry().await? {
            let file_type = entry.file_type().await?;
            let path = entry.path();
            let relative = path.strip_prefix(&local)?;
            let remote_path = remote_join(&remote, relative)?;
            if file_type.is_symlink() {
                bail!("recursive push does not follow symlink {}", path.display());
            }
            if file_type.is_dir() {
                remote_dirs.push(remote_path);
                stack.push(path);
            } else if file_type.is_file() {
                pending_files.push((path, remote_path));
            }
        }
    }

    ensure_remote_dirs(control, device.clone(), run_as, &remote_dirs).await?;
    let directories = remote_dirs.len() as u64;
    let mut files = 0u64;
    let mut bytes = 0u64;
    let mut transfers = tokio::task::JoinSet::new();
    for (path, remote_path) in pending_files {
        while transfers.len() >= jobs {
            join_transfer(&mut transfers, &mut files, &mut bytes).await?;
        }
        let control = control.to_owned();
        let device = device.clone();
        transfers
            .spawn(async move { push_file(&control, device, run_as, &path, remote_path).await });
    }
    drain_transfers(&mut transfers, &mut files, &mut bytes).await?;
    Ok(format!(
        "pushed {files} files in {directories} directories ({bytes} bytes)"
    ))
}

pub(super) async fn run_push(
    control: &str,
    device: Option<String>,
    run_as: RunAs,
    jobs: usize,
    local: PathBuf,
    remote: String,
) -> Result<()> {
    println!(
        "{}",
        push_path(control, device, run_as, jobs, local, remote).await?
    );
    Ok(())
}

pub(super) async fn run_push_all(
    control: &str,
    run_as: RunAs,
    jobs: usize,
    local: PathBuf,
    remote: String,
) -> Result<()> {
    let devices = list_devices(control).await?;
    if devices.is_empty() {
        bail!("no SideWire devices connected");
    }
    let mut tasks = tokio::task::JoinSet::new();
    for device in devices {
        let control = control.to_owned();
        let local = local.clone();
        let remote = remote.clone();
        tasks.spawn(async move {
            let result = push_path(
                &control,
                Some(device.id.clone()),
                run_as,
                jobs,
                local,
                remote,
            )
            .await;
            (device, result)
        });
    }
    let mut failed = false;
    while let Some(joined) = tasks.join_next().await {
        let (device, result) = joined?;
        match result {
            Ok(summary) => println!("[{}:{}] {summary}", device.name, &device.id[..8]),
            Err(error) => {
                eprintln!("[{}:{}] {error}", device.name, &device.id[..8]);
                failed = true;
            }
        }
    }
    if failed {
        bail!("one or more device pushes failed");
    }
    Ok(())
}

pub(super) async fn run_pull(
    control: &str,
    device: Option<String>,
    run_as: RunAs,
    jobs: usize,
    remote: String,
    local: PathBuf,
) -> Result<()> {
    let jobs = transfer_jobs(jobs);
    let local = absolute_output(local)?;
    if !remote_is_dir(control, device.clone(), run_as, &remote).await? {
        let local = pull_file_destination(&remote, local).await?;
        let bytes = pull_file(control, device, run_as, remote, &local).await?;
        println!("pulled 1 file ({bytes} bytes)");
        return Ok(());
    }

    tokio::fs::create_dir_all(&local).await?;
    let directories = remote_find(control, device.clone(), run_as, &remote, "d").await?;
    for directory in &directories {
        let relative = local_relative(&remote, directory)?;
        tokio::fs::create_dir_all(local.join(relative)).await?;
    }

    let remote_files = remote_find(control, device.clone(), run_as, &remote, "f").await?;
    let mut files = 0u64;
    let mut bytes = 0u64;
    let mut transfers = tokio::task::JoinSet::new();
    for remote_file in remote_files {
        let relative = local_relative(&remote, &remote_file)?;
        let local_file = local.join(relative);
        if let Some(parent) = local_file.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        while transfers.len() >= jobs {
            join_transfer(&mut transfers, &mut files, &mut bytes).await?;
        }
        let control = control.to_owned();
        let device = device.clone();
        transfers.spawn(async move {
            pull_file(&control, device, run_as, remote_file, &local_file).await
        });
    }
    drain_transfers(&mut transfers, &mut files, &mut bytes).await?;

    println!(
        "pulled {files} files in {} directories ({bytes} bytes)",
        directories.len()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{absolute_output, pull_file_destination};
    use std::path::PathBuf;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "sidewire-pull-test-{:032x}",
                rand::random::<u128>()
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn single_file_pull_into_current_directory_keeps_remote_name() {
        let name = "duck_detector_report_SM-A325N_20261024_101858.txt";
        let remote = format!("/sdcard/Download/{name}");
        for destination in [".", "./"] {
            let local = absolute_output(PathBuf::from(destination)).unwrap();
            assert_eq!(
                pull_file_destination(&remote, local.clone()).await.unwrap(),
                local.join(name)
            );
        }
        #[cfg(windows)]
        {
            let local = absolute_output(PathBuf::from(".\\")).unwrap();
            assert_eq!(
                pull_file_destination(&remote, local.clone()).await.unwrap(),
                local.join(name)
            );
        }
    }

    #[tokio::test]
    async fn existing_directory_preserves_spaces_and_unicode_in_remote_name() {
        let directory = TestDirectory::new();
        let name = "기기 보고서.txt";
        let local = directory.0.join("downloads.txt");
        tokio::fs::create_dir(&local).await.unwrap();
        assert_eq!(
            pull_file_destination(&format!("/sdcard/Download/{name}"), local.clone())
                .await
                .unwrap(),
            local.join(name)
        );
    }

    #[tokio::test]
    async fn explicit_file_and_missing_destinations_keep_the_requested_path() {
        let directory = TestDirectory::new();
        let existing = directory.0.join("renamed.txt");
        tokio::fs::write(&existing, b"existing contents")
            .await
            .unwrap();
        for local in [
            existing.clone(),
            directory.0.join("newfolder"),
            directory.0.join("missing-parent").join("renamed.txt"),
        ] {
            assert_eq!(
                pull_file_destination("/sdcard/Download/report.txt", local.clone())
                    .await
                    .unwrap(),
                local
            );
        }
        assert_eq!(
            tokio::fs::read(&existing).await.unwrap(),
            b"existing contents"
        );
        assert!(!directory.0.join("newfolder").exists());
    }

    #[tokio::test]
    async fn directory_destination_rejects_missing_or_special_remote_file_names() {
        let directory = TestDirectory::new();
        for remote in ["", "/", "/sdcard/Download/", "/sdcard/.", "/sdcard/.."] {
            assert!(
                pull_file_destination(remote, directory.0.clone())
                    .await
                    .is_err(),
                "unexpectedly accepted {remote:?}"
            );
        }
        #[cfg(windows)]
        for remote in ["/sdcard/..\\escape.txt", "/sdcard/C:escape.txt"] {
            assert!(
                pull_file_destination(remote, directory.0.clone())
                    .await
                    .is_err(),
                "unexpectedly accepted {remote:?}"
            );
        }
    }
}
