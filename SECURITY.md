# Security Policy

Only the latest release is supported. Please report vulnerabilities privately through GitHub's
security advisory interface rather than opening a public issue.

Build manifests are trusted project configuration, but `union-builder` deliberately does not accept
arbitrary shell commands. Repository URLs must be credential-free HTTPS GitHub URLs and all sources
must be fixed to complete commit IDs. Release builders should run with the least privileges needed
to read source trees and write a fresh output directory.

`verify`, `stage` and `install` reject symlinks, unsafe relative paths, unlisted files and checksum
mismatches. Install roots must be real directories. Releases are copied into immutable slots and
are never overwritten; Unix activation replaces only a relative `current` symlink atomically.
SHA-256 inventories detect corruption but are not signatures: operators must obtain the release
directory from a trusted, authenticated source before verification.

The tool intentionally does not manage services, databases or secrets. Run build and verification
without elevated privileges; grant write access to the selected install root only for the final
stage/install operation.
