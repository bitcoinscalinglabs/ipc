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

/// @dev A slash pre-signature of one member for one collateral output.
struct SlashPreSignature {
    /// @dev The output's txid in internal byte order.
    bytes32 txid;
    uint32 vout;
    /// @dev The signer's index in the ascending key order of the configuration the output names.
    uint16 index;
    /// @dev The 64-byte BIP340 signature.
    bytes signature;
}
