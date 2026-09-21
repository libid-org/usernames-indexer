// SPDX-License-Identifier: MIT OR Apache-2.0
pragma solidity ^0.8.20;

/// @notice Test double for the indexer: the exact event surface of
///         IdentityNames, each behind a function that just emits it. The
///         integration test deploys this on anvil and replays scenarios, so
///         the indexer decodes REAL ABI-encoded logs rather than values a
///         Rust test constructed for itself.
///
///         Event signatures are copied verbatim from
///         libid-contracts/solidity/contracts/identity/IdentityNames.sol —
///         a drifted copy here fails the integration test against the
///         bindings, which come from the published crate.
contract MockIdentityNames {
    event IdentityBound(
        address indexed owner,
        bytes32 indexed idNode,
        bytes32 indexed handleNode,
        bytes32 platformId,
        string userId,
        string handle,
        uint64 observedAt,
        bool published,
        uint16 ceremonyVersion
    );
    event CeremonyBound(
        bytes32 indexed authorizationDigest, address indexed owner, bytes32 indexed platformId, bytes clientIdentifier
    );
    event ClaimFeePaid(bytes32 indexed authorizationDigest, address indexed receiver, uint256 amount);
    event HandleRetired(bytes32 indexed platformId, bytes32 indexed handleNode, address indexed owner);
    event PlatformConfigured(bytes32 indexed platformId);
    event ProofVerifierConfigured(address verifier);
    event NameUnpublished(address indexed owner, bytes32 indexed platformId);

    function emitIdentityBound(
        address owner,
        bytes32 idNode,
        bytes32 handleNode,
        bytes32 platformId,
        string calldata userId,
        string calldata handle,
        uint64 observedAt,
        bool published,
        uint16 ceremonyVersion
    ) external {
        emit IdentityBound(
            owner, idNode, handleNode, platformId, userId, handle, observedAt, published, ceremonyVersion
        );
    }

    function emitCeremonyBound(
        bytes32 authorizationDigest,
        address owner,
        bytes32 platformId,
        bytes calldata clientIdentifier
    ) external {
        emit CeremonyBound(authorizationDigest, owner, platformId, clientIdentifier);
    }

    function emitClaimFeePaid(bytes32 authorizationDigest, address receiver, uint256 amount) external {
        emit ClaimFeePaid(authorizationDigest, receiver, amount);
    }

    function emitHandleRetired(bytes32 platformId, bytes32 handleNode, address owner) external {
        emit HandleRetired(platformId, handleNode, owner);
    }

    function emitPlatformConfigured(bytes32 platformId) external {
        emit PlatformConfigured(platformId);
    }

    function emitProofVerifierConfigured(address verifier) external {
        emit ProofVerifierConfigured(verifier);
    }

    function emitNameUnpublished(address owner, bytes32 platformId) external {
        emit NameUnpublished(owner, platformId);
    }
}
