// SPDX-License-Identifier: MIT OR Apache-2.0
pragma solidity ^0.8.20;

/// @notice Test double for the indexer: the exact event surface of
///         HandleEscrow, each behind a function that just emits it, a
///         `depositToHandle`'s two events in the order it emits them, and the
///         `registry` view the indexer reads at startup. The integration test
///         deploys this on anvil beside MockIdentityRegistry, so the indexer
///         decodes real ABI-encoded logs from two emitters.
///
///         Event signatures are copied verbatim from
///         libid-contracts/solidity/contracts/escrow/HandleEscrow.sol; the
///         hashed-identities suite checks every topic against the crate's
///         bindings.
contract MockHandleEscrow {
    address public immutable registry;

    constructor(address registry_) {
        registry = registry_;
    }

    event Deposited(
        bytes32 indexed handleNode,
        address indexed token,
        address indexed refundTo,
        address depositor,
        uint256 round,
        uint256 amount
    );
    event HandleRevealed(address indexed by, bytes32 indexed platformId, bytes32 indexed handleNode, string handle);
    event Forwarded(
        bytes32 indexed handleNode,
        address indexed token,
        address indexed depositor,
        address holder,
        uint256 amount,
        uint256 received
    );
    event Claimed(
        bytes32 indexed handleNode,
        address indexed token,
        address indexed claimer,
        address recipient,
        uint256 round,
        uint256 released,
        uint256 received
    );
    event Refunded(
        bytes32 indexed handleNode,
        address indexed token,
        address indexed refundTo,
        address recipient,
        uint256 round,
        uint256 released,
        uint256 received
    );

    function emitDeposited(
        bytes32 handleNode,
        address token,
        address refundTo,
        address depositor,
        uint256 round,
        uint256 amount
    ) external {
        emit Deposited(handleNode, token, refundTo, depositor, round, amount);
    }

    /// @notice A `depositToHandle` into a node nobody holds.
    function emitDepositToHandle(
        bytes32 handleNode,
        address token,
        address refundTo,
        address depositor,
        uint256 round,
        uint256 amount,
        bytes32 platformId,
        string calldata handle
    ) external {
        emit Deposited(handleNode, token, refundTo, depositor, round, amount);
        emit HandleRevealed(depositor, platformId, handleNode, handle);
    }

    function emitForwarded(
        bytes32 handleNode,
        address token,
        address depositor,
        address holder,
        uint256 amount,
        uint256 received
    ) external {
        emit Forwarded(handleNode, token, depositor, holder, amount, received);
    }

    function emitClaimed(
        bytes32 handleNode,
        address token,
        address claimer,
        address recipient,
        uint256 round,
        uint256 released,
        uint256 received
    ) external {
        emit Claimed(handleNode, token, claimer, recipient, round, released, received);
    }

    function emitRefunded(
        bytes32 handleNode,
        address token,
        address refundTo,
        address recipient,
        uint256 round,
        uint256 released,
        uint256 received
    ) external {
        emit Refunded(handleNode, token, refundTo, recipient, round, released, received);
    }
}
