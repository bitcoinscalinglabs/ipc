use super::*;
use clap::Parser;

#[derive(Debug, Parser)]
struct Command {
    #[command(flatten)]
    args: JoinUeArgs,
}

fn arguments() -> Vec<String> {
    [
        "join-ue",
        "--subnet",
        "/b4/test",
        "--collateral",
        "100000",
        "--ip",
        "192.0.2.1:8080",
        "--backup-address",
        "bcrt1pbackup",
        "--epochs",
        "128",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

// ipc-cli.md, Join: --epochs is a required count of seating epochs.
#[test]
fn join_ue_requires_and_preserves_the_epoch_count() {
    let parsed = Command::try_parse_from(arguments()).unwrap();
    assert_eq!(parsed.args.epochs, 128);
    let mut missing = arguments();
    missing.truncate(missing.len() - 2);
    assert!(Command::try_parse_from(missing).is_err());
}

// ipc-cli.md, Join / join.md, Creating 5: the epoch count is an unsigned u32.
#[test]
fn join_ue_rejects_negative_and_overwide_epoch_counts() {
    for bad in ["-1", "4294967296"] {
        let mut args = arguments();
        *args.last_mut().unwrap() = bad.into();
        assert!(Command::try_parse_from(args).is_err());
    }
}

// ipc-cli.md, Join: the old --lock-blocks option is replaced by --epochs.
#[test]
fn join_ue_does_not_accept_the_obsolete_block_lock_option() {
    let mut args = arguments();
    let i = args.len() - 2;
    args[i] = "--lock-blocks".into();
    assert!(Command::try_parse_from(args).is_err());
}
