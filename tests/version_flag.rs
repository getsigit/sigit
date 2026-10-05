//! `sigit --version` answers and exits without starting a session.

use std::process::{Command, Stdio};

fn version_output(flag: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_sigit"))
        .arg(flag)
        // A closed stdin: a build that mistook the flag for a session would
        // sit in ACP mode reading it.
        .stdin(Stdio::null())
        .output()
        .expect("sigit runs")
}

#[test]
fn version_flag_prints_the_release_and_exits() {
    for flag in ["--version", "-V"] {
        let output = version_output(flag);

        assert!(output.status.success(), "{flag} exits 0");
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            format!("sigit {}\n", env!("CARGO_PKG_VERSION")),
            "{flag} prints the release on stdout"
        );
        assert!(
            output.stderr.is_empty(),
            "{flag} logs nothing: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
