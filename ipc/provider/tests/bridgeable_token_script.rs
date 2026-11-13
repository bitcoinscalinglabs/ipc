#![cfg(unix)]

use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Output};

use tempfile::{tempdir, TempDir};

const MOCK_FOUNDRY: &str = r#"#!/usr/bin/env bash
set -euo pipefail
state="$MOCK_STATE"
tool="${0##*/}"
printf '%s %s\n' "$tool" "${1:-}" >> "$state/trace"
actual=$(cat "$state/actual")
visible=$(cat "$state/visible")
requested_nonce="$visible"
args=("$@")
for ((i = 0; i < ${#args[@]}; i++)); do
    if [[ "${args[i]}" == --nonce ]]; then
        requested_nonce=$(( ${args[i + 1]} ))
    fi
done

advance_nonce() {
    if [[ "$MOCK_RACE_PHASE" == "$1" ]] \
        && { [[ "$MOCK_RACE_MODE" == always ]] || [[ ! -e "$state/raced" ]]; }; then
        actual=$((actual + 2))
        echo "$actual" > "$state/actual"
        echo "$actual" > "$state/visible"
        touch "$state/raced"
    fi
    if (( requested_nonce != actual )); then
        echo "expected sequence $actual, got $requested_nonce" >&2
        exit 2
    fi
    if [[ "$MOCK_SEND_FAILURE" == "reject_$1" ]]; then
        echo "execution reverted: transaction rejected" >&2
        exit 2
    fi
    echo "$actual" > "$state/tx_nonce"
    echo "$((actual + 1))" > "$state/actual"
    echo "$1" > "$state/phase"
    echo 0 > "$state/polls"
    if [[ "$MOCK_LAG_PHASE" != "$1" ]]; then
        echo "$((actual + 1))" > "$state/visible"
    fi
    if [[ "$MOCK_SEND_FAILURE" == "transport_$1" ]]; then
        echo "connection reset by peer after submission" >&2
        exit 3
    fi
}

print_nonce() {
    case "$2" in
        decimal) printf '%s\n' "$1" ;;
        hex) printf '0x%x\n' "$1" ;;
        rpc_failure) exit 3 ;;
        *) printf '%s\n' "$2" ;;
    esac
}

if [[ "$tool" == forge ]]; then
    [[ "$1" == create ]]
    advance_nonce deploy
    echo 'Deployed to: 0x1111111111111111111111111111111111111111'
    printf 'Transaction hash: 0x%064d\n' 1
    exit 0
fi

case "$1" in
    wallet) echo '0x2222222222222222222222222222222222222222' ;;
    balance) echo 1000000 ;;
    send)
        advance_nonce register
        printf '0x%064d\n' 2
        ;;
    receipt)
        phase=$(cat "$state/phase")
        [[ "$MOCK_RECEIPT_MODE" != rpc_failure && "$MOCK_RECEIPT_MODE" != "rpc_failure_$phase" ]] || exit 3
        if [[ "$MOCK_RECEIPT_MODE" == "revert_$phase" ]]; then
            printf '%s\n' "$MOCK_FAILURE_STATUS"
        else
            printf '%s\n' "$MOCK_SUCCESS_STATUS"
        fi
        ;;
    tx) print_nonce "$(cat "$state/tx_nonce")" "$MOCK_TX_NONCE_FORMAT" ;;
    nonce)
        polls=$(cat "$state/polls")
        if [[ "$(cat "$state/phase")" == "$MOCK_LAG_PHASE" ]] && (( polls < 2 )); then
            echo "$((polls + 1))" > "$state/polls"
        else
            echo "$actual" > "$state/visible"
        fi
        print_nonce "$(cat "$state/visible")" "$MOCK_ACCOUNT_NONCE_FORMAT"
        ;;
    *) exit 64 ;;
esac
"#;

struct Deployment {
    dir: TempDir,
}

impl Deployment {
    fn new() -> Self {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("bin")).unwrap();
        fs::create_dir_all(root.join("scripts")).unwrap();
        fs::create_dir_all(root.join("contracts/contracts/examples")).unwrap();
        fs::create_dir_all(root.join("state")).unwrap();
        fs::write(root.join("state/actual"), "18\n").unwrap();
        fs::write(root.join("state/visible"), "18\n").unwrap();
        fs::write(root.join("state/phase"), "none\n").unwrap();
        fs::write(root.join("state/polls"), "0\n").unwrap();
        fs::write(
            root.join("contracts/contracts/examples/BridgeableToken.sol"),
            "",
        )
        .unwrap();
        fs::write(
            root.join("scripts/deploy_bridgeable_token.sh"),
            include_str!("../../../scripts/deploy_bridgeable_token.sh"),
        )
        .unwrap();
        for tool in ["cast", "forge", "sleep"] {
            let path = root.join("bin").join(tool);
            let script = if tool == "sleep" {
                "#!/usr/bin/env bash\nexit 0\n"
            } else {
                MOCK_FOUNDRY
            };
            fs::write(&path, script).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        Self { dir }
    }

    fn run(&self, lag_phase: &str, receipt_mode: &str) -> Output {
        self.run_with_env(lag_phase, receipt_mode, &[])
    }

    fn run_with_env(
        &self,
        lag_phase: &str,
        receipt_mode: &str,
        overrides: &[(&str, &str)],
    ) -> Output {
        let root = self.dir.path();
        let mut paths = vec![root.join("bin")];
        paths.extend(env::split_paths(&env::var_os("PATH").unwrap_or_default()));
        Command::new("bash")
            .arg(root.join("scripts/deploy_bridgeable_token.sh"))
            .args([
                "--rpc-url",
                "http://unused.invalid",
                "--private-key",
                "unused-test-key",
                "--gateway",
                "0x3333333333333333333333333333333333333333",
                "--name",
                "TOKEN",
                "--symbol",
                "TOK",
                "--decimals",
                "18",
                "--initial-supply",
                "100",
                "--broadcast",
            ])
            .env("PATH", env::join_paths(paths).unwrap())
            .env("MOCK_STATE", root.join("state"))
            .env("MOCK_LAG_PHASE", lag_phase)
            .env("MOCK_RECEIPT_MODE", receipt_mode)
            .env("MOCK_SUCCESS_STATUS", "true")
            .env("MOCK_FAILURE_STATUS", "false")
            .env("MOCK_TX_NONCE_FORMAT", "decimal")
            .env("MOCK_ACCOUNT_NONCE_FORMAT", "decimal")
            .env("MOCK_RACE_PHASE", "none")
            .env("MOCK_RACE_MODE", "once")
            .env("MOCK_SEND_FAILURE", "none")
            .envs(overrides.iter().copied())
            .output()
            .unwrap()
    }

    fn trace(&self) -> String {
        fs::read_to_string(self.dir.path().join("state/trace")).unwrap()
    }
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn registration_waits_for_the_deployment_nonce_to_be_visible() {
    let deployment = Deployment::new();
    assert_success(&deployment.run("deploy", "success"));
    let trace = deployment.trace();
    let before_send = trace.split("cast send").next().unwrap();
    assert!(before_send.matches("cast nonce").count() >= 3);
}

#[test]
fn consecutive_deployments_wait_for_the_registration_nonce_to_be_visible() {
    let deployment = Deployment::new();
    assert_success(&deployment.run("register", "success"));
    assert_success(&deployment.run("register", "success"));
    assert_eq!(deployment.trace().matches("cast send").count(), 2);
    assert_eq!(
        fs::read_to_string(deployment.dir.path().join("state/actual")).unwrap(),
        "22\n",
    );
}

#[test]
fn checkpoint_nonce_races_retry_only_the_rejected_transaction() {
    for phase in ["deploy", "register"] {
        let deployment = Deployment::new();
        let output = deployment.run_with_env("none", "success", &[("MOCK_RACE_PHASE", phase)]);
        assert_success(&output);
        let trace = deployment.trace();
        assert_eq!(
            trace.matches("forge create").count(),
            1 + usize::from(phase == "deploy"),
        );
        assert_eq!(
            trace.matches("cast send").count(),
            1 + usize::from(phase == "register"),
        );
        assert_eq!(
            fs::read_to_string(deployment.dir.path().join("state/actual")).unwrap(),
            "22\n",
        );
    }
}

#[test]
fn persistent_nonce_contention_has_a_bounded_retry_count() {
    for phase in ["deploy", "register"] {
        let deployment = Deployment::new();
        let output = deployment.run_with_env(
            "none",
            "success",
            &[("MOCK_RACE_PHASE", phase), ("MOCK_RACE_MODE", "always")],
        );
        assert!(!output.status.success());
        let trace = deployment.trace();
        assert_eq!(
            trace.matches("forge create").count(),
            if phase == "deploy" { 5 } else { 1 },
        );
        assert_eq!(
            trace.matches("cast send").count(),
            if phase == "register" { 5 } else { 0 },
        );
        assert!(!String::from_utf8_lossy(&output.stdout)
            .contains("Token registered for cross-subnet bridging."));
    }
}

#[test]
fn non_sequence_errors_do_not_retry_or_redeploy() {
    for phase in ["deploy", "register"] {
        for failure in ["reject", "transport"] {
            let deployment = Deployment::new();
            let mode = format!("{failure}_{phase}");
            let output =
                deployment.run_with_env("none", "success", &[("MOCK_SEND_FAILURE", &mode)]);
            assert!(!output.status.success());
            let trace = deployment.trace();
            assert_eq!(trace.matches("forge create").count(), 1);
            assert_eq!(
                trace.matches("cast send").count(),
                usize::from(phase == "register"),
            );
            assert!(!String::from_utf8_lossy(&output.stdout)
                .contains("Token registered for cross-subnet bridging."));
        }
    }
}

#[test]
fn successful_cast_receipt_formats_are_accepted() {
    for status in [
        "true",
        "1",
        "0x1",
        "1 (success)",
        "0x1 (success)",
        "0x01",
        "0X01",
        " \ttrue\r\n",
        "\t1 (success) \r\n",
    ] {
        let deployment = Deployment::new();
        let output = deployment.run_with_env("none", "success", &[("MOCK_SUCCESS_STATUS", status)]);
        assert_success(&output);
        assert_eq!(deployment.trace().matches("cast receipt").count(), 2);
        assert!(String::from_utf8_lossy(&output.stdout)
            .contains("Token registered for cross-subnet bridging."));
    }
}

#[test]
fn failed_and_malformed_receipt_formats_are_rejected() {
    for status in [
        "false",
        "0",
        "0x0",
        "0 (failed)",
        "0x0 (failed)",
        "false (failed)",
        "0x00",
        "0X00",
        " \tfalse\r\n",
        "",
        "null",
        "unknown",
        "10",
        "0x10",
        "truefalse",
        "true (failed)",
        "1 (failed)",
        "0x1 (failed)",
        "1 unexpected",
        "true\nfalse",
    ] {
        for phase in ["deploy", "register"] {
            let deployment = Deployment::new();
            let output = deployment.run_with_env(
                "none",
                &format!("revert_{phase}"),
                &[("MOCK_FAILURE_STATUS", status)],
            );
            assert!(
                !output.status.success(),
                "{phase} receipt status {status:?} was accepted",
            );
            assert!(String::from_utf8_lossy(&output.stderr).contains("failed (status "));
            let trace = deployment.trace();
            let completed = usize::from(phase == "register");
            assert_eq!(trace.matches("cast send").count(), completed);
            assert_eq!(trace.matches("cast tx").count(), completed);
            assert!(!String::from_utf8_lossy(&output.stdout)
                .contains("Token registered for cross-subnet bridging."));
        }
    }
}

#[test]
fn receipt_rpc_failures_are_rejected_for_both_transactions() {
    for phase in ["deploy", "register"] {
        let deployment = Deployment::new();
        let output = deployment.run("none", &format!("rpc_failure_{phase}"));
        assert!(!output.status.success());
        let trace = deployment.trace();
        let completed = usize::from(phase == "register");
        assert_eq!(trace.matches("cast send").count(), completed);
        assert_eq!(trace.matches("cast tx").count(), completed);
        assert!(!String::from_utf8_lossy(&output.stdout)
            .contains("Token registered for cross-subnet bridging."));
    }
}

#[test]
fn nonce_waits_accept_decimal_and_hex_output() {
    for tx_format in ["decimal", "hex"] {
        for account_format in ["decimal", "hex"] {
            for phase in ["deploy", "register"] {
                let deployment = Deployment::new();
                let overrides = [
                    ("MOCK_TX_NONCE_FORMAT", tx_format),
                    ("MOCK_ACCOUNT_NONCE_FORMAT", account_format),
                ];
                assert_success(&deployment.run_with_env(phase, "success", &overrides));
                assert_success(&deployment.run_with_env(phase, "success", &overrides));
                assert_eq!(deployment.trace().matches("cast send").count(), 2);
            }
        }
    }
}

#[test]
fn invalid_transaction_nonce_output_stops_registration() {
    for nonce in ["rpc_failure", "", "true", "-1", "0x", "18 unexpected"] {
        let deployment = Deployment::new();
        let output = deployment.run_with_env("none", "success", &[("MOCK_TX_NONCE_FORMAT", nonce)]);
        assert!(!output.status.success(), "nonce {nonce:?} was accepted");
        assert!(!deployment.trace().contains("cast send"));
        assert!(!String::from_utf8_lossy(&output.stdout)
            .contains("Token registered for cross-subnet bridging."));
    }
}
