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
    let mut command = Command::new(&program);
    command
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    detach(&mut command);
    let mut child = command
        .spawn()
        .with_context(|| format!("starting viewer {program:?}"))?;
    // Reap it when it exits so long reading sessions don't pile up zombies.
    std::thread::spawn(move || child.wait());
    Ok(())
}

/// Own process group, so Ctrl-C or closing hon's terminal doesn't kill the viewer.
#[cfg(unix)]
fn detach(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(windows)]
fn detach(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
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
        // The README's Windows example: quoting keeps the backslashes.
        let sumatra = r"C:\Program Files\SumatraPDF\SumatraPDF.exe";
        for viewer in [format!("\"{sumatra}\""), format!("'{sumatra}'")] {
            assert_eq!(
                viewer_command(&viewer, Path::new(r"C:\m\a.pdf")).unwrap(),
                (sumatra.into(), vec![r"C:\m\a.pdf".into()])
            );
        }
        assert_ne!(
            viewer_command(sumatra, Path::new("x")).unwrap().0,
            sumatra,
            "unquoted, shell-style splitting mangles the path"
        );
        assert!(viewer_command("  ", Path::new("/x")).is_err());
    }
}
