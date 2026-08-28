// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

/// @notice ENSIP-10 wildcard resolution.
///
/// @dev A client that fails to find a resolver for the full name strips the
///      leftmost label and asks again, until something answers. Whatever it
///      finds receives the ORIGINAL, complete name — which is what lets one
///      resolver at `handles.link` serve every name beneath it without a
///      registry entry per user.
///
///      Declared here rather than vendored, the way `IIdentityVerifier` and
///      `INotary` are: one function does not justify a dependency.
interface IExtendedResolver {
    /// @param name DNS wire format, e.g. `\x05alice\x01x\x07handles\x04link\x00`.
    /// @param data The resolution call the client wanted to make, such as
    ///             `addr(node, coinType)`.
    function resolve(bytes calldata name, bytes calldata data) external view returns (bytes memory);
}
