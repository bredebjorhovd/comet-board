#[cfg(unix)]
mod unix {
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::Path;
    use std::process::Command;

    fn install_engine(home: &Path) -> std::path::PathBuf {
        let release = home
            .join(".comet-native/app")
            .join(env!("CARGO_PKG_VERSION"));
        std::fs::create_dir_all(&release).unwrap();
        let engine = release.join("comet");
        std::fs::copy(env!("CARGO_BIN_EXE_comet"), &engine).unwrap();
        std::fs::set_permissions(&engine, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink(&release, home.join(".comet-native/app/current")).unwrap();
        engine
    }

    #[test]
    fn managed_headless_refuses_missing_or_mismatched_board_cli_before_engine_boot() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = install_engine(tmp.path());

        let missing = Command::new(&engine)
            .arg("headless")
            .env("HOME", tmp.path())
            .output()
            .unwrap();
        assert!(!missing.status.success());
        let stderr = String::from_utf8_lossy(&missing.stderr);
        assert!(stderr.contains("comet-board"), "{stderr}");
        assert!(stderr.contains("refusing to start"), "{stderr}");

        let board = engine.parent().unwrap().join("comet-board");
        std::fs::write(&board, "#!/bin/sh\necho 'comet-board 0.0.0'\n").unwrap();
        std::fs::set_permissions(&board, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mismatched = Command::new(&engine)
            .arg("headless")
            .env("HOME", tmp.path())
            .output()
            .unwrap();
        assert!(!mismatched.status.success());
        let stderr = String::from_utf8_lossy(&mismatched.stderr);
        assert!(stderr.contains("comet-board 0.0.0"), "{stderr}");
        assert!(
            stderr.contains(&format!("comet-board {}", env!("CARGO_PKG_VERSION"))),
            "{stderr}"
        );
    }
}
