// SPDX-License-Identifier: MIT OR Apache-2.0
pragma solidity ^0.8.23;

import {GatewayActorModifiers} from "../../lib/LibGatewayActorStorage.sol";
import {BottomUpCheckpoint, BottomUpMsgBatch} from "../../structs/CrossNet.sol";
import {LibGateway} from "../../lib/LibGateway.sol";
import {LibQuorum} from "../../lib/LibQuorum.sol";
import {Subnet} from "../../structs/Subnet.sol";
import {BitcoinCheckpoint, SlashPreSignature, SlashOutput, CarriedSlashOutput, MAX_SLASH_PRE_SIGNATURE_BYTES, MAX_CONFIGURATION_SIZE} from "../../structs/Bitcoin.sol";
import {QuorumObjKind} from "../../structs/Quorum.sol";
import {Address} from "@openzeppelin/contracts/utils/Address.sol";

import {InvalidBatchSource, NotEnoughBalance, MaxMsgsPerBatchExceeded, InvalidCheckpointSource, CheckpointAlreadyExists, NotAuthorized, SignatureReplay, InvalidSignature, InvalidSignatureLength} from "../../errors/IPCErrors.sol";
import {NotRegisteredSubnet, SubnetNotActive, SubnetNotFound, InvalidSubnet, CheckpointNotCreated} from "../../errors/IPCErrors.sol";
import {BatchNotCreated, InvalidBatchEpoch, BatchAlreadyExists, NotEnoughSubnetCircSupply, InvalidCheckpointEpoch} from "../../errors/IPCErrors.sol";

import {CrossMsgHelper} from "../../lib/CrossMsgHelper.sol";
import {IpcEnvelope, SubnetID} from "../../structs/CrossNet.sol";
import {SubnetIDHelper} from "../../lib/SubnetIDHelper.sol";

import {ActivityRollupRecorded, FullActivityRollup} from "../../structs/Activity.sol";

import {EnumerableSet} from "@openzeppelin/contracts/utils/structs/EnumerableSet.sol";

event BitcoinCheckpointAdded(uint256 indexed height, address indexed signatory, bytes32 indexed psbtHash);
event SubnetFinalHeightSet(uint256 height);
event SlashPreSignatureAdded(bytes32 indexed txid, uint32 vout, uint16 index, address indexed signatory);

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
    }

    /// @param key - The signer's x-only Bitcoin key, which the provider uses to place its signatures.
    function addBitcoinCheckpointSignature(
        uint256 height,
        bytes calldata psbt,
        bytes calldata signatures,
        bytes calldata batchTransferTx,
        bytes32 key
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
        // bool ok = s.checkpointQuorumMap.quorumSignatureSenders[height].contains(signatory);
        // if (!ok) {
        //     revert NotAuthorized(signatory);
        // }

        bytes32 psbtHash = keccak256(psbt);

        // Check if this is a new PSBT hash for this height
        {
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
            btcCheckpoint.keys[signatory] = key;
            emit BitcoinCheckpointAdded(height, signatory, psbtHash);
        }
    }

    /// @notice Stored unverified; one per (txid, vout, index). `power` is the signer's power and `threshold`
    /// the threshold of the configuration that the output's slash leaf names. Once the posted powers of an
    /// output reach the threshold, the next checkpoint carries all its slash pre-signatures.
    function addSlashPreSignature(
        bytes32 txid,
        uint32 vout,
        uint16 index,
        uint64 power,
        uint64 threshold,
        bytes calldata signature
    ) external {
        if (signature.length != 64) {
            revert InvalidSignatureLength();
        }
        if (index >= MAX_CONFIGURATION_SIZE || threshold == 0) {
            revert InvalidSignature();
        }
        bytes32 posted = keccak256(abi.encode(txid, vout, index));
        if (s.slashPreSignaturePosted[posted]) {
            revert SignatureReplay();
        }
        s.slashPreSignaturePosted[posted] = true;

        bytes32 key = keccak256(abi.encode(txid, vout, threshold));
        SlashOutput storage output = s.slashOutputs[key];
        output.threshold = threshold;
        output.signatures.push(SlashPreSignature({txid: txid, vout: vout, index: index, signature: signature}));
        output.power += power;
        if (output.power >= threshold && !output.queued) {
            output.queued = true;
            s.slashOutputsToCarry.push(key);
        }
        emit SlashPreSignatureAdded(txid, vout, index, msg.sender);
    }

    /// @notice Called at the creation of the checkpoint at `height`. It carries the outputs waiting in
    /// `slashOutputsToCarry`, in order, each with all its slash pre-signatures, as many outputs as
    /// `MAX_SLASH_PRE_SIGNATURE_BYTES` allows. `supplyAdjustmentBytes` is the payload size of the
    /// supply adjustments that the checkpoint carries after its bottom-up messages. Returns the
    /// number of slash pre-signatures carried.
    function carrySlashPreSignatures(
        uint256 height,
        uint256 supplyAdjustmentBytes
    ) external systemActorOnly returns (uint64 count) {
        // the payloads of the checkpoint's messages (token names and symbols) share the same transaction
        uint256 size = supplyAdjustmentBytes;
        (, BottomUpMsgBatch storage batch) = LibGateway.getBottomUpMsgBatch(height);
        for (uint256 j; j < batch.msgs.length; ++j) {
            size += batch.msgs[j].message.length;
        }
        uint256 i = s.slashOutputsToCarryStart;
        uint256 end = s.slashOutputsToCarry.length;
        for (; i < end; ++i) {
            bytes32 key = s.slashOutputsToCarry[i];
            SlashOutput storage output = s.slashOutputs[key];
            uint256 n = output.signatures.length;
            size += 40 + 67 * n;
            if (size > MAX_SLASH_PRE_SIGNATURE_BYTES) {
                break;
            }
            output.queued = false;
            output.carriedHeight = height;
            s.slashOutputsCarried[height].push(CarriedSlashOutput({output: key, count: uint32(n)}));
            count += uint64(n);
        }
        s.slashOutputsToCarryStart = i;
    }

    /// @notice The slash pre-signatures that the checkpoint at `height` carries.
    function getSlashPreSignatures(uint256 height) external view returns (SlashPreSignature[] memory signatures) {
        CarriedSlashOutput[] storage carried = s.slashOutputsCarried[height];
        uint256 total;
        for (uint256 i; i < carried.length; ++i) {
            total += carried[i].count;
        }
        signatures = new SlashPreSignature[](total);
        uint256 k;
        for (uint256 i; i < carried.length; ++i) {
            SlashPreSignature[] storage posted = s.slashOutputs[carried[i].output].signatures;
            for (uint256 j; j < carried[i].count; ++j) {
                signatures[k++] = posted[j];
            }
        }
    }

    /// @notice The posted powers of an output with `threshold`, the height of the last checkpoint that carried
    /// its slash pre-signatures (0 if none), and whether `index` has posted for the output.
    function getSlashOutput(
        bytes32 txid,
        uint32 vout,
        uint64 threshold,
        uint16 index
    ) external view returns (uint256 power, uint256 carriedHeight, bool posted) {
        SlashOutput storage output = s.slashOutputs[keccak256(abi.encode(txid, vout, threshold))];
        return (output.power, output.carriedHeight, s.slashPreSignaturePosted[keccak256(abi.encode(txid, vout, index))]);
    }

    /// @notice Get the Bitcoin checkpoint with the most signer weight (as verified at signing) at a height.
    /// @param height - The height of the block in the checkpoint.
    /// @return psbt - The base64 of the PSBT with the most weight.
    /// @return batchTransferTx - The hex of the batch transfer transaction, if present.
    /// @return signatories - The list of validators who signed.
    /// @return signatures - The list of signatures corresponding to the signatories.
    /// @return keys - The x-only Bitcoin keys that the signatories posted.
    function getBitcoinCheckpointSignatures(
        uint256 height
    )
        external
        view
        returns (
            bytes memory psbt,
            bytes memory batchTransferTx,
            address[] memory signatories,
            bytes[] memory signatures,
            bytes32[] memory keys
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
            return (new bytes(0), new bytes(0), new address[](0), new bytes[](0), new bytes32[](0));
        }

        // Ties keep the first.
        bytes32 mostWeightPsbtHash = hashes[0];
        uint256 maxWeight = signersWeight(height, hashes[0]);

        for (uint256 i = 1; i < hashes.length; i++) {
            uint256 weight = signersWeight(height, hashes[i]);
            if (weight > maxWeight) {
                maxWeight = weight;
                mostWeightPsbtHash = hashes[i];
            }
        }

        BitcoinCheckpoint storage btcCheckpoint = s.bitcoinCheckpoints[height][mostWeightPsbtHash];

        // Return the PSBT and batch transfer tx
        psbt = btcCheckpoint.psbt;
        batchTransferTx = btcCheckpoint.batchTransferTx;

        // Return signatories
        signatories = btcCheckpoint.signatories;

        // Map signatories to their signatures and keys
        signatures = new bytes[](signatories.length);
        keys = new bytes32[](signatories.length);
        for (uint256 i = 0; i < signatories.length; i++) {
            signatures[i] = btcCheckpoint.signatures[signatories[i]];
            keys[i] = btcCheckpoint.keys[signatories[i]];
        }

        return (psbt, batchTransferTx, signatories, signatures, keys);
    }

    function hasBitcoinCheckpointSignature(uint256 height, address signatory) external view returns (bool) {
        bytes32[] storage hashes = s.bitcoinCheckpointPsbtHashes[height];
        for (uint256 i = 0; i < hashes.length; i++) {
            address[] storage signatories = s.bitcoinCheckpoints[height][hashes[i]].signatories;
            for (uint256 j = 0; j < signatories.length; j++) {
                if (signatories[j] == signatory) {
                    return true;
                }
            }
        }
        return false;
    }

    function signersWeight(uint256 height, bytes32 psbtHash) internal view returns (uint256 weight) {
        address[] storage signatories = s.bitcoinCheckpoints[height][psbtHash].signatories;
        for (uint256 i = 0; i < signatories.length; i++) {
            weight += s.checkpointQuorumMap.quorumSignerWeights[height][signatories[i]];
        }
    }

    function setSubnetFinalHeight(uint256 height) external systemActorOnly {
        if (height <= s.subnetFinalHeight) {
            return;
        }
        s.subnetFinalHeight = height;
        emit SubnetFinalHeightSet(height);
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
