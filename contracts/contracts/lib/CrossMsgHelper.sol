// SPDX-License-Identifier: MIT OR Apache-2.0
pragma solidity ^0.8.23;

import {METHOD_SEND, EMPTY_BYTES} from "../constants/Constants.sol";
import {IpcEnvelope, ResultMsg, CallMsg, IpcMsgKind, OutcomeType} from "../structs/CrossNet.sol";
import {IPCMsgType} from "../enums/IPCMsgType.sol";
import {SubnetID, IPCAddress} from "../structs/Subnet.sol";
import {SubnetIDHelper} from "../lib/SubnetIDHelper.sol";
import {FvmAddressHelper} from "../lib/FvmAddressHelper.sol";
import {FvmAddress} from "../structs/FvmAddress.sol";
import {FilAddress} from "fevmate/contracts/utils/FilAddress.sol";
import {Address} from "@openzeppelin/contracts/utils/Address.sol";
import {Asset} from "../structs/Subnet.sol";
import {AssetHelper} from "./AssetHelper.sol";
import {IIpcHandler} from "../../sdk/interfaces/IIpcHandler.sol";
import {GatewayActorStorage, TokenMetadata, LibGatewayActorStorage} from "../lib/LibGatewayActorStorage.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {IWrappedToken} from "../interfaces/IWrappedToken.sol";
import {WrappedTokenFactory} from "../token/WrappedTokenFactory.sol";
import {TokenMetadataNotFound} from "../errors/IPCErrors.sol";

/// @title Helper library for manipulating IpcEnvelope-related structs
library CrossMsgHelper {
    using SubnetIDHelper for SubnetID;
    using FilAddress for address;
    using FvmAddressHelper for FvmAddress;
    using AssetHelper for Asset;

    /// @notice Phase 2 diagnostic marker. Emitted at each step of _executeErcTransfer
    ///         so that, on revert, the last surviving marker (events emitted by the
    ///         gateway delegatecall context survive a downstream sub-call revert)
    ///         tells us exactly where the failure happened.
    event ErcTransferStep(uint8 step, uint256 data);

    error CannotExecuteEmptyEnvelope();

    function createTransferMsg(
        IPCAddress memory from,
        IPCAddress memory to,
        uint256 value
    ) public pure returns (IpcEnvelope memory) {
        return
            IpcEnvelope({
                kind: IpcMsgKind.Transfer,
                from: from,
                to: to,
                value: value,
                message: EMPTY_BYTES,
                nonce: 0
            });
    }

    function createCallMsg(
        IPCAddress memory from,
        IPCAddress memory to,
        uint256 value,
        bytes4 method,
        bytes memory params
    ) public pure returns (IpcEnvelope memory) {
        CallMsg memory message = CallMsg({method: abi.encodePacked(method), params: params});
        return
            IpcEnvelope({
                kind: IpcMsgKind.Call,
                from: from,
                to: to,
                value: value,
                message: abi.encode(message),
                nonce: 0
            });
    }

    /// @notice Creates a receipt message for the given envelope.
    /// It reverts the from and to to return to the original sender
    /// and identifies the receipt through the hash of the original message.
    function createResultMsg(
        IpcEnvelope calldata crossMsg,
        OutcomeType outcome,
        bytes memory ret
    ) public pure returns (IpcEnvelope memory) {
        ResultMsg memory message = ResultMsg({id: toHash(crossMsg), outcome: outcome, ret: ret});
        uint256 value = crossMsg.value;
        if (outcome == OutcomeType.Ok) {
            // if the message was executed successfully, the value stayed
            // in the subnet and there's no need to return it.
            value = 0;
        }
        return
            IpcEnvelope({
                kind: IpcMsgKind.Result,
                from: crossMsg.to,
                to: crossMsg.from,
                value: value,
                message: abi.encode(message),
                nonce: 0
            });
    }

    function createReleaseMsg(
        SubnetID calldata subnet,
        address signer,
        FvmAddress calldata to,
        uint256 value
    ) public pure returns (IpcEnvelope memory) {
        return
            createTransferMsg(
                IPCAddress({subnetId: subnet, rawAddress: FvmAddressHelper.from(signer)}),
                IPCAddress({subnetId: subnet.getParentSubnet(), rawAddress: to}),
                value
            );
    }

    function createFundMsg(
        SubnetID calldata subnet,
        address signer,
        FvmAddress calldata to,
        uint256 value
    ) public pure returns (IpcEnvelope memory) {
        return
            createTransferMsg(
                IPCAddress({subnetId: subnet.getParentSubnet(), rawAddress: FvmAddressHelper.from(signer)}),
                IPCAddress({subnetId: subnet, rawAddress: to}),
                value
            );
    }

    function applyType(IpcEnvelope calldata message, SubnetID calldata currentSubnet) public pure returns (IPCMsgType) {
        SubnetID memory toSubnet = message.to.subnetId;
        SubnetID memory fromSubnet = message.from.subnetId;
        SubnetID memory currentParentSubnet = currentSubnet.commonParent(toSubnet);
        SubnetID memory messageParentSubnet = fromSubnet.commonParent(toSubnet);

        if (currentParentSubnet.equals(messageParentSubnet)) {
            if (fromSubnet.route.length > messageParentSubnet.route.length) {
                return IPCMsgType.BottomUp;
            }
        }

        return IPCMsgType.TopDown;
    }

    function toHash(IpcEnvelope memory crossMsg) internal pure returns (bytes32) {
        return keccak256(abi.encode(crossMsg));
    }

    function toHash(IpcEnvelope[] memory crossMsgs) public pure returns (bytes32) {
        return keccak256(abi.encode(crossMsgs));
    }

    function isEmpty(IpcEnvelope memory crossMsg) internal pure returns (bool) {
        // envelopes need to necessarily include a message inside except
        // if it is a plain `Transfer`.
        if (crossMsg.kind == IpcMsgKind.Transfer) {
            return crossMsg.value == 0;
        }
        return crossMsg.message.length == 0;
    }

    /// @notice Executes a cross message envelope.
    ///
    /// This function doesn't revert except if the envelope is empty.
    /// It returns a success flag and the return data for the success or
    /// the error so it can be returned to the sender through a cross-message receipt.
    /// NOTE: Execute assumes that the fund it is handling have already been
    /// released for their use so they can be conveniently included in the
    /// forwarded message, or the receipt in the case of failure.
    function execute(
        IpcEnvelope calldata crossMsg,
        Asset memory supplySource
    ) public returns (bool success, bytes memory ret) {
        if (isEmpty(crossMsg)) {
            revert CannotExecuteEmptyEnvelope();
        }

        address recipient = crossMsg.to.rawAddress.extractEvmAddress().normalize();
        if (crossMsg.kind == IpcMsgKind.Transfer) {
            return supplySource.transferFunds({recipient: payable(recipient), value: crossMsg.value});
        } else if (crossMsg.kind == IpcMsgKind.Call || crossMsg.kind == IpcMsgKind.Result) {
            // send the envelope directly to the entrypoint
            // use supplySource so the tokens in the message are handled successfully
            // and by the right supply source
            return
                supplySource.performCall(
                    payable(recipient),
                    abi.encodeCall(IIpcHandler.handleIpcMessage, (crossMsg)),
                    crossMsg.value
                );
        } else if (crossMsg.kind == IpcMsgKind.ErcTransfer) {
            return _executeErcTransfer(crossMsg, recipient);
        } else if (crossMsg.kind == IpcMsgKind.ErcRegistration) {
            _executeErcRegistration(crossMsg);
            return (true, EMPTY_BYTES);
        }
        return (false, EMPTY_BYTES);
    }

    // checks whether the cross messages are sorted in ascending order or not
    function isSorted(IpcEnvelope[] calldata crossMsgs) external pure returns (bool) {
        uint256 prevNonce;
        uint256 length = crossMsgs.length;
        for (uint256 i; i < length; ) {
            uint256 nonce = crossMsgs[i].nonce;

            if (prevNonce >= nonce) {
                // gas-opt: original check: i > 0
                if (i != 0) {
                    return false;
                }
            }

            prevNonce = nonce;
            unchecked {
                ++i;
            }
        }

        return true;
    }

    /// @notice Executes a top-down ErcTransfer message on arrival.
    ///         Case A — arriving at the home subnet: unlocks the original
    ///                  ERC20 tokens.
    ///         Case B — arriving at a non-home subnet: mints WrappedToken,
    ///                  deploying it via the WrappedTokenFactory on first encounter.
    /// @dev Called via delegatecall from LibGateway, so storage access via appStorage() is safe.
    function _executeErcTransfer(IpcEnvelope calldata crossMsg, address recipient)
        internal
        returns (bool ok, bytes memory err)
    {
        GatewayActorStorage storage s = LibGatewayActorStorage.appStorage();
        (SubnetID memory homeSubnet, address homeToken, uint256 amount) =
            abi.decode(crossMsg.message, (SubnetID, address, uint256));
        emit ErcTransferStep(1, uint256(uint160(homeToken)));

        if (s.networkName.equals(homeSubnet)) {
            // Case A: this is the home subnet — release the locked tokens to the recipient.
            emit ErcTransferStep(10, amount);
            try IERC20(homeToken).transfer(recipient, amount) returns (bool) {
                emit ErcTransferStep(11, 0);
                return (true, EMPTY_BYTES);
            } catch (bytes memory e) {
                emit ErcTransferStep(111, e.length);
                return (false, e);
            }
        } else {
            // Case B: non-home subnet — mint a WrappedToken for the recipient.
            emit ErcTransferStep(20, 0);
            bytes32 key = keccak256(abi.encode(homeSubnet, homeToken));
            address wrappedAddr = s.wrappedTokens[key];
            emit ErcTransferStep(21, uint256(uint160(wrappedAddr)));
            if (wrappedAddr == address(0)) {
                TokenMetadata storage meta = s.tokenMetadata[key];
                emit ErcTransferStep(22, bytes(meta.name).length);
                if (bytes(meta.name).length == 0) {
                    emit ErcTransferStep(122, 0);
                    return (false, abi.encodeWithSelector(TokenMetadataNotFound.selector));
                }
                emit ErcTransferStep(23, uint256(uint160(s.wrappedTokenFactory)));
                try WrappedTokenFactory(s.wrappedTokenFactory).deployWrappedToken(
                    homeSubnet, homeToken, meta.name, meta.symbol, meta.decimals
                ) returns (address w) {
                    wrappedAddr = w;
                    emit ErcTransferStep(24, uint256(uint160(w)));
                } catch (bytes memory e) {
                    emit ErcTransferStep(123, e.length);
                    return (false, e);
                }
                s.wrappedTokens[key] = wrappedAddr;
                emit ErcTransferStep(25, 0);
            }
            emit ErcTransferStep(26, amount);
            try IWrappedToken(wrappedAddr).mint(recipient, amount) {
                emit ErcTransferStep(27, 0);
                return (true, EMPTY_BYTES);
            } catch (bytes memory e) {
                emit ErcTransferStep(127, e.length);
                return (false, e);
            }
        }
    }

    /// @notice Executes a top-down ErcRegistration message.
    ///         Stores token metadata so that _executeErcTransfer can deploy WrappedTokens
    ///         on first delivery. First write wins — metadata is immutable once registered.
    /// @dev Called via delegatecall from LibGateway, so storage access via appStorage() is safe.
    ///      Message format: abi.encode(homeSubnet, homeToken, name, symbol, decimals, initialSupply).
    ///      initialSupply is used by the bitcoin-ipc monitor for balance seeding; ignored here.
    function _executeErcRegistration(IpcEnvelope calldata crossMsg) internal {
        GatewayActorStorage storage s = LibGatewayActorStorage.appStorage();
        // Decode 6 fields; initialSupply is ignored by the contract.
        (SubnetID memory homeSubnet, address homeToken, string memory name, string memory symbol, uint8 decimals, ) =
            abi.decode(crossMsg.message, (SubnetID, address, string, string, uint8, uint256));
        bytes32 key = keccak256(abi.encode(homeSubnet, homeToken));
        if (bytes(s.tokenMetadata[key].name).length == 0) {
            s.tokenMetadata[key] = TokenMetadata({name: name, symbol: symbol, decimals: decimals});
        }
    }
}
