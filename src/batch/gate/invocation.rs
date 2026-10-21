//! Resolve shebangs before spawn so every script and interpreter is pinned.
use super::executable;
use crate::{util::process::CommandOptions, FlowError, Result};
use std::{
    fs::File,
    os::unix::{fs::FileExt, io::AsRawFd},
    path::Path,
};

pub(super) fn prepare(argv: &[String], options: &mut CommandOptions) -> Result<Vec<String>> {
    let mut argv = argv.to_vec();
    for depth in 0..=4 {
        let program = argv
            .first()
            .ok_or_else(|| FlowError::progress("missing gate verifier executable"))?;
        let file = executable::open(Path::new(program))?;
        let Some(interpreter) = shebang(&file)? else {
            options.executable_file = Some(file);
            return Ok(argv);
        };
        if depth == 4 {
            return Err(FlowError::progress(
                "gate verifier interpreter chain exceeds four scripts",
            ));
        }
        let mut next = interpreter;
        next.push(format!("/proc/self/fd/{}", file.as_raw_fd()));
        next.extend(argv.into_iter().skip(1));
        options.inherited_files.push(file);
        argv = next;
    }
    unreachable!("bounded interpreter loop returns at its final iteration")
}

fn shebang(file: &File) -> Result<Option<Vec<String>>> {
    let mut bytes = [0; 256];
    let length = file.read_at(&mut bytes, 0)?;
    let bytes = &bytes[..length];
    if !bytes.starts_with(b"#!") {
        return Ok(None);
    }
    let end = bytes
        .iter()
        .position(|byte| *byte == b'\n')
        .ok_or_else(|| FlowError::progress("gate verifier shebang must end within 256 bytes"))?;
    let line = std::str::from_utf8(&bytes[2..end])
        .map_err(|_| FlowError::progress("gate verifier shebang must be UTF-8"))?;
    parse(line).map(Some)
}

fn parse(line: &str) -> Result<Vec<String>> {
    if line.contains('\0') {
        return Err(FlowError::progress("gate verifier shebang contains NUL"));
    }
    let line = line.trim_matches([' ', '\t']);
    let split = line.find([' ', '\t']).unwrap_or(line.len());
    let (program, argument) = line.split_at(split);
    if !Path::new(program).is_absolute() {
        return Err(FlowError::progress(
            "gate verifier interpreter must be absolute",
        ));
    }
    let mut argv = vec![program.to_owned()];
    let argument = argument.trim_start_matches([' ', '\t']);
    if !argument.is_empty() {
        argv.push(argument.to_owned());
    }
    Ok(argv)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt};

    fn script(path: &Path, text: &str) {
        fs::write(path, text).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn pinned_chain_survives_script_and_interpreter_replacement() {
        let root = crate::util::fs::test_tempdir();
        let interpreter = root.path().join("interpreter");
        let client = root.path().join("client");
        script(
            &interpreter,
            "#!/bin/sh\nprintf 'old:%s:%s' \"$1\" \"$3\"\n",
        );
        script(
            &client,
            &format!("#!{} option with spaces\n", interpreter.display()),
        );
        let argv = vec![client.to_str().unwrap().to_owned(), "request".into()];
        let mut options = CommandOptions::default();
        let pinned = prepare(&argv, &mut options).unwrap();
        for path in [&interpreter, &client] {
            let replacement = root.path().join("replacement");
            script(&replacement, "#!/bin/sh\nprintf new\n");
            fs::rename(replacement, path).unwrap();
        }
        let result = crate::util::process::run_command(&pinned, &options).unwrap();
        assert!(result.success());
        assert_eq!(result.stdout, b"old:option with spaces:request");
        for file in options
            .inherited_files
            .iter()
            .chain(options.executable_file.iter())
        {
            // SAFETY: file owns the live descriptor; this only reads its flags.
            assert_ne!(
                unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
                0
            );
        }
        let mut fresh = CommandOptions::default();
        let argv = prepare(&argv, &mut fresh).unwrap();
        assert_eq!(
            crate::util::process::run_command(&argv, &fresh)
                .unwrap()
                .stdout,
            b"new"
        );
    }

    #[test]
    fn malformed_missing_and_recursive_interpreters_fail_closed() {
        let root = crate::util::fs::test_tempdir();
        let client = root.path().join("client");
        let variants = [
            "#!relative\n".to_owned(),
            "#!\n".to_owned(),
            "#!/bin/sh\0\n".to_owned(),
            format!("#!/{}\n", "a".repeat(256)),
            format!("#!{}\n", root.path().join("missing").display()),
            format!("#!{}\n", client.display()),
        ];
        for text in variants {
            script(&client, &text);
            assert!(prepare(
                &[client.to_str().unwrap().into()],
                &mut CommandOptions::default()
            )
            .is_err());
        }
        fs::write(&client, b"#!\xff\n").unwrap();
        assert!(prepare(
            &[client.to_str().unwrap().into()],
            &mut CommandOptions::default()
        )
        .is_err());
    }
}
