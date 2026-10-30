# Public Ace Stream source review

Inventory verified on 2026-10-29 against the public `acestream` GitHub account. The review concentrates on client contracts that Outpace can implement and test. Repository inventory is not a claim that every line in every repository was audited. Public application code also does not establish complete parity with the closed engine.

## Pinned repository inventory

| Repository | Revision | Scope |
| --- | --- | --- |
| [ace-media-library-android](https://github.com/acestream/ace-media-library-android/tree/3abdeb48f14e) | `3abdeb48f14e` | Player and media-library integration |
| [ace-network-docs](https://github.com/acestream/ace-network-docs/tree/54a185dd0993) | `54a185dd0993` | DAO/network documentation; adjacent subsystem |
| [ace-network-explorer](https://github.com/acestream/ace-network-explorer/tree/2fa6f77239c3) | `2fa6f77239c3` | Stellar-derived ledger explorer; adjacent subsystem |
| [ace-network-node](https://github.com/acestream/ace-network-node/tree/2b69d8493633) | `2b69d8493633` | Stellar-derived consensus node; adjacent subsystem |
| [ace-script-extension](https://github.com/acestream/ace-script-extension/tree/ad395daee41d) | `ad395daee41d` | Browser userscript manager; entrypoint review |
| [acestream-android-sdk](https://github.com/acestream/acestream-android-sdk/tree/65a6e5399b5c) | `65a6e5399b5c` | Descriptor, HTTP request and response contracts |
| [acestream-docs](https://github.com/acestream/acestream-docs/tree/fec817567504) | `fec817567504` | Published protocol and integration documentation |
| [acestream-engine-android](https://github.com/acestream/acestream-engine-android/tree/88ddcc93ae71) | `88ddcc93ae71` | Android packaging and engine startup |
| [acestream-engine-android-core](https://github.com/acestream/acestream-engine-android-core/tree/3798163fc053) | `3798163fc053` | Client playback orchestration and controls |
| [acestream-engine-client](https://github.com/acestream/acestream-engine-client/tree/e65c982a5884) | `e65c982a5884` | Android service discovery and lifecycle |
| [acestream-media-android](https://github.com/acestream/acestream-media-android/tree/5bd2d243b567) | `5bd2d243b567` | Media application packaging |
| [acestream-tif-provider](https://github.com/acestream/acestream-tif-provider/tree/5d3e74d1f201) | `5d3e74d1f201` | TV Input Framework source integration |
| [android-service-client-example](https://github.com/acestream/android-service-client-example/tree/53fa0a65b9e9) | `53fa0a65b9e9` | Service-client usage example |
| [connect-sdk](https://github.com/acestream/connect-sdk/tree/1d27251e505d) | `1d27251e505d` | Remote-player integration |
| [discourse-multilingual](https://github.com/acestream/discourse-multilingual/tree/7b61a69df2c5) | `7b61a69df2c5` | Forum localization; adjacent subsystem |
| [discourse-translator](https://github.com/acestream/discourse-translator/tree/f123bdc22feb) | `f123bdc22feb` | Forum translation; adjacent subsystem |
| [discourse-x-multilingual-helper](https://github.com/acestream/discourse-x-multilingual-helper/tree/8ec6dc466f96) | `8ec6dc466f96` | Forum category localization; adjacent subsystem |
| [medialibrary](https://github.com/acestream/medialibrary/tree/759e34b52bb5) | `759e34b52bb5` | VLC-derived media catalogue |
| [streaming-utils](https://github.com/acestream/streaming-utils/tree/54a17d6ce4a3) | `54a17d6ce4a3` | Streaming configuration utilities |
| [webextension](https://github.com/acestream/webextension/tree/c0f3fb950762) | `c0f3fb950762` | Browser/engine integration |

The ledger, DAO and forum projects are separate systems. Their repository trees and entrypoint documentation were inspected to establish relevance; their algorithms are not streaming-engine implementations.

## Client contracts implemented in this pass

The Android SDK requests media discovery before playback using
[`EngineApi.getMediaFiles`](https://github.com/acestream/acestream-android-sdk/blob/65a6e5399b5c/src/main/java/org/acestream/sdk/controller/EngineApi.java#L88).
The application rejects an empty file list and uses its MIME when selecting an
output route. Outpace now reports `video/mp2t` and a safe display filename for
verified live sources. VOD and unobserved HLS properties remain separate.

An infohash or magnet query can now reuse the live descriptor already verified
by the local provider. The manager checks the requested hash again; the lookup
does not open a source or perform a network request. Unknown identities retain
unknown properties and an empty file list. Tests cover both HTTP aliases, both
selector forms, a mismatched provider response and absence of new sessions.

The same SDK's
[`stopSession`](https://github.com/acestream/acestream-android-sdk/blob/65a6e5399b5c/src/main/java/org/acestream/sdk/controller/EngineApi.java#L110)
adds `api_version=2` to the advertised command URL. Managed-origin controls now
accept this exact hint so the request reaches the stop handler. Unsupported
versions and duplicate parameters remain errors; the caller still needs the
specific session's private URL.

## Evidence and implementation boundaries

Two additional client contracts need separate designs rather than success-shaped
stubs:

- The pinned TIF provider requests `get_media_files` without `api_version` and
  consumes a legacy map of files. The pinned Android SDK explicitly requests
  version 2 and consumes `result.files` as an array. Outpace keeps its existing
  version-2 default; silently changing that default would break current clients.
  Supporting the unchanged TIF client needs an explicit legacy adapter or a
  negotiated route, not merely accepting another query parameter.
- The browser extension requests `get_available_players` and `open_in_player`.
  Outpace has no player registry or platform launch service. Implementing these
  calls requires actual discovery, device ownership and launch behavior; an
  empty success response would not provide that functionality.

- Client request builders and response consumers establish concrete interoperability requirements; comments and repository descriptions alone do not establish runtime behavior.
- File metadata must reflect a verified descriptor. An unresolved infohash is not evidence of a live stream, file layout or MIME type.
- A descriptor pointing to HLS does not establish whether the unobserved manifest is live or VOD, nor whether it is encrypted.
- API acceptance, successful compilation and synthetic playback tests do not prove playback of a particular public channel.
- The source references are provenance for independently implemented behavior. No application, forum, consensus or player implementation is transplanted into Outpace by this review.

See the [HTTP compatibility contract](compat-matrix.md), [source resolution](../source-resolution.md) and [retained replay contract](../timeshift.md) for the supported runtime behavior.
