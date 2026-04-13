// SPDX-License-Identifier: MIT OR Apache-2.0
pragma solidity ^0.8.23;

import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";
import {ERC20Burnable} from "@openzeppelin/contracts/token/ERC20/extensions/ERC20Burnable.sol";
import {Ownable} from "@openzeppelin/contracts/access/Ownable.sol";
import {ERC165} from "@openzeppelin/contracts/utils/introspection/ERC165.sol";
import {IERC165} from "../interfaces/IERC165.sol";
import {IWrappedToken} from "../interfaces/IWrappedToken.sol";
import {SubnetID} from "../structs/Subnet.sol";

/// @notice Wrapped representation of an ERC20 token on a non-home subnet.
///         Deployed by WrappedTokenFactory on first cross-subnet delivery.
///         The deploying gateway becomes the owner and sole minter/burner.
contract WrappedToken is ERC20, ERC20Burnable, Ownable, ERC165, IWrappedToken {
    SubnetID private _homeSubnet;
    address private immutable _homeTokenAddress;
    uint8 private immutable _decimals;

    /// @notice Phase 3 diagnostic markers emitted during construction.
    ///         Survives only if the constructor itself completes successfully
    ///         (events from a reverted CREATE frame are unwound).
    event WTConstructorStep(uint8 step, uint256 data);

    constructor(
        SubnetID memory homeSubnet_,
        address homeTokenAddress_,
        string memory name_,
        string memory symbol_,
        uint8 decimals_
    ) ERC20(name_, symbol_) Ownable(msg.sender) {
        emit WTConstructorStep(1, uint256(uint160(msg.sender)));
        _homeSubnet = homeSubnet_;
        emit WTConstructorStep(2, homeSubnet_.route.length);
        _homeTokenAddress = homeTokenAddress_;
        emit WTConstructorStep(3, uint256(uint160(homeTokenAddress_)));
        _decimals = decimals_;
        emit WTConstructorStep(4, uint256(decimals_));
    }

    function homeSubnet() external view override returns (SubnetID memory) {
        return _homeSubnet;
    }

    function homeTokenAddress() external view override returns (address) {
        return _homeTokenAddress;
    }

    function decimals() public view override returns (uint8) {
        return _decimals;
    }

    /// @notice Mint tokens. Only the gateway (owner) may call this.
    function mint(address to, uint256 amount) external override onlyOwner {
        _mint(to, amount);
    }

    /// @inheritdoc IWrappedToken
    function burnFrom(address from, uint256 amount) public override(ERC20Burnable, IWrappedToken) {
        ERC20Burnable.burnFrom(from, amount);
    }

    function supportsInterface(
        bytes4 interfaceId
    ) public view override(ERC165, IERC165) returns (bool) {
        return interfaceId == type(IWrappedToken).interfaceId || super.supportsInterface(interfaceId);
    }
}
