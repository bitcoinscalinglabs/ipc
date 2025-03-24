// SPDX-License-Identifier: MIT OR Apache-2.0
pragma solidity ^0.8.23;

import {GatewayActorModifiers} from "../../lib/LibGatewayActorStorage.sol";
import {BottomUpCheckpoint} from "../../structs/CrossNet.sol";
import {LibGateway} from "../../lib/LibGateway.sol";
import {LibQuorum} from "../../lib/LibQuorum.sol";
import {Subnet} from "../../structs/Subnet.sol";
import {BitcoinCheckpoint} from "../../structs/Bitcoin.sol";
import {QuorumObjKind} from "../../structs/Quorum.sol";
import {Address} from "@openzeppelin/contracts/utils/Address.sol";

import {InvalidBatchSource, NotEnoughBalance, MaxMsgsPerBatchExceeded, InvalidCheckpointSource, CheckpointAlreadyExists, NotAuthorized, SignatureReplay} from "../../errors/IPCErrors.sol";
import {NotRegisteredSubnet, SubnetNotActive, SubnetNotFound, InvalidSubnet, CheckpointNotCreated} from "../../errors/IPCErrors.sol";
import {BatchNotCreated, InvalidBatchEpoch, BatchAlreadyExists, NotEnoughSubnetCircSupply, InvalidCheckpointEpoch} from "../../errors/IPCErrors.sol";

import {CrossMsgHelper} from "../../lib/CrossMsgHelper.sol";
import {IpcEnvelope, SubnetID} from "../../structs/CrossNet.sol";
import {SubnetIDHelper} from "../../lib/SubnetIDHelper.sol";

import {ActivityRollupRecorded, FullActivityRollup} from "../../structs/Activity.sol";

import {EnumerableSet} from "@openzeppelin/contracts/utils/structs/EnumerableSet.sol";

contract CheckpointingFacet is GatewayActorModifiers {
    using SubnetIDHelper for SubnetID;
    using CrossMsgHelper for IpcEnvelope;
    using EnumerableSet for EnumerableSet.AddressSet;

    /// @notice submit a verified checkpoint in the gateway to trigger side-effects.
    /// @dev this method is called by the corresponding subnet actor.
    ///     Called from a subnet actor if the checkpoint is cryptographically valid.
    /// @param checkpoint The bottom-up checkpoint to be committed.
    function commitCheckpoint(BottomUpCheckpoint calldata checkpoint) external {
        // checkpoint is used to implement access control
        if (checkpoint.subnetID.getActor() != msg.sender) {
            revert InvalidCheckpointSource();
        }
        (bool subnetExists, Subnet storage subnet) = LibGateway.getSubnet(msg.sender);
        if (!subnetExists) {
            revert SubnetNotFound();
        }
        if (!checkpoint.subnetID.equals(subnet.id)) {
            revert InvalidSubnet();
        }

        LibGateway.checkMsgLength(checkpoint.msgs);

        execBottomUpMsgs(checkpoint.msgs, subnet);
    }

    /// @notice creates a new bottom-up checkpoint
    /// @param checkpoint - a bottom-up checkpoint
    /// @param membershipRootHash - a root hash of the Merkle tree built from the validator public keys and their weight
    /// @param membershipWeight - the total weight of the membership
    /// @param activity - the full activity rollup
    function createBottomUpCheckpoint(
        BottomUpCheckpoint calldata checkpoint,
        bytes32 membershipRootHash,
        uint256 membershipWeight,
        FullActivityRollup calldata activity
    ) external systemActorOnly {
        if (LibGateway.bottomUpCheckpointExists(checkpoint.blockHeight)) {
            revert CheckpointAlreadyExists();
        }

        LibQuorum.createQuorumInfo({
            self: s.checkpointQuorumMap,
            objHeight: checkpoint.blockHeight,
            objHash: keccak256(abi.encode(checkpoint)),
            membershipRootHash: membershipRootHash,
            membershipWeight: membershipWeight,
            majorityPercentage: s.majorityPercentage
        });

        LibGateway.storeBottomUpCheckpoint(checkpoint);

        emit ActivityRollupRecorded(uint64(checkpoint.blockHeight), activity);
    }

    /// @notice Set a new checkpoint retention height and garbage collect all checkpoints in range [`retentionHeight`, `newRetentionHeight`)
    /// @dev `retentionHeight` is the height of the first incomplete checkpointswe must keep to implement checkpointing.
    /// All checkpoints with a height less than `retentionHeight` are removed from the history, assuming they are committed to the parent.
    /// @param newRetentionHeight - the height of the oldest checkpoint to keep
    function pruneBottomUpCheckpoints(uint256 newRetentionHeight) external systemActorOnly {
        // we need to clean manually the checkpoints because Solidity does not support passing
        // a storage variable as an interface (so we can iterate and remove directly inside pruneQuorums)
        for (uint256 h = s.checkpointQuorumMap.retentionHeight; h < newRetentionHeight; ) {
            delete s.bottomUpCheckpoints[h];
            delete s.bottomUpMsgBatches[h];
            // Delete each BitcoinCheckpoint using the tracked hashes
            bytes32[] storage hashes = s.bitcoinCheckpointPsbtHashes[h];
            for (uint i = 0; i < hashes.length; i++) {
                delete s.bitcoinCheckpoints[h][hashes[i]];
            }
            delete s.bitcoinCheckpointPsbtHashes[h];
            unchecked {
                ++h;
            }
        }

        LibQuorum.pruneQuorums(s.checkpointQuorumMap, newRetentionHeight);
    }

    /// @notice checks whether the provided checkpoint signature for the block at height `height` is valid and accumulates that it
    /// @dev If adding the signature leads to reaching the threshold, then the checkpoint is removed from `incompleteCheckpoints`
    /// @param height - the height of the block in the checkpoint
    /// @param membershipProof - a Merkle proof that the validator was in the membership at height `height` with weight `weight`
    /// @param weight - the weight of the validator
    /// @param signature - the signature of the checkpoint
    function addCheckpointSignature(
        uint256 height,
        bytes32[] memory membershipProof,
        uint256 weight,
        bytes memory signature
    ) external {
        // check if the checkpoint was already pruned before getting checkpoint
        // and triggering the signature
        LibQuorum.isHeightAlreadyProcessed(s.checkpointQuorumMap, height);

        // slither-disable-next-line unused-return
        (bool exists, ) = LibGateway.getBottomUpCheckpoint(height);
        if (!exists) {
            revert CheckpointNotCreated();
        }
        LibQuorum.addQuorumSignature({
            self: s.checkpointQuorumMap,
            height: height,
            membershipProof: membershipProof,
            weight: weight,
            signature: signature
        });

        // TODO(themis): store btc signature
    }

    function addBitcoinCheckpointSignature(
        uint256 height,
        bytes calldata psbt,
        bytes calldata signatures,
        bytes calldata batchTransferTx
    ) external {
        // check if the checkpoint was already pruned before getting checkpoint
        // and triggering the signature
        LibQuorum.isHeightAlreadyProcessed(s.checkpointQuorumMap, height);

        // slither-disable-next-line unused-return
        (bool exists, ) = LibGateway.getBottomUpCheckpoint(height);
        if (!exists) {
            revert CheckpointNotCreated();
        }

        address signatory = msg.sender;

        // Check if the signatory was included in the quorum signature senders
        bool ok = s.checkpointQuorumMap.quorumSignatureSenders[height].contains(signatory);
        if (!ok) {
            revert NotAuthorized(signatory);
        }

        bytes32 psbtHash = keccak256(psbt);

        // Check if this is a new PSBT hash for this height
        bool hashExists = false;
        bytes32[] storage hashes = s.bitcoinCheckpointPsbtHashes[height];
        for (uint i = 0; i < hashes.length; i++) {
            if (hashes[i] == psbtHash) {
                hashExists = true;
                break;
            }
        }
        if (!hashExists) {
            s.bitcoinCheckpointPsbtHashes[height].push(psbtHash);
        }

        BitcoinCheckpoint storage btcCheckpoint = s.bitcoinCheckpoints[height][psbtHash];
        // Set the PSBT if it hasn't been set yet
        if (btcCheckpoint.psbt.length == 0) {
            btcCheckpoint.psbt = psbt;
        }

        // Set the batch transfer transaction if provided
        if (batchTransferTx.length > 0 && btcCheckpoint.batchTransferTx.length == 0) {
            btcCheckpoint.batchTransferTx = batchTransferTx;
        }

        // Check if sender is already a signatory
        bool alreadySigned = false;
        for (uint i = 0; i < btcCheckpoint.signatories.length; i++) {
            if (btcCheckpoint.signatories[i] == signatory) {
                alreadySigned = true;
                break;
            }
        }

        // Add the signature to the Bitcoin checkpoint if not already a signatory
        if (alreadySigned) {
            revert SignatureReplay();
        } else {
            btcCheckpoint.signatories.push(signatory);
            btcCheckpoint.signatures[signatory] = signatures;
        }
    }

    /// @notice Get the Bitcoin checkpoint with the most signatures for a specific height.
    /// @param height - The height of the block in the checkpoint.
    /// @return psbt - The base64 of the PSBT with the most signatures.
    /// @return batchTransferTx - The hex of the batch transfer transaction, if present.
    /// @return signatories - The list of validators who signed.
    /// @return signatures - The list of signatures corresponding to the signatories.
    function getBitcoinCheckpointSignatures(
        uint256 height
    )
        external
        view
        returns (
            bytes memory psbt,
            bytes memory batchTransferTx,
            address[] memory signatories,
            bytes[] memory signatures
        )
    {
        // Check if the checkpoint exists
        (bool exists, ) = LibGateway.getBottomUpCheckpoint(height);
        if (!exists) {
            revert CheckpointNotCreated();
        }

        bytes32[] storage hashes = s.bitcoinCheckpointPsbtHashes[height];
        if (hashes.length == 0) {
            // No Bitcoin checkpoints found for this height
            return (new bytes(0), new bytes(0), new address[](0), new bytes[](0));
        }

        // Find the checkpoint with the most signatures
        bytes32 mostSignedPsbtHash;
        uint256 maxSignatures = 0;

        for (uint256 i = 0; i < hashes.length; i++) {
            bytes32 currentHash = hashes[i];
            BitcoinCheckpoint storage currentCheckpoint = s.bitcoinCheckpoints[height][currentHash];

            uint256 signatureCount = currentCheckpoint.signatories.length;

            // When multiple PSBTs have the same number of signatures,
            if (signatureCount > maxSignatures) {
                maxSignatures = signatureCount;
                mostSignedPsbtHash = currentHash;
            }
        }

        // Get the checkpoint with the most signatures
        BitcoinCheckpoint storage btcCheckpoint = s.bitcoinCheckpoints[height][mostSignedPsbtHash];

        // Return the PSBT and batch transfer tx
        psbt = btcCheckpoint.psbt;
        batchTransferTx = btcCheckpoint.batchTransferTx;

        // Return signatories
        signatories = btcCheckpoint.signatories;

        // Map signatories to their signatures
        signatures = new bytes[](signatories.length);
        for (uint256 i = 0; i < signatories.length; i++) {
            signatures[i] = btcCheckpoint.signatures[signatories[i]];
        }

        return (psbt, batchTransferTx, signatories, signatures);
    }

    /// @notice submit a batch of cross-net messages for execution.
    /// @param msgs The batch of bottom-up cross-network messages to be executed.
    function execBottomUpMsgs(IpcEnvelope[] calldata msgs, Subnet storage subnet) internal {
        uint256 totalValue;
        uint256 crossMsgLength = msgs.length;

        for (uint256 i; i < crossMsgLength; ) {
            totalValue += msgs[i].value;
            unchecked {
                ++i;
            }
        }

        uint256 totalAmount = totalValue;

        if (subnet.circSupply < totalAmount) {
            revert NotEnoughSubnetCircSupply();
        }

        subnet.circSupply -= totalAmount;

        // execute cross-messages
        LibGateway.applyMessages(subnet.id, msgs);
    }
}
