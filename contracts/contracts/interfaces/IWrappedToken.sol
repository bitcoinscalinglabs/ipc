// SPDX-License-Identifier: MIT OR Apache-2.0
pragma solidity ^0.8.23;

import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {IERC165} from "./IERC165.sol";
import {SubnetID} from "../structs/Subnet.sol";

/// @notice Interface implemented by WrappedToken contracts deployed on non-home subnets.
///         GatewayErcFacet uses ERC165 to detect WrappedTokens and dispatch the burn path.
interface IWrappedToken is IERC165 {
    /// @notice The subnet where the original token is natively deployed.
    function homeSubnet() external view returns (SubnetID memory);

    /// @notice The address of the original token on its home subnet.
    function homeTokenAddress() external view returns (address);

    /// @notice Mint `amount` tokens to `to`. Callable only by the gateway (owner).
    function mint(address to, uint256 amount) external;

    /// @notice Burn `amount` tokens from `from` (requires allowance). Callable by anyone with allowance.
    function burnFrom(address from, uint256 amount) external;
}
