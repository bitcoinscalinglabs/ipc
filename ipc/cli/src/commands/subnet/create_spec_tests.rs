use super::*;
use clap::Parser;

#[derive(Debug, Parser)]
struct Command {
    #[command(flatten)]
    args: CreateSubnetArgs,
}

fn arguments() -> Vec<String> {
    [
        "create",
        "--parent",
        "/b4",
        "--min-validators",
        "4",
        "--bottomup-check-period",
        "30",
        "btc",
        "--min-validator-stake",
        "100000",
        "--power-unit-exponent",
        "4",
        "--validator-whitelist",
        "key1,key2,key3,key4",
        "--epoch-length",
        "144",
        "--min-timeout",
        "1",
        "--max-timeout",
        "128",
        "--min-deposit",
        "330",
        "--kill-vote-period",
        "36",
        "--kill-delay-period",
        "5",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

// ipc-cli.md, Create a subnet: the btc subcommand carries all seven epoch parameter flags.
#[test]
fn btc_create_parses_every_epoch_parameter_in_the_documented_position() {
    let parsed = Command::try_parse_from(arguments()).unwrap();
    assert_eq!(parsed.args.parent, "/b4");
    assert_eq!(parsed.args.min_validators, 4);
    assert_eq!(parsed.args.bottomup_check_period, 30);
    let SpecifiedNetwork::Btc(btc) = parsed.args.network_specific else {
        panic!("expected btc")
    };
    assert_eq!(btc.min_validator_stake, 100000);
    assert_eq!(btc.power_unit_exponent, 4);
    assert_eq!(btc.validator_whitelist, "key1,key2,key3,key4");
    assert_eq!(btc.epoch_length, 144);
    assert_eq!((btc.min_timeout, btc.max_timeout), (1, 128));
    assert_eq!(btc.min_deposit, 330);
    assert_eq!((btc.kill_vote_period, btc.kill_delay_period), (36, 5));
    assert_eq!(btc.min_cross_msg_fee, 1);
}

// ipc-cli.md, Create a subnet: none of the seven epoch parameter flags has a default.
#[test]
fn btc_create_requires_each_epoch_parameter_explicitly() {
    for flag in [
        "--power-unit-exponent",
        "--epoch-length",
        "--min-timeout",
        "--max-timeout",
        "--min-deposit",
        "--kill-vote-period",
        "--kill-delay-period",
    ] {
        let mut args = arguments();
        let i = args.iter().position(|s| s == flag).unwrap();
        args.drain(i..i + 2);
        assert!(Command::try_parse_from(args).is_err(), "missing {flag}");
    }
}

// ipc-cli.md, Create a subnet / create-subnet.md, Transaction format: epoch parameters keep their unsigned wire widths.
#[test]
fn btc_create_rejects_negative_and_overwide_epoch_values() {
    for (flag, bad) in [
        ("--power-unit-exponent", "256"),
        ("--epoch-length", "18446744073709551616"),
        ("--min-timeout", "4294967296"),
        ("--max-timeout", "4294967296"),
        ("--min-deposit", "-1"),
        ("--kill-vote-period", "-1"),
        ("--kill-delay-period", "-1"),
    ] {
        let mut args = arguments();
        let i = args.iter().position(|s| s == flag).unwrap();
        args[i + 1] = bad.into();
        assert!(Command::try_parse_from(args).is_err(), "{flag} {bad}");
    }
}
