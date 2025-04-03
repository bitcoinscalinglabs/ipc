// SPDX-License-Identifier: MIT OR Apache-2.0
pragma solidity ^0.8.23;

struct BitcoinCheckpoint {
    /// @dev The base64 of the psbt.
    bytes psbt;
    /// @dev the hex of the batch transfer transaction, if present.
    bytes batchTransferTx;
    /// @dev The list of validators who signed.
    address[] signatories;
    /// @dev The list of signatures.
    mapping(address => bytes) signatures;
}
