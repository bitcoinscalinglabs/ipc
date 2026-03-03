// SPDX-License-Identifier: MIT OR Apache-2.0
pragma solidity ^0.8.23;

/// @title RewardConfig
/// @notice Configuration for IPC emission chain rewards. Stores snapshot parameters and
///         exposes tokensPerSnapshot(snapshot) for the reward schedule.
/// @dev (0, 0) is the sentinel for non-emission chains. Emission configs must have snapshotLength > 0.
///      Future: add scheduleOverride to delegate to a contract with custom schedule logic
///      (e.g. 42M total supply, halving every 200K snapshots).
contract RewardConfig {
    uint64 public immutable activationHeight;
    uint64 public immutable snapshotLength;

    /// @param _activationHeight Bitcoin height at which rewards start
    /// @param _snapshotLength Number of Bitcoin blocks per snapshot
    constructor(uint64 _activationHeight, uint64 _snapshotLength) {
        require(
            _activationHeight == 0 || _snapshotLength > 0,
            "RewardConfig: emission requires snapshot_length > 0"
        );
        activationHeight = _activationHeight;
        snapshotLength = _snapshotLength;
    }

    /// @notice Returns the number of tokens to mint for a given snapshot.
    /// @dev For now returns constant 1000. Future: delegate to scheduleOverride or use segment-based logic.
    /// @param snapshot Snapshot number
    /// @return tokens Number of tokens to distribute in this snapshot
    function tokensPerSnapshot(uint256 snapshot) external pure returns (uint256 tokens) {
        snapshot; // silence unused warning
        return 1000;
    }
}
