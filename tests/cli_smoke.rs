//! Smoke tests black-box do binário `zcode-cli`.
//!
//! Não exigem runtime ZCode ao vivo: spawna o binário compilado (via
//! `CARGO_BIN_EXE_zcode-cli`) com `std::process::Command`. Cobrem o mínimo
//! de contrato de CLI que deve valer em qualquer máquina de CI.

use std::process::Command;

/// Caminho do binário embutido no build de testes (funciona em Windows e Unix).
fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_zcode-cli"))
}

#[test]
fn version_exit_0_e_imprime_versao() {
    let out = bin()
        .arg("--version")
        .output()
        .expect("spawn --version deve funcionar");
    assert!(
        out.status.success(),
        "exit 0 esperado; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(env!("CARGO_PKG_VERSION")),
        "stdout deve conter a versao do pacote: {stdout}"
    );
}

#[test]
fn help_exit_0_e_menciona_doctor() {
    let out = bin()
        .arg("--help")
        .output()
        .expect("spawn --help deve funcionar");
    assert!(
        out.status.success(),
        "exit 0 esperado; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("doctor"),
        "help deve mencionar o subcomando doctor: {stdout}"
    );
}

/// `doctor --json` sem ZCode instalado: exit 0 (com JSON) OU exit 1 (com
/// stderr preenchido). Resiliente aos dois caminhos — CI e dev local podem
/// ter ou não o desktop ZCode presente.
#[test]
fn doctor_json_exit_0_ou_1_e_saida_coerente() {
    let out = bin()
        .args(["doctor", "--json"])
        .output()
        .expect("spawn doctor --json deve funcionar");
    let code = out.status.code().expect("exit code disponivel");
    assert!(
        code == 0 || code == 1,
        "exit 0 ou 1 esperado, obtido {code}; stdout: {} stderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    if code == 0 {
        // exit 0 => stdout deve ser JSON valido (pretty-print do doctor).
        let ok = serde_json::from_str::<serde_json::Value>(&stdout).is_ok()
            || stdout
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .is_some_and(|l| serde_json::from_str::<serde_json::Value>(l).is_ok());
        assert!(ok, "exit 0 deve ter stdout JSON valido: {stdout}");
    } else {
        // exit 1 => stderr deve explicar a falha (há checks FALHA).
        assert!(
            !out.stderr.is_empty(),
            "exit 1 deve ter stderr com conteudo; stdout: {stdout}"
        );
    }
}
