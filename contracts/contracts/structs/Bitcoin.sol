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

struct SlashPreSignature {
    /// @dev The output's txid in internal byte order.
    bytes32 txid;
    uint32 vout;
    /// @dev The 64-byte BIP340 signature of the output's slash transaction.
    bytes signature;
}

/// @dev The most bytes of slash pre-signatures that one checkpoint carries, counted as 40 bytes per output and
/// 67 per signature, upper bounds of their size in the batch-transfer data, plus the payload bytes of the
/// checkpoint's bottom-up messages. 20,000 of the 400,000 weight units of a standard transaction stay for the
/// rest of the batch-transfer transaction.
uint256 constant MAX_SLASH_PRE_SIGNATURE_BYTES = 380_000;

/// @dev The most slash pre-signatures of one output that one checkpoint carries: as many as fit in
/// `MAX_SLASH_PRE_SIGNATURE_BYTES` alone.
uint256 constant MAX_SLASH_PRE_SIGNATURES_PER_OUTPUT = (MAX_SLASH_PRE_SIGNATURE_BYTES - 40) / 67;

/// @dev The slash pre-signatures posted for one output.
struct SlashOutput {
    /// @dev Whether the output waits for a checkpoint to carry its slash pre-signatures.
    bool queued;
    /// @dev The height of the last checkpoint that carried them, 0 if none.
    uint256 carriedHeight;
    /// @dev In posting order, never shortened.
    SlashPreSignature[] signatures;
}

/// @dev One output that a checkpoint carries, with the number of its slash pre-signatures that it carries.
struct CarriedSlashOutput {
    bytes32 output;
    uint32 count;
}
