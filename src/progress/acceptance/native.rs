//! Trusted native collectors import the role's bounded report before signalling.
use super::*;
use std::path::Path;
pub fn collect(
    queue_root: &Path,
    wait: &str,
    identity: &J,
    cwd: &Path,
    outcome: &str,
) -> Result<()> {
    if outcome != "complete" {
        return Ok(());
    }
    let mut queue = crate::queue::load_queue(queue_root)?;
    if queue["waits"][wait]["lineage"]["role"] != "acceptance" {
        return Ok(());
    }
    let path = cwd.join(".herdr-verdict.json");
    let Some(report) = read_report(&path)? else {
        return Ok(());
    };
    submit(&mut queue, wait, identity, &report)?;
    crate::queue::save(queue_root, &mut queue)
}

fn read_report(path: &Path) -> Result<Option<J>> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    read_regular(file).map(Some)
}
fn read_regular(file: std::fs::File) -> Result<J> {
    use std::io::Read;
    ensure(file.metadata()?.is_file(), "verdict must be a regular file")?;
    let mut bytes = Vec::new();
    file.take(131073).read_to_end(&mut bytes)?;
    ensure(bytes.len() <= 131072, "verdict exceeds size bound")?;
    Ok(serde_json::from_slice(&bytes)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn report_reads_are_bounded_and_do_not_follow_links() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("report");
        assert!(read_report(&path).unwrap().is_none());
        std::fs::write(&path, b"{}").unwrap();
        assert_eq!(read_report(&path).unwrap(), Some(json!({})));
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(read_report(&link).is_err());
        assert!(read_report(dir.path()).is_err());
        std::fs::write(&path, vec![b' '; 131073]).unwrap();
        assert!(read_report(&path).is_err());
        std::fs::write(&path, b"partial {").unwrap();
        assert!(read_report(&path).is_err());
    }
}
