//! Print the RewardToken EVM address (0x + 40 hex chars), can be used for importing into EVM wallets.
//!
//! Run: cargo run -p fendermint_vm_actor_interface --example reward_token_addr

use fendermint_vm_actor_interface::{init::builtin_actor_eth_addr, ipc};

fn main() {
    let addr = builtin_actor_eth_addr(ipc::REWARD_TOKEN_ACTOR_ID);
    println!(
        "RewardToken address (actor ID {}): 0x{}",
        ipc::REWARD_TOKEN_ACTOR_ID,
        hex::encode(addr.0)
    );
}
