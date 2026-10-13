// SPDX-License-Identifier: MIT OR Apache-2.0
pragma solidity ^0.8.20;

/// @notice Test double for the indexer: the exact event surface of
///         IdentityRegistry, each behind a function that just emits it, and a
///         disclosing bind's two events in the order `bind` emits them. The
///         integration test deploys this on anvil and replays scenarios, so
///         the indexer decodes real ABI-encoded logs rather than values a
///         Rust test constructed for itself.
///
///         Event signatures are copied verbatim from
///         libid-contracts/solidity/contracts/identity/IdentityRegistry.sol;
///         the anvil suite checks every topic against the crate's bindings.
contract MockIdentityRegistry {
    event IdentityBound(
        address indexed holder,
        bytes32 indexed idNode,
        bytes32 indexed handleNode,
        bytes32 platformId,
        uint64 observedAt,
        uint16 ceremonyVersion
    );
    event CeremonyBound(
        bytes32 indexed authorizationDigest, address indexed holder, bytes32 indexed platformId, bytes clientIdentifier
    );
    event BindFeePaid(bytes32 indexed authorizationDigest, address indexed receiver, uint256 amount);
    event HandleRetired(bytes32 indexed platformId, bytes32 indexed handleNode, address indexed holder);
    event ProofVerifierConfigured(address verifier);
    event HandlePublished(
        address indexed holder, bytes32 indexed platformId, bytes32 indexed handleNode, string handle
    );
    event HandleUnpublished(address indexed holder, bytes32 indexed platformId);

    function emitIdentityBound(
        address holder,
        bytes32 idNode,
        bytes32 handleNode,
        bytes32 platformId,
        uint64 observedAt,
        uint16 ceremonyVersion
    ) external {
        emit IdentityBound(holder, idNode, handleNode, platformId, observedAt, ceremonyVersion);
    }

    /// @notice A bind whose ceremony disclosed the handle.
    function emitDisclosingBind(
        address holder,
        bytes32 idNode,
        bytes32 handleNode,
        bytes32 platformId,
        uint64 observedAt,
        uint16 ceremonyVersion,
        string calldata handle
    ) external {
        emit IdentityBound(holder, idNode, handleNode, platformId, observedAt, ceremonyVersion);
        emit HandlePublished(holder, platformId, handleNode, handle);
    }

    function emitHandlePublished(address holder, bytes32 platformId, bytes32 handleNode, string calldata handle)
        external
    {
        emit HandlePublished(holder, platformId, handleNode, handle);
    }

    function emitCeremonyBound(
        bytes32 authorizationDigest,
        address holder,
        bytes32 platformId,
        bytes calldata clientIdentifier
    ) external {
        emit CeremonyBound(authorizationDigest, holder, platformId, clientIdentifier);
    }

    function emitBindFeePaid(bytes32 authorizationDigest, address receiver, uint256 amount) external {
        emit BindFeePaid(authorizationDigest, receiver, amount);
    }

    function emitHandleRetired(bytes32 platformId, bytes32 handleNode, address holder) external {
        emit HandleRetired(platformId, handleNode, holder);
    }

    function emitProofVerifierConfigured(address verifier) external {
        emit ProofVerifierConfigured(verifier);
    }

    function emitHandleUnpublished(address holder, bytes32 platformId) external {
        emit HandleUnpublished(holder, platformId);
    }
}
