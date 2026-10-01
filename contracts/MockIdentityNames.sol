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
        address indexed holder,
        bytes32 indexed idNode,
        bytes32 indexed handleNode,
        bytes32 platformId,
        string id,
        string handle,
        uint64 observedAt,
        bool published,
        uint16 ceremonyVersion
    );
    event CeremonyBound(
        bytes32 indexed authorizationDigest, address indexed holder, bytes32 indexed platformId, bytes clientIdentifier
    );
    event BindFeePaid(bytes32 indexed authorizationDigest, address indexed receiver, uint256 amount);
    event HandleRetired(bytes32 indexed platformId, bytes32 indexed handleNode, address indexed holder);
    event PlatformConfigured(bytes32 indexed platformId);
    event ProofVerifierConfigured(address verifier);
    event HandleUnpublished(address indexed holder, bytes32 indexed platformId);

    function emitIdentityBound(
        address holder,
        bytes32 idNode,
        bytes32 handleNode,
        bytes32 platformId,
        string calldata id,
        string calldata handle,
        uint64 observedAt,
        bool published,
        uint16 ceremonyVersion
    ) external {
        emit IdentityBound(
            holder, idNode, handleNode, platformId, id, handle, observedAt, published, ceremonyVersion
        );
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

    function emitPlatformConfigured(bytes32 platformId) external {
        emit PlatformConfigured(platformId);
    }

    function emitProofVerifierConfigured(address verifier) external {
        emit ProofVerifierConfigured(verifier);
    }

    function emitHandleUnpublished(address holder, bytes32 platformId) external {
        emit HandleUnpublished(holder, platformId);
    }
}
