use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, anyhow, bail};

/// Splits `viewer` shell-style and appends `path` as the last argument.
pub fn viewer_command(viewer: &str, path: &Path) -> anyhow::Result<(String, Vec<String>)> {
    let mut args =
        shlex::split(viewer).ok_or_else(|| anyhow!("viewer {viewer:?} has unbalanced quotes"))?;
    if args.is_empty() {
        bail!("viewer is empty");
    }
    let program = args.remove(0);
    args.push(path.to_string_lossy().into_owned());
    Ok((program, args))
}

/// Opens `path` with `viewer`, or the OS default app when unset. Never waits for it.
pub fn open_in_viewer(path: &Path, viewer: Option<&str>) -> anyhow::Result<()> {
    let Some(viewer) = viewer else {
        return open::that_detached(path).with_context(|| format!("opening {}", path.display()));
    };
    let (program, args) = viewer_command(viewer, path)?;
    Command::new(&program)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("starting viewer {program:?}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viewer_command_splits_and_appends() {
        assert_eq!(
            viewer_command("open -a 'YACReader'", Path::new("/x/a b.pdf")).unwrap(),
            (
                "open".into(),
                vec!["-a".into(), "YACReader".into(), "/x/a b.pdf".into()]
            )
        );
        assert!(viewer_command("open 'unterminated", Path::new("/x")).is_err());
        assert!(viewer_command("  ", Path::new("/x")).is_err());
    }
}
