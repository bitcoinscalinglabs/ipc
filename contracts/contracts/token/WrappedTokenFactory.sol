// SPDX-License-Identifier: MIT OR Apache-2.0
pragma solidity ^0.8.23;

import {WrappedToken} from "./WrappedToken.sol";
import {SubnetID} from "../structs/Subnet.sol";

/// @notice Deploys WrappedToken contracts on demand during ErcTransfer top-down delivery.
///         Called by the gateway inline inside _executeErcTransfer on first delivery of a token.
contract WrappedTokenFactory {
    event WrappedTokenDeployed(
        address indexed homeToken,
        address indexed wrappedToken
    );

    /// @notice Deploy a new WrappedToken and transfer ownership to the caller (gateway).
    /// @param homeSubnet   The subnet where the original token lives.
    /// @param homeToken    The address of the original token on its home subnet.
    /// @param name         Token name — from TokenMetadata stored by recordTokenMetadata().
    /// @param symbol       Token symbol.
    /// @param decimals_    Token decimals.
    /// @return The address of the newly deployed WrappedToken.
    function deployWrappedToken(
        SubnetID memory homeSubnet,
        address homeToken,
        string memory name,
        string memory symbol,
        uint8 decimals_
    ) external returns (address) {
        // CREATE2 with salt = keccak(homeSubnet, homeToken): deterministic per token pair, not nonce-dependent.
        bytes32 salt = keccak256(abi.encode(homeSubnet, homeToken));

        // Deploy owned by the caller (the gateway) so it is the sole minter/burner.
        WrappedToken token;
        try new WrappedToken{salt: salt}(homeSubnet, homeToken, name, symbol, decimals_, msg.sender) returns (WrappedToken t) {
            token = t;
        } catch (bytes memory err) {
            assembly {
                revert(add(err, 0x20), mload(err))
            }
        }

        emit WrappedTokenDeployed(homeToken, address(token));
        return address(token);
    }
}
