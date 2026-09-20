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
///
///         Regenerate the bytecode embedded in tests/anvil.rs with:
///           forge build (any foundry project containing only this file),
///           then take .bytecode.object from out/MockIdentityNames.sol/MockIdentityNames.json
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
        uint32 version
    );
    event HandleRetired(bytes32 indexed platformId, bytes32 indexed handleNode, address indexed owner);
    event PlatformConfigured(bytes32 indexed platformId);
    event VerifierConfigured(
        bytes32 indexed platformId, uint32 indexed version, address verifier, uint64 maxFutureObservation
    );
    event VerifierRetired(bytes32 indexed platformId, uint32 indexed version);
    event LatestVersionChanged(bytes32 indexed platformId, uint32 indexed version);
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
        uint32 version
    ) external {
        emit IdentityBound(
            owner, idNode, handleNode, platformId, userId, handle, observedAt, published, version
        );
    }

    function emitHandleRetired(bytes32 platformId, bytes32 handleNode, address owner) external {
        emit HandleRetired(platformId, handleNode, owner);
    }

    function emitPlatformConfigured(bytes32 platformId) external {
        emit PlatformConfigured(platformId);
    }

    function emitVerifierConfigured(
        bytes32 platformId,
        uint32 version,
        address verifier,
        uint64 maxFutureObservation
    ) external {
        emit VerifierConfigured(platformId, version, verifier, maxFutureObservation);
    }

    function emitVerifierRetired(bytes32 platformId, uint32 version) external {
        emit VerifierRetired(platformId, version);
    }

    function emitLatestVersionChanged(bytes32 platformId, uint32 version) external {
        emit LatestVersionChanged(platformId, version);
    }

    function emitNameUnpublished(address owner, bytes32 platformId) external {
        emit NameUnpublished(owner, platformId);
    }
}
