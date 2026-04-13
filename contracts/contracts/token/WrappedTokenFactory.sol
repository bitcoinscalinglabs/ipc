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

    /// @notice Phase 3 diagnostic: emitted at each step of deployWrappedToken.
    ///         Survives a downstream revert because the factory frame catches the inner revert.
    event FactoryStep(uint8 step, uint256 data);
    /// @notice Captures the raw bytes of an inner revert from `new WrappedToken(...)`.
    event FactoryNewFailed(bytes errBytes);

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
        emit FactoryStep(1, uint256(uint160(homeToken)));
        emit FactoryStep(2, uint256(uint160(msg.sender)));
        emit FactoryStep(3, uint256(uint160(address(this))));

        // CREATE2 with salt = keccak(homeSubnet, homeToken): deterministic per token pair, not nonce-dependent.
        bytes32 salt = keccak256(abi.encode(homeSubnet, homeToken));
        emit FactoryStep(7, uint256(salt));

        WrappedToken token;
        try new WrappedToken{salt: salt}(homeSubnet, homeToken, name, symbol, decimals_) returns (WrappedToken t) {
            token = t;
            emit FactoryStep(4, uint256(uint160(address(t))));
        } catch (bytes memory err) {
            emit FactoryStep(104, err.length);
            emit FactoryNewFailed(err);
            // Re-revert with the original error bytes so callers see the failure as before.
            assembly {
                revert(add(err, 0x20), mload(err))
            }
        }

        // Transfer ownership to the calling gateway so it becomes the sole minter/burner.
        try token.transferOwnership(msg.sender) {
            emit FactoryStep(5, 0);
        } catch (bytes memory err) {
            emit FactoryStep(105, err.length);
            emit FactoryNewFailed(err);
            assembly {
                revert(add(err, 0x20), mload(err))
            }
        }

        emit WrappedTokenDeployed(homeToken, address(token));
        emit FactoryStep(6, 0);
        return address(token);
    }
}
